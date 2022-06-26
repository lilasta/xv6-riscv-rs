use core::ffi::c_void;

use crate::{
    context::Context,
    lock::LockGuard,
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
    // TODO: safeな理由を書く
    pub fn get_current() -> &'static mut Self {
        unsafe { &mut *mycpu() }
    }

    pub fn push_disabling_interrupt(&mut self) {
        // TODO: おそらく順序が大事?
        let is_enabled = unsafe { is_interrupt_enabled() };

        unsafe {
            disable_interrupt();
        }

        if self.disable_interrupt_depth == 0 {
            self.is_interrupt_enabled_before = is_enabled as u32;
        }

        self.disable_interrupt_depth += 1;
    }

    pub fn pop_disabling_interrupt(&mut self) {
        assert!(
            unsafe { !is_interrupt_enabled() },
            "pop_disabling_interrupt: interruptible"
        );
        assert!(
            self.disable_interrupt_depth > 0,
            "pop_disabling_interrupt: not pushed before"
        );

        self.disable_interrupt_depth -= 1;

        if self.disable_interrupt_depth == 0 {
            if self.is_interrupt_enabled_before == 1 {
                unsafe { enable_interrupt() }
            }
        }
    }

    pub fn sleep<T>(&self, _wakeup_token: usize, _guard: LockGuard<T>) {
        todo!();
    }

    pub fn wakeup(&self, _token: usize) {
        todo!();
    }
}

extern "C" {
    fn mycpu() -> *mut CPU;
}
