use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::*},
};

use crate::{interrupt, process};

use super::Lock;

#[derive(Debug)]
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
    cpuid: AtomicUsize,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            cpuid: AtomicUsize::new(usize::MAX),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Acquire)
    }

    pub fn is_held_by_current_cpu(&self) -> bool {
        assert!(!interrupt::is_enabled());

        // TODO: Orderingは正しいのか?
        self.is_locked() && self.cpuid.load(Acquire) == process::cpuid()
    }
}

impl<T> Lock for SpinLock<T> {
    type Target = T;

    unsafe fn get(&self) -> &T {
        &*self.value.get()
    }

    unsafe fn get_mut(&self) -> &mut T {
        &mut *self.value.get()
    }

    unsafe fn raw_lock(&self) {
        // disable interrupts to avoid deadlock.
        interrupt::push_off();

        // 1つのCPUが2度ロックすることはできない
        assert!(!self.is_held_by_current_cpu());

        // On RISC-V, sync_lock_test_and_set turns into an atomic swap:
        //   a5 = 1
        //   s1 = &lk->locked
        //   amoswap.w.aq a5, a5, (s1)
        while self
            .locked
            .compare_exchange(false, true, Acquire, Relaxed)
            .is_err()
        {}

        // Tell the Rust compiler and the processor to not move loads or stores
        // past this point, to ensure that the critical section's memory
        // references happen strictly after the lock is acquired.
        // On RISC-V, this emits a fence instruction.
        // TODO: Orderingは正しいのか?
        core::sync::atomic::fence(Acquire);

        // Record info about lock acquisition for holding() and debugging.
        self.cpuid.store(process::cpuid(), Release);
    }

    unsafe fn raw_unlock(&self) {
        // 同じCPUによってロックされているかチェック
        assert!(self.is_held_by_current_cpu());

        self.cpuid.store(usize::MAX, Release);

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
        self.locked.store(false, Release);

        interrupt::pop_off();
    }
}

unsafe impl<T> Sync for SpinLock<T> {}
