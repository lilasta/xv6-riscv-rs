use core::ffi::c_void;

use crate::{
    config::NCPU,
    context::Context,
    lock::{Lock, LockGuard},
    process::ProcessState,
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled, read_reg},
};

use super::Process;

// Per-CPU state.
#[repr(C)]
pub struct CPU {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    process: *mut Process,

    // swtch() here to enter scheduler().
    context: Context,

    // Depth of push_off() nesting.
    disable_interrupt_depth: u32,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: u32,
}

impl CPU {
    const fn new() -> Self {
        Self {
            process: core::ptr::null_mut(),
            context: Context::zeroed(),
            disable_interrupt_depth: 0,
            is_interrupt_enabled_before: 0,
        }
    }

    pub fn sleep<L: Lock>(&self, wakeup_token: usize, guard: &mut LockGuard<L>) {
        unsafe {
            let p = self.process;

            // Must acquire p->lock in order to
            // change p->state and then call sched.
            // Once we hold p->lock, we can be
            // guaranteed that we won't miss any wakeup
            // (wakeup locks p->lock),
            // so it's okay to release lk.

            let process = (*p).lock.lock();
            (*L::get_lock_ref(guard)).raw_unlock();

            // Go to sleep.
            (*p).chan = wakeup_token;
            (*p).state = ProcessState::Sleeping;

            extern "C" {
                fn sched();
            }

            sched();

            // Tidy up.
            (*p).chan = 0;

            // Reacquire original lock.
            Lock::unlock(process);
            (*L::get_lock_ref(guard)).raw_lock();
        }
    }

    pub fn wakeup(&self, token: usize) {
        extern "C" {
            fn wakeup(chan: *const c_void);
        }

        unsafe { wakeup(token as *const _) };
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

pub fn id() -> usize {
    assert!(unsafe { !is_interrupt_enabled() });
    unsafe { read_reg!(tp) as usize }
}

pub fn current() -> &'static mut CPU {
    assert!(unsafe { !is_interrupt_enabled() });
    assert!(id() < NCPU);

    static mut CPUS: [CPU; NCPU] = [const { CPU::new() }; _];
    unsafe { &mut CPUS[id()] }
}

pub unsafe fn process() -> &'static mut Process {
    without_interrupt(|| &mut *current().process)
}

pub fn push_disabling_interrupt() {
    // TODO: おそらく順序が大事?
    let is_enabled = unsafe { is_interrupt_enabled() };

    unsafe {
        disable_interrupt();
    }

    let cpu = current();

    if cpu.disable_interrupt_depth == 0 {
        cpu.is_interrupt_enabled_before = is_enabled as u32;
    }

    cpu.disable_interrupt_depth += 1;
}

pub fn pop_disabling_interrupt() {
    assert!(
        unsafe { !is_interrupt_enabled() },
        "pop_disabling_interrupt: interruptible"
    );

    let cpu = current();

    assert!(
        cpu.disable_interrupt_depth > 0,
        "pop_disabling_interrupt: not pushed before"
    );

    cpu.disable_interrupt_depth -= 1;

    if cpu.disable_interrupt_depth == 0 {
        if cpu.is_interrupt_enabled_before == 1 {
            unsafe { enable_interrupt() }
        }
    }
}

pub fn without_interrupt<R>(f: impl FnOnce() -> R) -> R {
    push_disabling_interrupt();
    let ret = f();
    pop_disabling_interrupt();
    ret
}

mod binding {
    use crate::lock::spin_c::SpinLockC;

    use super::*;

    #[no_mangle]
    extern "C" fn cpuid() -> i32 {
        id() as i32
    }

    #[no_mangle]
    extern "C" fn mycpu() -> *mut CPU {
        current()
    }

    #[no_mangle]
    extern "C" fn myproc() -> *mut Process {
        without_interrupt(|| current().process)
    }

    #[no_mangle]
    unsafe extern "C" fn pid() -> usize {
        (*myproc()).pid as usize
    }

    #[no_mangle]
    unsafe extern "C" fn sleep(chan: usize, lock: *mut SpinLockC) {
        let mut guard = LockGuard::new(&mut *lock);
        current().sleep(chan, &mut guard);
        core::mem::forget(guard);
    }
}
