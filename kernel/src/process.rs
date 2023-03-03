mod process;
mod scheduler;
mod table;
mod trapframe;

use core::mem::MaybeUninit;

use crate::allocator;
use crate::bitmap::Bitmap;
use crate::config::{NCPU, NPROC, ROOTDEV};
use crate::memory_layout::{kstack, kstack_index};
use crate::process::process::ProcessContext;
use crate::riscv::enable_interrupt;
use crate::spinlock::{SpinLock, SpinLockGuard};
use crate::trap::usertrapret;
use crate::vm::PageTable;
use crate::{cpu, fs, interrupt, log};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
};

use self::process::{Process, ProcessState};

pub enum Assigned<'a> {
    None,
    Switching(SpinLockGuard<'a, Process>),
    Assigned(&'a SpinLock<Process>),
}

impl<'a> Assigned<'a> {
    pub const fn get(&self) -> Option<&'a SpinLock<Process>> {
        match self {
            Self::Assigned(process) => Some(process),
            _ => None,
        }
    }

    pub fn switch(&mut self, process: SpinLockGuard<'a, Process>) {
        *self = Self::Switching(process);
    }

    pub fn assign(&mut self) {
        let this = core::mem::replace(self, Self::None);
        match this {
            Self::Switching(process) => {
                *self = Self::Assigned(SpinLock::unlock(process));
            }
            _ => panic!(),
        }
    }

    pub fn release(&mut self) {
        *self = Self::None;
    }
}

static mut ASSIGNED: [Assigned<'static>; NCPU] = [const { Assigned::None }; _];

fn map_assigned<R>(f: impl FnOnce(&Assigned<'static>) -> R) -> R {
    interrupt::off(|| unsafe { f(&ASSIGNED[cpu::id()]) })
}

fn map_assigned_mut<R>(f: impl FnOnce(&mut Assigned<'static>) -> R) -> R {
    interrupt::off(|| unsafe { f(&mut ASSIGNED[cpu::id()]) })
}

pub fn read_memory<T>(addr: usize) -> Option<T> {
    let process = context()?;
    if addr >= process.sz || addr + core::mem::size_of::<T>() > process.sz {
        return None;
    }

    let mut dst = MaybeUninit::uninit();
    if unsafe { process.pagetable.read(&mut dst, addr).is_err() } {
        return None;
    }

    Some(unsafe { dst.assume_init() })
}

#[must_use]
pub fn write_memory<T: 'static>(addr: usize, value: T) -> bool {
    let process = context().unwrap();
    unsafe { process.pagetable.write(addr, &value).is_ok() }
}

// Copy from either a user address, or kernel address,
// depending on usr_src.
// Returns 0 on success, -1 on error.
pub unsafe fn copyin_either<T: ?Sized>(dst: &mut T, user_src: bool, src: usize) -> bool {
    let proc_context = map_assigned(Assigned::get)
        .unwrap()
        .get_mut()
        .context()
        .unwrap();
    if user_src {
        proc_context.pagetable.read(dst, src).is_ok()
    } else {
        core::ptr::copy(
            core::ptr::from_exposed_addr::<u8>(src),
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
    let proc_context = map_assigned(Assigned::get)
        .unwrap()
        .get_mut()
        .context()
        .unwrap();

    if user_dst {
        proc_context.pagetable.write(dst, src).is_ok()
    } else {
        core::ptr::copy(
            <*const T>::cast::<u8>(src),
            core::ptr::from_exposed_addr_mut::<u8>(dst),
            core::mem::size_of_val(src),
        );
        true
    }
}

pub fn context() -> Option<&'static mut ProcessContext> {
    unsafe { map_assigned(Assigned::get)?.get_mut().context() }
}

pub fn id() -> Option<usize> {
    let process = map_assigned(Assigned::get)?;
    let process = process.lock();
    Some(process.pid)
}

pub fn is_killed() -> Option<bool> {
    let process = map_assigned(Assigned::get)?;
    let process = process.lock();
    Some(process.killed)
}

pub fn set_killed() -> Option<()> {
    let process = map_assigned(Assigned::get)?;
    let mut process = process.lock();
    process.killed = true;
    Some(())
}

pub fn is_running() -> bool {
    match map_assigned(Assigned::get) {
        Some(process) => process.lock().is_running(),
        None => false,
    }
}

static KSTACK_USED: SpinLock<Bitmap<NPROC>> = SpinLock::new(Bitmap::new());

pub fn initialize_kstack(pagetable: &mut PageTable) {
    for i in 0..NPROC {
        let memory = allocator::get().lock().allocate_page().unwrap();
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

        map_assigned_mut(Assigned::assign);

        if FIRST {
            FIRST = false;
            fs::initialize(ROOTDEV);
        }

        usertrapret();
    }
}

// Load the user initcode into address 0 of pagetable,
// for the very first process.
// sz must be less than a page.
unsafe fn uvminit(pagetable: &mut PageTable, src: *const u8, size: usize) {
    assert!(size < PGSIZE);

    let mem = allocator::get().lock().allocate_page().unwrap();
    core::ptr::write_bytes(mem.as_ptr(), 0, PGSIZE);

    pagetable
        .map(
            0,
            mem.addr().get(),
            PGSIZE,
            PTE::W | PTE::R | PTE::X | PTE::U,
        )
        .unwrap();

    core::ptr::copy_nonoverlapping(src, mem.as_ptr(), size);
}

pub unsafe fn setup_init_process() {
    // a user program that calls exec("/init")
    static INITCODE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initcode"));

    let mut context = ProcessContext::allocate(finish_dispatch).unwrap();
    uvminit(&mut context.pagetable, INITCODE.as_ptr(), INITCODE.len());
    context.sz = PGSIZE;

    context.trapframe.epc = 0;
    context.trapframe.sp = PGSIZE as _;

    let log = log::start();
    context.cwd = fs::search_inode(&"/", &log).map(|inode| inode.pin());
    drop(log);

    // process.name = "initcode";

    let mut process = table::get().allocate_process().unwrap();
    assert!(process.pid == 1);

    process.state = ProcessState::Runnable(context);
}

pub fn allocate_pagetable(trapframe: usize) -> Result<PageTable, ()> {
    let mut pagetable = PageTable::allocate_in(allocator::get())?;

    // map the trampoline code (for system call return)
    // at the highest user virtual address.
    // only the supervisor uses it, on the way
    // to/from user space, so not PTE_U.
    if pagetable
        .map(TRAMPOLINE, trampoline as usize, PGSIZE, PTE::R | PTE::X)
        .is_err()
    {
        return Err(());
    }

    // map the trapframe just below TRAMPOLINE, for trampoline.S.
    if pagetable
        .map(TRAPFRAME, trapframe, PGSIZE, PTE::R | PTE::W)
        .is_err()
    {
        pagetable.unmap(TRAMPOLINE, 1, false);
        return Err(());
    }

    Ok(pagetable)
}

pub fn free_pagetable(pagetable: &mut PageTable, size: usize) {
    pagetable.unmap(TRAMPOLINE, 1, false);
    pagetable.unmap(TRAPFRAME, 1, false);
    if size > 0 {
        pagetable.unmap(0, crate::riscv::paging::pg_roundup(size) / PGSIZE, true);
    }
}

pub fn sleep<T>(token: usize, guard: &mut SpinLockGuard<'static, T>) {
    // Must acquire p->lock in order to
    // change p->state and then call sched.
    // Once we hold p->lock, we can be
    // guaranteed that we won't miss any wakeup
    // (wakeup locks p->lock),
    // so it's okay to release lk.

    let mut process = map_assigned(Assigned::get).unwrap().lock();
    SpinLock::unlock_temporarily(guard, || {
        // Go to sleep.
        process.state.sleep(token).unwrap();
        return_to_scheduler(process);
    })
}

pub fn wakeup(token: usize) {
    table::get().wakeup(token, map_assigned(Assigned::get));
}

pub unsafe fn fork() -> Option<usize> {
    let p = map_assigned(Assigned::get)?;
    let process = p.get_mut();
    let mut process_new = table::get().allocate_process()?;
    let context_new = context().unwrap().try_clone(finish_dispatch).ok()?;

    process_new.name = process.name;
    let pid = process_new.pid;
    let parent_ptr = &mut process_new.parent as *mut *mut _ as *mut *mut SpinLock<Process>;
    SpinLock::unlock_temporarily(&mut process_new, || {
        let _guard = (*table::wait_lock()).lock();
        *parent_ptr = p as *const _ as *mut _;
        //table::table().register_parent(process.pid as _, pid as _);
    });

    process_new.state.setup(context_new).unwrap();

    Some(pid)
}

pub unsafe fn exit(status: i32) {
    let pl = map_assigned(Assigned::get).unwrap();
    let process = pl.get_mut();
    assert!(process.pid != 1);

    let context = process.context().unwrap();
    for opened in context.ofile.iter_mut() {
        opened.take();
    }
    context.cwd.take();

    let _guard = (*table::wait_lock()).lock();
    //table::table().remove_parent(process.pid as usize);

    let initptr = table::get().iter().find(|p| p.get().pid == 1).unwrap();
    for p in table::get().iter() {
        if p.get().parent == (pl as *const _ as *mut _) {
            p.get_mut().parent = initptr as *const _ as *mut _;
            wakeup(initptr as *const _ as usize);
        }
    }

    wakeup(process.parent as usize);

    let mut process = map_assigned(Assigned::get).unwrap().lock();
    process.state.die(status).unwrap();
    drop(_guard);

    return_to_scheduler(process);
    unreachable!("zombie exit");
}

pub fn scheduler() {
    loop {
        unsafe { enable_interrupt() };

        for process in table::get().iter() {
            let mut process = process.lock();
            if process.state.is_runnable() {
                process.state.run().unwrap();

                let process_context = &process.context().unwrap().context as *const _;
                map_assigned_mut(|slot| slot.switch(process));

                unsafe { cpu::dispatch(&*process_context) };

                map_assigned_mut(Assigned::release);
            }
        }
    }
}

fn return_to_scheduler(mut process: SpinLockGuard<'static, Process>) {
    assert!(!interrupt::is_enabled());
    assert!(interrupt::get_depth() == 1);
    assert!(!process.state.is_running());

    let context = &mut process.context().unwrap().context as *mut _;
    map_assigned_mut(|slot| slot.switch(process));

    let intena = interrupt::is_enabled_before();
    unsafe { cpu::preemption(&mut *context) };
    interrupt::set_enabled_before(intena);

    map_assigned_mut(Assigned::assign);
}

pub fn pause() {
    let mut process = map_assigned(Assigned::get).unwrap().lock();
    process.state.pause().unwrap();
    return_to_scheduler(process);
}

pub unsafe fn wait(addr: Option<usize>) -> Option<usize> {
    let current = map_assigned(Assigned::get)?;
    let mut guard = (*table::wait_lock()).lock();
    loop {
        let mut havekids = false;
        for process in table::get().iter() {
            if process.get().parent == (current as *const _ as *mut _) {
                let mut process = process.lock();
                havekids = true;

                if let ProcessState::Zombie(_, exit_status) = process.state {
                    let pid = process.pid;
                    if let Some(addr) = addr {
                        if context()
                            .unwrap()
                            .pagetable
                            .write(addr, &exit_status)
                            .is_err()
                        {
                            return None;
                        }
                    }
                    process.deallocate();
                    return Some(pid);
                }
            }
        }

        if !havekids || current.get().killed {
            return None;
        }

        sleep(current as *const _ as usize, &mut guard);
    }
}

pub fn kill(pid: usize) -> bool {
    table::get().kill(pid)
}

pub fn procdump() {
    crate::print!("\n");
    for process in table::get().iter() {
        unsafe { process.get().dump() };
    }
}
