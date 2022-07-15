use core::{
    ffi::c_void,
    sync::atomic::{AtomicI32, AtomicU32, Ordering::*},
};

use crate::{
    process::{cpu, CPU},
    riscv::is_interrupt_enabled,
};

use super::Lock;

#[repr(C)]
pub struct SpinLockC {
    locked: AtomicU32,
    name: *mut c_void,
    cpuid: AtomicI32,
}

impl SpinLockC {
    pub const fn new() -> Self {
        Self {
            locked: AtomicU32::new(0),
            name: core::ptr::null_mut(),
            cpuid: AtomicI32::new(0),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Relaxed) != 0
    }

    pub fn is_held_by_current_cpu(&self) -> bool {
        assert!(unsafe { !is_interrupt_enabled() });

        self.is_locked() && self.cpuid.load(Relaxed) == cpu::id() as _
    }
}

impl Lock for SpinLockC {
    type Target = ();

    unsafe fn get(&self) -> &Self::Target {
        unimplemented!()
    }

    unsafe fn get_mut(&self) -> &mut Self::Target {
        unimplemented!()
    }

    unsafe fn raw_lock(&self) {
        // disable interrupts to avoid deadlock.
        CPU::push_disabling_interrupt();

        // 1つのCPUが2度ロックすることはできない
        assert!(!self.is_held_by_current_cpu());

        // On RISC-V, sync_lock_test_and_set turns into an atomic swap:
        //   a5 = 1
        //   s1 = &lk->locked
        //   amoswap.w.aq a5, a5, (s1)
        while self
            .locked
            .compare_exchange(0, 1, Acquire, Relaxed)
            .is_err()
        {}

        // Tell the Rust compiler and the processor to not move loads or stores
        // past this point, to ensure that the critical section's memory
        // references happen strictly after the lock is acquired.
        // On RISC-V, this emits a fence instruction.
        // TODO: Orderingは正しいのか?
        core::sync::atomic::fence(Acquire);

        // Record info about lock acquisition for holding() and debugging.
        self.cpuid.store(cpu::id() as i32, Release);
    }

    unsafe fn raw_unlock(&self) {
        // 同じCPUによってロックされているかチェック
        assert!(self.is_held_by_current_cpu());

        self.cpuid.store(-1, Release);

        // Tell the C compiler and the CPU to not move loads or stores
        // past this point, to ensure that all the stores in the critical
        // section are visible to other CPUs before the lock is released,
        // and that loads in the critical section occur strictly before
        // the lock is released.
        // On RISC-V, this emits a fence instruction.
        // TODO: Orderingは正しいのか?
        core::sync::atomic::fence(Release);

        // Release the lock, equivalent to lk->locked = 0.
        // This code doesn't use a C assignment, since the C standard
        // implies that an assignment might be implemented with
        // multiple store instructions.
        // On RISC-V, sync_lock_release turns into an atomic swap:
        //   s1 = &lk->locked
        //   amoswap.w zero, zero, (s1)
        self.locked.store(0, Release);

        CPU::pop_disabling_interrupt();
    }
}

unsafe impl Sync for SpinLockC {}
