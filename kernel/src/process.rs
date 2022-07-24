pub mod context;
pub mod cpu;
pub mod kernel_stack;
pub mod process;
pub mod table;
pub mod trapframe;

use core::ffi::{c_char, c_void};

use crate::lock::{Lock, LockGuard};
use crate::riscv::{enable_interrupt, is_interrupt_enabled};
use crate::vm::binding::{copyin, copyout};
use crate::{config::NOFILE, lock::spin_c::SpinLockC, riscv::paging::PageTable};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
};

use self::context::CPUContext;
use self::process::{Process, ProcessState};
use self::trapframe::TrapFrame;

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
    let proc_context = cpu::process().unwrap().get_mut();
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
    let proc_context = cpu::process().unwrap().get_mut();
    if user_src {
        copyin(proc_context.pagetable.unwrap(), dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

pub fn sleep<L: Lock>(wakeup_token: usize, guard: &mut LockGuard<L>) {
    unsafe {
        // Must acquire p->lock in order to
        // change p->state and then call sched.
        // Once we hold p->lock, we can be
        // guaranteed that we won't miss any wakeup
        // (wakeup locks p->lock),
        // so it's okay to release lk.

        let mut process = cpu::transition(|state| state.stop1()).unwrap();
        (*L::get_lock_ref(guard)).raw_unlock();

        // Go to sleep.
        process.chan = wakeup_token;
        process.state = ProcessState::Sleeping;

        sched(process);

        // Reacquire original lock.
        (*L::get_lock_ref(guard)).raw_lock();
    }
}

pub fn wakeup(token: usize) {
    extern "C" {
        fn wakeup(chan: *const c_void);
    }

    unsafe { wakeup(token as *const _) };
}

pub unsafe fn fork() -> Option<usize> {
    let process = cpu::process()?;
    let process_new = table::table().allocate_process()?;
    todo!()
}

#[no_mangle]
pub extern "C" fn scheduler() {
    let mut cpu = cpu::current();

    loop {
        unsafe { enable_interrupt() };

        for process in table::table().iter() {
            let mut process = process.lock();
            if process.state == ProcessState::Runnable {
                process.state = ProcessState::Running;

                let context_ptr = &process.context as *const _;
                cpu::transition(|state| state.start(process).unwrap());

                unsafe { context::switch(&mut cpu.context, context_ptr) };

                cpu::transition(|state| state.end()).unwrap();
            }
        }
    }
}

pub fn sched(mut process: LockGuard<'static, SpinLockC<Process>>) {
    let mut cpu = cpu::current();
    assert!(cpu.disable_interrupt_depth == 1);
    assert!(process.state != ProcessState::Running);
    assert!(unsafe { !is_interrupt_enabled() });

    let context_ptr = &mut process.context as *mut _;
    cpu::transition(|state| state.stop2(process).unwrap());

    let intena = cpu.is_interrupt_enabled_before;
    unsafe { context::switch(context_ptr, &cpu.context) };
    cpu.is_interrupt_enabled_before = intena;

    cpu::transition(|state| state.complete_switch().unwrap());
}

pub fn pause() {
    let mut process = cpu::transition(|state| state.stop1().unwrap());
    process.state = ProcessState::Runnable;
    sched(process);
}

pub fn procdump() {
    crate::print!("\n");
    for process in table::table().iter() {
        unsafe { process.get().dump() };
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
    pub fn null() -> Self {
        Self {
            lock: core::ptr::null_mut(),
            state: core::ptr::null_mut(),
            chan: core::ptr::null_mut(),
            killed: core::ptr::null_mut(),
            xstate: core::ptr::null_mut(),
            pid: core::ptr::null_mut(),
            parent: core::ptr::null_mut(),
            kstack: core::ptr::null_mut(),
            sz: core::ptr::null_mut(),
            pagetable: core::ptr::null_mut(),
            trapframe: core::ptr::null_mut(),
            context: core::ptr::null_mut(),
            ofile: core::ptr::null_mut(),
            cwd: core::ptr::null_mut(),
            name: core::ptr::null_mut(),
            original: core::ptr::null_mut(),
        }
    }
    pub fn from_process(process: &SpinLockC<Process>) -> Self {
        let lock_ptr = process as *const _ as *mut _;
        let original = process as *const _ as *mut _;
        let process = unsafe { process.get_mut() };
        Self {
            lock: lock_ptr,
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
            original,
        }
    }
}

mod binding {
    use super::*;

    #[no_mangle]
    unsafe extern "C" fn exit_glue(status: i32, wait_lock: *mut SpinLockC<()>) {
        let mut process = cpu::transition(|state| state.stop1().unwrap());
        process.xstate = status;
        process.state = ProcessState::Zombie;
        (*wait_lock).raw_unlock();
        sched(process);
        unreachable!("zombie exit");
    }

    #[no_mangle]
    unsafe extern "C" fn freeproc(p: ProcessGlue) {
        (*p.original.cast::<SpinLockC<Process>>())
            .get_mut()
            .deallocate();
    }

    #[no_mangle]
    unsafe extern "C" fn growproc(n: i32) -> i32 {
        let p = cpu::process().unwrap().get_mut();
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

    #[no_mangle]
    extern "C" fn cpuid() -> i32 {
        cpu::id() as i32
    }

    #[no_mangle]
    unsafe extern "C" fn myproc() -> ProcessGlue {
        match cpu::process() {
            Some(p) => ProcessGlue::from_process(p),
            None => ProcessGlue::null(),
        }
    }

    #[no_mangle]
    extern "C" fn push_off() {
        cpu::push_disabling_interrupt();
    }

    #[no_mangle]
    extern "C" fn pop_off() {
        cpu::pop_disabling_interrupt();
    }

    #[no_mangle]
    unsafe extern "C" fn sleep(chan: usize, lock: *mut SpinLockC<()>) {
        let mut guard = LockGuard::new(&mut *lock);
        super::sleep(chan, &mut guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    extern "C" fn r#yield() {
        super::pause();
    }

    #[no_mangle]
    extern "C" fn procinit() {
        table::table().init();
    }

    #[no_mangle]
    extern "C" fn proc(index: i32) -> ProcessGlue {
        ProcessGlue::from_process(table::table().iter().nth(index as usize).unwrap())
    }

    #[no_mangle]
    unsafe extern "C" fn allocproc() -> ProcessGlue {
        match table::table().allocate_process() {
            Some(process) => {
                let refe = Lock::get_lock_ref(&process);
                let glue = ProcessGlue::from_process(refe);
                core::mem::forget(process);
                glue
            }
            None => ProcessGlue::null(),
        }
    }

    #[no_mangle]
    extern "C" fn wakeup(chan: usize) {
        table::table().wakeup(chan);
    }

    #[no_mangle]
    extern "C" fn kill(pid: i32) -> i32 {
        table::table().kill(pid as usize) as i32
    }
}
