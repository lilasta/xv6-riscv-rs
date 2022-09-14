mod context;
mod cpu;
pub mod process;
mod scheduler;
mod table;
mod trapframe;

use core::mem::MaybeUninit;

use crate::allocator::KernelAllocator;
use crate::bitmap::Bitmap;
use crate::config::{NCPU, NPROC, ROOTDEV};
use crate::interrupt::InterruptGuard;
use crate::memory_layout::{kstack, kstack_index};
use crate::process::process::ProcessContext;
use crate::riscv::paging::PageTable;
use crate::riscv::{self, enable_interrupt};
use crate::spinlock::{SpinLock, SpinLockGuard};
use crate::vm::{uvminit, PageTableExtension};
use crate::{fs, interrupt};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
};

use self::context::CPUContext;
use self::cpu::CPU;
use self::process::{Process, ProcessState};

fn current() -> Option<&'static SpinLock<Process>> {
    cpu().assigned_process()
}

pub fn read_memory<T>(addr: usize) -> Option<T> {
    let process = context().unwrap();
    if addr >= process.sz || addr + core::mem::size_of::<T>() > process.sz {
        return None;
    }

    let mut dst = MaybeUninit::uninit();
    if unsafe { process.pagetable.read(&mut dst, addr).is_err() } {
        return None;
    }

    Some(unsafe { dst.assume_init() })
}

// Copy from either a user address, or kernel address,
// depending on usr_src.
// Returns 0 on success, -1 on error.
pub unsafe fn copyin_either<T: ?Sized>(dst: &mut T, user_src: bool, src: usize) -> bool {
    let proc_context = current().unwrap().get_mut().context().unwrap();
    if user_src {
        proc_context.pagetable.read(dst, src).is_ok()
    } else {
        core::ptr::copy(
            <*const u8>::from_bits(src),
            <*mut T>::cast::<u8>(dst),
            core::mem::size_of_val(dst),
        );
        true
    }
}

// Copy to either a user address, or kernel address,
// depending on usr_dst.
// Returns 0 on success, -1 on error.
pub unsafe fn copyout_either<T: ?Sized>(user_dst: bool, dst: usize, src: &T) -> bool {
    let proc_context = current().unwrap().get_mut().context().unwrap();
    if user_dst {
        proc_context.pagetable.write(dst, src).is_ok()
    } else {
        core::ptr::copy(
            <*const T>::cast::<u8>(src),
            <*mut u8>::from_bits(dst),
            core::mem::size_of_val(src),
        );
        true
    }
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

pub fn set_killed() -> Option<()> {
    unsafe { current()?.get_mut().killed = 1 };
    Some(())
}

pub fn is_running() -> bool {
    let Some(process) = current() else {
        return false;
    };

    unsafe { process.get().is_running() }
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
    static INITCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initcode"));

    let mut context = ProcessContext::allocate(finish_dispatch).unwrap();
    uvminit(context.pagetable, INITCODE.as_ptr(), INITCODE.len());
    context.sz = PGSIZE;

    context.trapframe.as_mut().epc = 0;
    context.trapframe.as_mut().sp = PGSIZE as _;
    context.cwd = fs::search_inode(&"/");

    // process.name = "initcode";

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

pub fn sleep<T>(token: usize, guard: &mut SpinLockGuard<T>) {
    // Must acquire p->lock in order to
    // change p->state and then call sched.
    // Once we hold p->lock, we can be
    // guaranteed that we won't miss any wakeup
    // (wakeup locks p->lock),
    // so it's okay to release lk.

    let mut process = cpu().pause().unwrap();
    SpinLock::unlock_temporarily(guard, || {
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

    for (i, opened) in process.context().unwrap().ofile.iter().enumerate() {
        context_new.ofile[i] = opened.clone();
    }
    context_new.cwd = Some(process.context().unwrap().cwd.as_ref().cloned().unwrap());

    process_new.name = process.name;

    let pid = process_new.pid;

    let parent_ptr = &mut process_new.parent as *mut *mut _ as *mut *mut SpinLock<Process>;
    SpinLock::unlock_temporarily(&mut process_new, || {
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
    for opened in context.ofile.iter_mut() {
        opened.take();
    }
    context.cwd.take();

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

fn return_to_scheduler(mut process: SpinLockGuard<'static, Process>) {
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
                        if current
                            .get_mut()
                            .context()
                            .unwrap()
                            .pagetable
                            .write(addr, &exit_status)
                            .is_err()
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
