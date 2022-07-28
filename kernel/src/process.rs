mod context;
mod cpu;
pub mod kernel_stack;
pub mod process;
mod scheduler;
mod table;
mod trapframe;

use core::ffi::{c_char, c_void};

use crate::config::{NCPU, ROOTDEV};
use crate::interrupt::InterruptGuard;
use crate::lock::spin::SpinLock;
use crate::lock::{Lock, LockGuard};
use crate::log::LogGuard;
use crate::process::process::ProcessContext;
use crate::process::table::ProcessMetadata;
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
use self::trapframe::TrapFrame;

type Process = self::process::Process<ProcessMetadata, ProcessContext, usize, i32>;

pub fn current() -> Option<&'static SpinLock<Process>> {
    cpu().process()
}

pub fn cpuid() -> usize {
    assert!(!interrupt::is_enabled());
    unsafe { riscv::read_reg!(tp) as usize }
}

pub fn cpu() -> InterruptGuard<&'static mut CPU<*mut CPUContext, Process, ProcessContext>> {
    InterruptGuard::with(|| unsafe {
        assert!(!interrupt::is_enabled());
        assert!(cpuid() < NCPU);

        static mut CPUS: [CPU<*mut CPUContext, Process, ProcessContext>; NCPU] =
            [const { CPU::Ready }; _];
        &mut CPUS[cpuid()]
    })
}

pub extern "C" fn forkret() {
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

    let mut process = table::table().allocate_process().unwrap();
    let metadata = ProcessMetadata::new(table::table().allocate_pid(), [0; _]);
    let mut context = ProcessContext::allocate(forkret).unwrap();
    uvminit(context.pagetable, INITCODE.as_ptr(), INITCODE.len());
    context.sz = PGSIZE;
    context.trapframe.as_mut().epc = 0;
    context.trapframe.as_mut().sp = PGSIZE as _;

    // context.name = "initcode";

    extern "C" {
        fn namei(str: *const c_char) -> *mut c_void;
    }
    context.cwd = namei(cstr!("/").as_ptr());
    process.setup(metadata, context).unwrap();
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
    let mut cpu = cpu();
    let proc_context = cpu.context().unwrap();
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
    let mut cpu = cpu();
    let proc_context = cpu.context().unwrap();
    if user_src {
        copyin(proc_context.pagetable, dst, src, len) == 0
    } else {
        core::ptr::copy(<*const u8>::from_bits(src), <*mut u8>::from_bits(dst), len);
        true
    }
}

pub fn sleep<L: Lock>(wakeup_token: usize, guard: &mut LockGuard<L>) {
    // Must acquire p->lock in order to
    // change p->state and then call sched.
    // Once we hold p->lock, we can be
    // guaranteed that we won't miss any wakeup
    // (wakeup locks p->lock),
    // so it's okay to release lk.

    let (mut process, context) = cpu().pause().unwrap();
    L::unlock_temporarily(guard, || {
        // Go to sleep.
        process.sleep(context, wakeup_token).unwrap();
        sched(process);
    })
}

pub fn wakeup(token: usize) {
    table::table().wakeup(token);
}

pub unsafe fn fork() -> Option<usize> {
    let mut cpu = cpu();
    let p = cpu.process()?;
    let current_process = p.get();
    let current_context = cpu.context().unwrap();

    let mut process_new = table::table().allocate_process()?;
    let metadata = ProcessMetadata::new(table::table().allocate_pid(), [0; _]);
    let mut context = ProcessContext::allocate(forkret).unwrap();

    if let Err(_) = current_context
        .pagetable
        .copy(&mut context.pagetable, context.sz)
    {
        return None;
    }

    context.sz = context.sz;
    *context.trapframe.as_mut() = current_context.trapframe.as_mut().clone();
    context.trapframe.as_mut().a0 = 0;

    extern "C" {
        fn filedup(f: *mut c_void) -> *mut c_void;
        fn idup(f: *mut c_void) -> *mut c_void;
    }

    for (from, to) in current_context.ofile.iter().zip(context.ofile.iter_mut()) {
        if !from.is_null() {
            *to = filedup(*from);
        }
    }
    context.cwd = idup(current_context.cwd);

    let pid = current_process.metadata().unwrap().pid;

    let parent_ptr = &mut process_new.metadata_mut().unwrap().parent as *mut *mut _
        as *mut *mut SpinLock<Process>;
    Lock::unlock_temporarily(&mut process_new, || {
        let _guard = (*table::wait_lock()).lock();
        *parent_ptr = p as *const _ as *mut _;
        //table::table().register_parent(process.pid as _, pid as _);
    });

    process_new.setup(metadata, context).unwrap();

    Some(pid as _)
}

pub unsafe fn exit(status: i32) {
    let pl = current().unwrap();
    let process = pl.get_mut();
    assert!(process.metadata().unwrap().pid != 1);

    extern "C" {
        fn fileclose(fd: *mut c_void);
        fn iput(i: *mut c_void);
    }

    {
        let mut cpu = cpu();
        let context = cpu.context().unwrap();
        for fd in context.ofile.iter_mut() {
            if !fd.is_null() {
                fileclose(*fd);
                *fd = core::ptr::null_mut();
            }
        }

        let _guard = LogGuard::new();
        iput(context.cwd);
        drop(_guard);
        context.cwd = core::ptr::null_mut();
    }

    let _guard = (*table::wait_lock()).lock();
    //table::table().remove_parent(process.pid as usize);

    let initptr = table::table()
        .iter()
        .find(|p| p.get().metadata().unwrap().pid == 1)
        .unwrap();
    for p in table::table().iter() {
        if p.get().metadata().unwrap().parent == (pl as *const _ as *mut _) {
            p.get_mut().metadata_mut().unwrap().parent = initptr as *const _ as *mut _;
            wakeup(initptr as *const _ as usize);
        }
    }

    wakeup(process.metadata().unwrap().parent as usize);

    let (mut process, _) = cpu().pause().unwrap();
    process.die(status).unwrap();
    drop(_guard);

    sched(process);
    unreachable!("zombie exit");
}

#[no_mangle]
pub extern "C" fn scheduler() {
    loop {
        unsafe { enable_interrupt() };

        for process in table::table().iter() {
            let mut process = process.lock();
            if process.is_runnable() {
                let process_context = process.run().unwrap();

                let mut cpu_context = CPUContext::zeroed();
                cpu()
                    .start_dispatch(&mut cpu_context, process, process_context)
                    .unwrap();

                let new_context_ptr = &cpu().context().unwrap().context as *const _;
                unsafe { context::switch(&mut cpu_context, new_context_ptr) };

                cpu().finish_preemption().unwrap();
            }
        }
    }
}

fn sched(mut process: LockGuard<'static, SpinLock<Process>>) {
    assert!(!interrupt::is_enabled());
    assert!(interrupt::get_depth() == 1);
    assert!(!process.is_running());

    let process_context = &mut process.context_mut().unwrap().context as *mut _;
    let cpu_context = cpu().start_preemption(process).unwrap();

    let intena = interrupt::is_enabled_before();
    unsafe { context::switch(process_context, &*cpu_context) };
    interrupt::set_enabled_before(intena);

    cpu().finish_dispatch().unwrap();
}

pub fn pause() {
    let (mut process, context) = cpu().pause().unwrap();
    process.pause(context).unwrap();
    sched(process);
}

pub unsafe fn wait(addr: Option<usize>) -> Option<usize> {
    let mut cpu = cpu();
    let current = cpu.process()?;
    let context = cpu.context()?;

    let mut _guard = (*table::wait_lock()).lock();
    loop {
        let mut havekids = false;
        for process in table::table().iter() {
            if process.get().metadata().unwrap().parent == (current as *const _ as *mut _) {
                let mut process = process.lock();
                havekids = true;

                if let Process::Zombie(_, xstate) = &*process {
                    let pid = process.metadata().unwrap().pid;
                    if let Some(addr) = addr {
                        if copyout(
                            context.pagetable,
                            addr,
                            xstate as *const _ as usize,
                            core::mem::size_of_val(xstate),
                        ) < 0
                        {
                            return None;
                        }
                    }
                    process.clear().unwrap();
                    return Some(pid as _);
                }
            }
        }

        if !havekids || current.get().metadata().unwrap().killed {
            return None;
        }

        sleep(current as *const _ as usize, &mut _guard);
    }
}

pub fn procdump() {
    todo!();
    /*
    crate::print!("\n");
    for process in table::table().iter() {
        unsafe { process.get().dump() };
    }
    */
}

#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
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
    pub killed: *mut bool,       // If non-zero, have been killed
    pub pid: *mut usize,         // Process ID

    // these are private to the process, so p->lock need not be held.
    pub kstack: *mut usize,                // Virtual address of kernel stack
    pub sz: *mut usize,                    // Size of process memory (bytes)
    pub pagetable: *mut PageTable,         // User page table
    pub trapframe: *mut TrapFrame,         // data page for trampoline.S
    pub context: *mut CPUContext,          // swtch() here to run process
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
            context: core::ptr::null_mut(),
            ofile: core::ptr::null_mut(),
            cwd: core::ptr::null_mut(),
            name: core::ptr::null_mut(),
            original: core::ptr::null_mut(),
        }
    }

    pub fn from_process(process: &SpinLock<Process>, context: &mut ProcessContext) -> Self {
        let original = process as *const _ as *mut _;
        let process = unsafe { process.get_mut() };
        let state = match process {
            process::Process::Invalid => ProcessStateGlue::Unused,
            process::Process::Unused => ProcessStateGlue::Unused,
            process::Process::Runnable(_, _) => ProcessStateGlue::Runnable,
            process::Process::Running(_) => ProcessStateGlue::Running,
            process::Process::Sleeping(_, _, _) => ProcessStateGlue::Sleeping,
            process::Process::Zombie(_, _) => ProcessStateGlue::Zombie,
        };
        Self {
            state: state,
            killed: &mut process.metadata_mut().unwrap().killed,
            pid: &mut process.metadata_mut().unwrap().pid,
            kstack: &mut context.kstack,
            sz: &mut context.sz,
            pagetable: &mut context.pagetable,
            trapframe: context.trapframe.as_ptr(),
            context: &mut context.context,
            ofile: &mut context.ofile,
            cwd: &mut context.cwd,
            name: &mut process.metadata_mut().unwrap().name,
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
        let mut cpu = cpu();
        let p = cpu.context().unwrap();
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
        let mut cpu = cpu();
        match current() {
            Some(p) => ProcessGlue::from_process(p, cpu.context().unwrap()),
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
        table::table().kill(pid as usize) as i32
    }
}
