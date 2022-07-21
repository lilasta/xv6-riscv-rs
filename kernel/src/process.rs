pub mod context;
pub mod cpu;
pub mod table;
pub mod trapframe;

use core::ffi::{c_char, c_void};
use core::ptr::NonNull;

use crate::allocator::KernelAllocator;
use crate::lock::Lock;
use crate::riscv::enable_interrupt;
use crate::vm::binding::{copyin, copyout};
use crate::{config::NOFILE, lock::spin_c::SpinLockC, riscv::paging::PageTable};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
};

use self::context::CPUContext;
use self::trapframe::TrapFrame;

#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
pub enum ProcessState {
    Unused,
    Used,
    Sleeping,
    Runnable,
    Running,
    Zombie,
}

// Per-process state
#[derive(Debug)]
pub struct Process {
    pub lock: SpinLockC<()>,

    // p->lock must be held when using these:
    pub state: ProcessState, // Process state
    pub chan: usize,         // If non-zero, sleeping on chan
    pub killed: i32,         // If non-zero, have been killed
    pub xstate: i32,         // Exit status to be returned to parent's wait
    pub pid: i32,            // Process ID

    // wait_lock must be held when using this:
    pub parent: *mut Process, // Parent process

    // these are private to the process, so p->lock need not be held.
    pub kstack: usize,                // Virtual address of kernel stack
    pub sz: usize,                    // Size of process memory (bytes)
    pub pagetable: Option<PageTable>, // User page table
    pub trapframe: *mut TrapFrame,    // data page for trampoline.S
    pub context: CPUContext,          // swtch() here to run process
    pub ofile: [*mut c_void; NOFILE], // Open files
    pub cwd: *mut c_void,             // Current directory
    pub name: [c_char; 16],           // Process name (debugging)
}

impl Process {
    pub const fn unused() -> Self {
        Self {
            lock: SpinLockC::new(()),
            state: ProcessState::Unused,
            chan: 0,
            killed: 0,
            xstate: 0,
            pid: 0,
            parent: core::ptr::null_mut(),
            kstack: 0,
            sz: 0,
            pagetable: None,
            trapframe: core::ptr::null_mut(),
            context: CPUContext::zeroed(),
            ofile: [core::ptr::null_mut(); _],
            cwd: core::ptr::null_mut(),
            name: [0; _],
        }
    }

    // Look in the process table for an UNUSED proc.
    // If found, initialize state required to run in the kernel,
    // and return with p->lock held.
    // If there are no free procs, or a memory allocation fails, return 0.
    pub unsafe fn allocate(&mut self) {
        extern "C" {
            fn allocpid() -> i32;
            fn forkret();
        }

        self.pid = allocpid();
        self.state = ProcessState::Used;

        // Allocate a trapframe page.
        match KernelAllocator::get().allocate() {
            Some(trapframe) => self.trapframe = trapframe.as_ptr(),
            None => {
                self.deallocate();
                return;
            }
        }

        // An empty user page table.
        match allocate_pagetable(self.trapframe.addr()) {
            Ok(pagetable) => self.pagetable = Some(pagetable),
            Err(_) => {
                self.deallocate();
                return;
            }
        }

        // Set up new context to start executing at forkret,
        // which returns to user space.
        self.context = CPUContext::zeroed();
        self.context.ra = forkret as u64;
        self.context.sp = (self.kstack + PGSIZE) as u64;
    }

    // free a proc structure and the data hanging from it,
    // including user pages.
    // p->lock must be held.
    pub unsafe fn deallocate(&mut self) {
        if !self.trapframe.is_null() {
            KernelAllocator::get().deallocate(NonNull::new_unchecked(self.trapframe));
            self.trapframe = core::ptr::null_mut();
        }

        if let Some(pagetable) = &self.pagetable {
            free_pagetable(*pagetable, self.sz);
        }

        self.sz = 0;
        self.pid = 0;
        self.parent = core::ptr::null_mut();
        self.name[0] = 0;
        self.chan = 0;
        self.killed = 0;
        self.xstate = 0;
        self.state = ProcessState::Unused;
    }

    pub fn resize_memory(&mut self, n: isize) -> Result<(), ()> {
        if n == 0 {
            return Ok(());
        }

        let old_size = self.sz;
        let new_size = self.sz.wrapping_add_signed(n);
        if n > 0 {
            self.sz = self.pagetable.unwrap().grow(old_size, new_size)?;
        } else {
            self.sz = self.pagetable.unwrap().shrink(old_size, new_size)?;
        }
        return Ok(());
    }

    pub fn dump(&self) {
        if self.state == ProcessState::Unused {
            return;
        }

        crate::println!("{} {:?} {:?}", self.pid, self.state, self.name);
    }
}

pub fn allocate_pagetable(trapframe: usize) -> Result<PageTable, ()> {
    let mut pagetable = PageTable::allocate()?;
    extern "C" {
        fn uvmfree(pt: PageTable, size: usize);
    }

    // map the trampoline code (for system call return)
    // at the highest user virtual address.
    // only the supervisor uses it, on the way
    // to/from user space, so not PTE_U.
    if pagetable
        .map(TRAMPOLINE, trampoline as usize, PGSIZE, PTE::R | PTE::X)
        .is_err()
    {
        unsafe {
            uvmfree(pagetable, 0);
        }
        return Err(());
    }

    // map the trapframe just below TRAMPOLINE, for trampoline.S.
    if pagetable
        .map(TRAPFRAME, trapframe, PGSIZE, PTE::R | PTE::W)
        .is_err()
    {
        pagetable.unmap(TRAMPOLINE, 1, false);
        unsafe {
            uvmfree(pagetable, 0);
        }
        return Err(());
    }

    Ok(pagetable)
}

pub fn free_pagetable(mut pagetable: PageTable, size: usize) {
    extern "C" {
        fn uvmfree(pt: PageTable, size: usize);
    }

    pagetable.unmap(TRAMPOLINE, 1, false);
    pagetable.unmap(TRAPFRAME, 1, false);
    unsafe { uvmfree(pagetable, size) };
}

// Copy to either a user address, or kernel address,
// depending on usr_dst.
// Returns 0 on success, -1 on error.
unsafe fn copyout_either(user_dst: bool, dst: usize, src: usize, len: usize) -> bool {
    let proc_context = cpu::process();
    if user_dst {
        copyout(proc_context.pagetable.unwrap(), dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

// Copy from either a user address, or kernel address,
// depending on usr_src.
// Returns 0 on success, -1 on error.
unsafe fn copyin_either(dst: usize, user_src: bool, src: usize, len: usize) -> bool {
    let proc_context = cpu::process();
    if user_src {
        copyin(proc_context.pagetable.unwrap(), dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

#[no_mangle]
pub extern "C" fn scheduler() {
    let cpu = cpu::current();
    cpu.process = core::ptr::null_mut();

    loop {
        unsafe { enable_interrupt() };

        for process in table::table().iter_mut() {
            unsafe { process.lock.raw_lock() };
            if process.state == ProcessState::Runnable {
                process.state = ProcessState::Running;
                cpu.process = process;

                unsafe { context::swtch(&mut cpu.context, &process.context) };

                cpu.process = core::ptr::null_mut();
            }
            unsafe { process.lock.raw_unlock() };
        }
    }
}

pub fn procdump() {
    crate::print!("\n");
    for process in table::table().iter() {
        process.dump();
    }
}

// Per-process state
#[repr(C)]
#[derive(Debug)]
pub struct ProcessGlue {
    pub lock: *mut SpinLockC<()>,

    // p->lock must be held when using these:
    pub state: *mut ProcessState, // Process state
    pub chan: *mut usize,         // If non-zero, sleeping on chan
    pub killed: *mut i32,         // If non-zero, have been killed
    pub xstate: *mut i32,         // Exit status to be returned to parent's wait
    pub pid: *mut i32,            // Process ID

    // wait_lock must be held when using this:
    pub parent: *mut *mut Process, // Parent process

    // these are private to the process, so p->lock need not be held.
    pub kstack: *mut usize,                // Virtual address of kernel stack
    pub sz: *mut usize,                    // Size of process memory (bytes)
    pub pagetable: *mut Option<PageTable>, // User page table
    pub trapframe: *mut *mut TrapFrame,    // data page for trampoline.S
    pub context: *mut CPUContext,          // swtch() here to run process
    pub ofile: *mut [*mut c_void; NOFILE], // Open files
    pub cwd: *mut *mut c_void,             // Current directory
    pub name: *mut [c_char; 16],           // Process name (debugging)
    pub original: *mut c_void,
}

impl ProcessGlue {
    pub fn from_process(process: &mut Process) -> Self {
        Self {
            lock: &mut process.lock,
            state: &mut process.state,
            chan: &mut process.chan,
            killed: &mut process.killed,
            xstate: &mut process.xstate,
            pid: &mut process.pid,
            parent: &mut process.parent,
            kstack: &mut process.kstack,
            sz: &mut process.sz,
            pagetable: (&mut process.pagetable as *mut Option<_>).cast(),
            trapframe: &mut process.trapframe,
            context: &mut process.context,
            ofile: &mut process.ofile,
            cwd: &mut process.cwd,
            name: &mut process.name,
            original: (process as *mut Process).cast(),
        }
    }
}

mod binding {
    use super::*;

    #[no_mangle]
    unsafe extern "C" fn freeproc(p: ProcessGlue) {
        (*p.original.cast::<Process>()).deallocate();
    }

    #[no_mangle]
    extern "C" fn proc_pagetable(trapframe: usize) -> u64 {
        match allocate_pagetable(trapframe) {
            Ok(pt) => pt.as_u64(),
            Err(_) => 0,
        }
    }

    #[no_mangle]
    extern "C" fn proc_freepagetable(pagetable: PageTable, size: usize) {
        free_pagetable(pagetable, size)
    }

    #[no_mangle]
    unsafe extern "C" fn growproc(n: i32) -> i32 {
        let p = cpu::process();
        match p.resize_memory(n as _) {
            Ok(_) => 0,
            Err(_) => -1,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn either_copyout(user_dst: i32, dst: usize, src: usize, len: usize) -> i32 {
        match copyout_either(user_dst != 0, dst, src, len) {
            true => 0,
            false => -1,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn either_copyin(dst: usize, user_src: i32, src: usize, len: usize) -> i32 {
        match copyin_either(dst, user_src != 0, src, len) {
            true => 0,
            false => -1,
        }
    }
}
