use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::*},
};

use crate::{cpu, interrupt};

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
        self.is_locked() && self.cpuid.load(Acquire) == cpu::id()
    }

    pub fn lock(&self) -> SpinLockGuard<T> {
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
        self.cpuid.store(cpu::id(), Release);

        SpinLockGuard::new(self)
    }

    unsafe fn unlock_raw(&self) {
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

    pub fn unlock<'a>(guard: SpinLockGuard<'a, T>) -> &'a Self {
        let this = guard.lock;
        drop(guard);
        this
    }

    pub fn unlock_temporarily<R>(guard: &mut SpinLockGuard<T>, f: impl FnOnce() -> R) -> R {
        let lock = guard.lock;

        unsafe { core::ptr::drop_in_place(guard) };
        let ret = f();
        unsafe { core::ptr::write(guard, lock.lock()) };
        ret
    }

    pub unsafe fn get(&self) -> &T {
        &*self.value.get()
    }

    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.value.get()
    }
}

unsafe impl<T> Sync for SpinLock<T> {}

#[derive(Debug)]
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<'a, T> SpinLockGuard<'a, T> {
    const fn new(lock: &'a SpinLock<T>) -> Self {
        Self { lock }
    }
}

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        unsafe { self.lock.unlock_raw() }
    }
}
