use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering::*},
};

use crate::process::CPU;

use super::Lock;

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
    cpu: AtomicPtr<CPU>,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            cpu: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(SeqCst)
    }

    fn is_held_by_current_cpu(&self) -> bool {
        // TODO: Orderingは正しいのか?
        let cpu = self.cpu.load(SeqCst);
        self.is_locked() && cpu == CPU::get_current()
    }
}

impl<T> Lock<T> for SpinLock<T> {
    unsafe fn get(&self) -> &T {
        &*self.value.get()
    }

    unsafe fn get_mut(&self) -> &mut T {
        &mut *self.value.get()
    }

    unsafe fn raw_lock(&self) {
        let cpu = CPU::get_current();

        // disable interrupts to avoid deadlock.
        cpu.push_disabling_interrupt();

        // 1つのCPUが2度ロックすることはできない
        assert!(!self.is_held_by_current_cpu());

        // On RISC-V, sync_lock_test_and_set turns into an atomic swap:
        //   a5 = 1
        //   s1 = &lk->locked
        //   amoswap.w.aq a5, a5, (s1)
        // TODO: Orderingは正しいのか?
        while self
            .locked
            .compare_exchange(false, true, SeqCst, SeqCst)
            .is_err()
        {}

        // Tell the Rust compiler and the processor to not move loads or stores
        // past this point, to ensure that the critical section's memory
        // references happen strictly after the lock is acquired.
        // On RISC-V, this emits a fence instruction.
        // TODO: Orderingは正しいのか?
        core::sync::atomic::fence(SeqCst);

        // Record info about lock acquisition for holding() and debugging.
        // TODO: Orderingは正しいのか?
        self.cpu.store(cpu, SeqCst);
    }

    unsafe fn raw_unlock(&self) {
        // 同じCPUによってロックされているかチェック
        assert!(self.is_held_by_current_cpu());

        self.cpu.store(core::ptr::null_mut(), SeqCst);

        // Tell the C compiler and the CPU to not move loads or stores
        // past this point, to ensure that all the stores in the critical
        // section are visible to other CPUs before the lock is released,
        // and that loads in the critical section occur strictly before
        // the lock is released.
        // On RISC-V, this emits a fence instruction.
        // TODO: Orderingは正しいのか?
        core::sync::atomic::fence(SeqCst);

        // Release the lock, equivalent to lk->locked = 0.
        // This code doesn't use a C assignment, since the C standard
        // implies that an assignment might be implemented with
        // multiple store instructions.
        // On RISC-V, sync_lock_release turns into an atomic swap:
        //   s1 = &lk->locked
        //   amoswap.w zero, zero, (s1)
        self.locked.store(false, SeqCst);

        CPU::get_current().pop_disabling_interrupt();
    }
}
