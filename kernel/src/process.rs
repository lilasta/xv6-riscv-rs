use core::ffi::c_void;

use crate::{
    context::Context,
    lock::{Lock, LockGuard},
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled},
};

// Per-CPU state.
#[repr(C)]
pub struct CPU {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    process: *mut c_void,

    // swtch() here to enter scheduler().
    context: Context,

    // Depth of push_off() nesting.
    disable_interrupt_depth: u32,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: u32,
}

impl CPU {
    // TODO: めっちゃ危ない
    pub fn get_current() -> &'static mut Self {
        extern "C" {
            fn mycpu() -> *mut CPU;
        }

        assert!(unsafe { !is_interrupt_enabled() });
        unsafe { &mut *mycpu() }
    }

    pub fn without_interrupt<R>(f: impl FnOnce() -> R) -> R {
        Self::push_disabling_interrupt();
        let ret = f();
        Self::pop_disabling_interrupt();
        ret
    }

    pub fn push_disabling_interrupt() {
        // TODO: おそらく順序が大事?
        let is_enabled = unsafe { is_interrupt_enabled() };

        unsafe {
            disable_interrupt();
        }

        let cpu = Self::get_current();

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

        let cpu = CPU::get_current();

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

    pub fn sleep<L: Lock>(&self, wakeup_token: usize, guard: &mut LockGuard<L>) {
        let lock = L::get_lock_ref(guard);
        extern "C" {
            fn sleep_binding1();
            fn sleep_binding2(chan: *const c_void);
        }
        unsafe {
            sleep_binding1();
            core::ptr::drop_in_place(guard);
            sleep_binding2(wakeup_token as *const _);
        }
        unsafe { core::ptr::write(guard, lock.lock()) };
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

pub mod cpu {
    use crate::riscv::read_reg;

    pub fn id() -> usize {
        unsafe { read_reg!(tp) as usize }
    }
}
