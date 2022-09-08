mod context;
mod cpu;
pub mod process;
mod scheduler;
mod table;
mod trapframe;

use core::ffi::{c_char, c_void};
use core::mem::MaybeUninit;

use crate::allocator::KernelAllocator;
use crate::bitmap::Bitmap;
use crate::config::{NCPU, NPROC, ROOTDEV};
use crate::interrupt::InterruptGuard;
use crate::lock::spin::SpinLock;
use crate::lock::{Lock, LockGuard};
use crate::memory_layout::{kstack, kstack_index};
use crate::process::process::ProcessContext;
use crate::riscv::{self, enable_interrupt};
use crate::vm::binding::{copyin, copyout};
use crate::vm::uvminit;
use crate::{config::NOFILE, riscv::paging::PageTable};
use crate::{cstr, interrupt};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
};

use self::context::CPUContext;
use self::cpu::CPU;
use self::process::{Process, ProcessState};
use self::trapframe::TrapFrame;

fn current() -> Option<&'static SpinLock<Process>> {
    cpu().assigned_process()
}

pub unsafe fn read_memory<T>(addr: usize) -> Option<T> {
    let process = context().unwrap();
    if addr >= process.sz || addr + core::mem::size_of::<T>() > process.sz {
        return None;
    }

    let dst = MaybeUninit::uninit();
    if copyin(
        process.pagetable,
        dst.as_ptr() as usize,
        addr,
        core::mem::size_of::<T>(),
    ) != 0
    {
        return None;
    }

    Some(dst.assume_init())
}

pub fn id() -> Option<usize> {
    Some(unsafe { current()?.get().pid as usize })
}

pub fn context() -> Option<&'static mut ProcessContext> {
    unsafe { current()?.get_mut().context() }
}

pub fn is_killed() -> Option<bool> {
    Some(unsafe { current()?.get().killed != 0 })
}

pub fn cpuid() -> usize {
    assert!(!interrupt::is_enabled());
    unsafe { riscv::read_reg!(tp) as usize }
}

fn cpu() -> InterruptGuard<&'static mut CPU<'static, *mut CPUContext, Process>> {
    InterruptGuard::with(|| unsafe {
        assert!(!interrupt::is_enabled());
        assert!(cpuid() < NCPU);

        static mut CPUS: [CPU<*mut CPUContext, Process>; NCPU] = [const { CPU::Ready }; _];
        &mut CPUS[cpuid()]
    })
}

static KSTACK_USED: SpinLock<Bitmap<NPROC>> = SpinLock::new(Bitmap::new());

pub fn initialize_kstack(pagetable: &mut PageTable) {
    for i in 0..NPROC {
        let memory = KernelAllocator::get().allocate_page().unwrap();
        let pa = memory.addr().get();
        let va = kstack(i);
        pagetable.map(va, pa, PGSIZE, PTE::R | PTE::W).unwrap();
    }
}

pub fn allocate_kstack() -> Option<usize> {
    KSTACK_USED.lock().allocate().map(kstack)
}

pub fn deallocate_kstack(addr: usize) -> Result<(), ()> {
    KSTACK_USED.lock().deallocate(kstack_index(addr))
}

extern "C" fn finish_dispatch() {
    unsafe {
        static mut FIRST: bool = true;

        cpu().finish_dispatch().unwrap();

        if FIRST {
            FIRST = false;

            extern "C" {
                fn fsinit(dev: i32);
            }

            fsinit(ROOTDEV as _);
        }

        extern "C" {
            fn usertrapret();
        }

        usertrapret();
    }
}

pub unsafe fn setup_init_process() {
    // a user program that calls exec("/init")
    // od -t xC initcode
    static INITCODE: &[u8] = &[
        0x17, 0x05, 0x00, 0x00, 0x13, 0x05, 0x45, 0x02, 0x97, 0x05, 0x00, 0x00, 0x93, 0x85, 0x35,
        0x02, 0x93, 0x08, 0x70, 0x00, 0x73, 0x00, 0x00, 0x00, 0x93, 0x08, 0x20, 0x00, 0x73, 0x00,
        0x00, 0x00, 0xef, 0xf0, 0x9f, 0xff, 0x2f, 0x69, 0x6e, 0x69, 0x74, 0x00, 0x00, 0x24, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let mut context = ProcessContext::allocate(finish_dispatch).unwrap();
    uvminit(context.pagetable, INITCODE.as_ptr(), INITCODE.len());
    context.sz = PGSIZE;

    context.trapframe.as_mut().epc = 0;
    context.trapframe.as_mut().sp = PGSIZE as _;

    // process.name = "initcode";

    extern "C" {
        fn namei(str: *const c_char) -> *mut c_void;
    }
    context.cwd = namei(cstr!("/").as_ptr());

    let mut process = table::table().allocate_process().unwrap();
    assert!(process.pid == 1);

    process.state = ProcessState::Runnable(context);
}

pub fn allocate_pagetable(trapframe: usize) -> Result<PageTable, ()> {
    let mut pagetable = PageTable::allocate()?;

    // map the trampoline code (for system call return)
    // at the highest user virtual address.
    // only the supervisor uses it, on the way
    // to/from user space, so not PTE_U.
    if pagetable
        .map(TRAMPOLINE, trampoline as usize, PGSIZE, PTE::R | PTE::X)
        .is_err()
    {
        pagetable.deallocate();
        return Err(());
    }

    // map the trapframe just below TRAMPOLINE, for trampoline.S.
    if pagetable
        .map(TRAPFRAME, trapframe, PGSIZE, PTE::R | PTE::W)
        .is_err()
    {
        pagetable.unmap(TRAMPOLINE, 1, false);
        pagetable.deallocate();
        return Err(());
    }

    Ok(pagetable)
}

pub fn free_pagetable(mut pagetable: PageTable, size: usize) {
    pagetable.unmap(TRAMPOLINE, 1, false);
    pagetable.unmap(TRAPFRAME, 1, false);
    if size > 0 {
        pagetable.unmap(0, crate::riscv::paging::pg_roundup(size) / PGSIZE, true);
    }
    pagetable.deallocate();
}

// Copy to either a user address, or kernel address,
// depending on usr_dst.
// Returns 0 on success, -1 on error.
pub unsafe fn copyout_either(user_dst: bool, dst: usize, src: usize, len: usize) -> bool {
    let proc_context = current().unwrap().get_mut().context().unwrap();
    if user_dst {
        copyout(proc_context.pagetable, dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

// Copy from either a user address, or kernel address,
// depending on usr_src.
// Returns 0 on success, -1 on error.
pub unsafe fn copyin_either(dst: usize, user_src: bool, src: usize, len: usize) -> bool {
    let proc_context = current().unwrap().get_mut().context().unwrap();
    if user_src {
        copyin(proc_context.pagetable, dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

pub fn sleep<L: Lock>(token: usize, guard: &mut LockGuard<L>) {
    // Must acquire p->lock in order to
    // change p->state and then call sched.
    // Once we hold p->lock, we can be
    // guaranteed that we won't miss any wakeup
    // (wakeup locks p->lock),
    // so it's okay to release lk.

    let mut process = cpu().pause().unwrap();
    L::unlock_temporarily(guard, || {
        // Go to sleep.
        process.state.sleep(token).unwrap();
        return_to_scheduler(process);
    })
}

pub fn wakeup(token: usize) {
    table::table().wakeup(token);
}

pub unsafe fn fork() -> Option<usize> {
    let p = current()?;
    let process = p.get_mut();
    let mut process_new = table::table().allocate_process()?;
    let mut context_new = ProcessContext::allocate(finish_dispatch).ok()?;

    let size = process.context().unwrap().sz;
    if let Err(_) = process
        .context()
        .unwrap()
        .pagetable
        .copy(&mut context_new.pagetable, size)
    {
        return None;
    }

    context_new.sz = process.context().unwrap().sz;
    *context_new.trapframe.as_mut() = process.context().unwrap().trapframe.as_ref().clone();
    context_new.trapframe.as_mut().a0 = 0;

    extern "C" {
        fn filedup(f: *mut c_void) -> *mut c_void;
        fn idup(f: *mut c_void) -> *mut c_void;
    }

    for (from, to) in process
        .context()
        .unwrap()
        .ofile
        .iter()
        .zip(context_new.ofile.iter_mut())
    {
        if !from.is_null() {
            *to = filedup(*from);
        }
    }
    context_new.cwd = idup(process.context().unwrap().cwd);

    process_new.name = process.name;

    let pid = process_new.pid;

    let parent_ptr = &mut process_new.parent as *mut *mut _ as *mut *mut SpinLock<Process>;
    Lock::unlock_temporarily(&mut process_new, || {
        let _guard = (*table::wait_lock()).lock();
        *parent_ptr = p as *const _ as *mut _;
        //table::table().register_parent(process.pid as _, pid as _);
    });

    process_new.state.setup(context_new).unwrap();

    Some(pid as _)
}

pub unsafe fn exit(status: i32) {
    let pl = current().unwrap();
    let process = pl.get_mut();
    assert!(process.pid != 1);

    let context = process.context().unwrap();
    context.release_files();

    let _guard = (*table::wait_lock()).lock();
    //table::table().remove_parent(process.pid as usize);

    let initptr = table::table().iter().find(|p| p.get().pid == 1).unwrap();
    for p in table::table().iter() {
        if p.get().parent == (pl as *const _ as *mut _) {
            p.get_mut().parent = initptr as *const _ as *mut _;
            wakeup(initptr as *const _ as usize);
        }
    }

    wakeup(process.parent as usize);

    let mut process = cpu().pause().unwrap();
    process.state.die(status).unwrap();
    drop(_guard);

    return_to_scheduler(process);
    unreachable!("zombie exit");
}

#[no_mangle]
pub extern "C" fn scheduler() {
    loop {
        unsafe { enable_interrupt() };

        for process in table::table().iter() {
            let mut process = process.lock();
            if process.state.is_runnable() {
                process.state.run().unwrap();

                let mut cpu_context = CPUContext::zeroed();
                let process_context = &process.context().unwrap().context as *const _;
                cpu().start_dispatch(&mut cpu_context, process).unwrap();

                unsafe { context::switch(&mut cpu_context, process_context) };

                cpu().finish_preemption().unwrap();
            }
        }
    }
}

fn return_to_scheduler(mut process: LockGuard<'static, SpinLock<Process>>) {
    assert!(!interrupt::is_enabled());
    assert!(interrupt::get_depth() == 1);
    assert!(!process.state.is_running());

    let process_context = &mut process.context().unwrap().context as *mut _;
    let cpu_context = cpu().start_preemption(process).unwrap();

    let intena = interrupt::is_enabled_before();
    unsafe { context::switch(process_context, &*cpu_context) };
    interrupt::set_enabled_before(intena);

    cpu().finish_dispatch().unwrap();
}

pub fn pause() {
    let mut process = cpu().pause().unwrap();
    process.state.pause().unwrap();
    return_to_scheduler(process);
}

pub unsafe fn wait(addr: Option<usize>) -> Option<usize> {
    let current = current()?;
    let mut _guard = (*table::wait_lock()).lock();
    loop {
        let mut havekids = false;
        for process in table::table().iter() {
            if process.get().parent == (current as *const _ as *mut _) {
                let mut process = process.lock();
                havekids = true;

                if let ProcessState::Zombie(_, exit_status) = process.state {
                    let pid = process.pid;
                    if let Some(addr) = addr {
                        if copyout(
                            current.get_mut().context().unwrap().pagetable,
                            addr,
                            &exit_status as *const _ as usize,
                            core::mem::size_of_val(&exit_status),
                        ) < 0
                        {
                            return None;
                        }
                    }
                    process.deallocate();
                    return Some(pid as _);
                }
            }
        }

        if !havekids || current.get().killed != 0 {
            return None;
        }

        sleep(current as *const _ as usize, &mut _guard);
    }
}

pub fn kill(pid: usize) -> bool {
    table::table().kill(pid)
}

pub fn procdump() {
    crate::print!("\n");
    for process in table::table().iter() {
        unsafe { process.get().dump() };
    }
}

#[repr(C)]
#[derive(Debug)]
pub enum ProcessStateGlue {
    Unused,
    Used,
    Sleeping,
    Runnable,
    Running,
    Zombie,
}

// Per-process state
#[repr(C)]
#[derive(Debug)]
pub struct ProcessGlue {
    // p->lock must be held when using these:
    pub state: ProcessStateGlue, // Process state
    pub killed: *mut i32,        // If non-zero, have been killed
    pub pid: *mut i32,           // Process ID

    // these are private to the process, so p->lock need not be held.
    pub kstack: *mut usize,                // Virtual address of kernel stack
    pub sz: *mut usize,                    // Size of process memory (bytes)
    pub pagetable: *mut PageTable,         // User page table
    pub trapframe: *mut TrapFrame,         // data page for trampoline.S
    pub ofile: *mut [*mut c_void; NOFILE], // Open files
    pub cwd: *mut *mut c_void,             // Current directory
    pub name: *mut [c_char; 16],           // Process name (debugging)
    pub original: *mut c_void,
}

impl ProcessGlue {
    pub fn null() -> Self {
        Self {
            state: ProcessStateGlue::Unused,
            killed: core::ptr::null_mut(),
            pid: core::ptr::null_mut(),
            kstack: core::ptr::null_mut(),
            sz: core::ptr::null_mut(),
            pagetable: core::ptr::null_mut(),
            trapframe: core::ptr::null_mut(),
            ofile: core::ptr::null_mut(),
            cwd: core::ptr::null_mut(),
            name: core::ptr::null_mut(),
            original: core::ptr::null_mut(),
        }
    }

    pub fn from_process(process: &SpinLock<Process>) -> Self {
        let original = process as *const _ as *mut _;
        let process = unsafe { process.get_mut() };
        let context = unsafe { &mut *(process.context().unwrap() as *mut ProcessContext) };

        let state = match process.state {
            ProcessState::Invalid => ProcessStateGlue::Unused,
            ProcessState::Unused => ProcessStateGlue::Unused,
            ProcessState::Used => ProcessStateGlue::Used,
            ProcessState::Sleeping(_, _) => ProcessStateGlue::Sleeping,
            ProcessState::Runnable(_) => ProcessStateGlue::Runnable,
            ProcessState::Running(_) => ProcessStateGlue::Running,
            ProcessState::Zombie(_, _) => ProcessStateGlue::Zombie,
        };

        Self {
            state,
            killed: &mut process.killed,
            pid: &mut process.pid,
            kstack: &mut context.kstack,
            sz: &mut context.sz,
            pagetable: &mut context.pagetable,
            trapframe: context.trapframe.as_ptr(),
            ofile: &mut context.ofile,
            cwd: &mut context.cwd,
            name: &mut process.name,
            original,
        }
    }
}

mod binding {
    use crate::lock::spin_c::SpinLockC;

    use super::*;

    #[no_mangle]
    unsafe extern "C" fn userinit() {
        super::setup_init_process();
    }

    #[no_mangle]
    unsafe extern "C" fn exit(status: i32) {
        super::exit(status);
    }

    #[no_mangle]
    unsafe extern "C" fn wait(addr: usize) -> i32 {
        match super::wait(if addr == 0 { None } else { Some(addr) }) {
            Some(pid) => pid as _,
            None => -1,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn growproc(n: i32) -> i32 {
        let p = current().unwrap().get_mut().context().unwrap();
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
        super::cpuid() as i32
    }

    #[no_mangle]
    unsafe extern "C" fn myproc() -> ProcessGlue {
        match current() {
            Some(p) => ProcessGlue::from_process(p),
            None => ProcessGlue::null(),
        }
    }

    #[no_mangle]
    unsafe extern "C" fn sleep(chan: usize, lock: *mut SpinLockC<()>) {
        let mut guard = LockGuard::new(&mut *lock);
        super::sleep(chan, &mut guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    extern "C" fn fork() -> i32 {
        match unsafe { super::fork() } {
            Some(pid) => pid as _,
            None => -1,
        }
    }

    #[no_mangle]
    extern "C" fn r#yield() {
        super::pause();
    }

    #[no_mangle]
    extern "C" fn procinit() {}

    #[no_mangle]
    extern "C" fn wakeup(chan: usize) {
        super::wakeup(chan);
    }

    #[no_mangle]
    extern "C" fn kill(pid: i32) -> i32 {
        super::kill(pid as usize) as i32
    }
}
