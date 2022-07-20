pub mod sleep;
pub mod spin;
pub mod spin_c;

use core::{
    marker::Destruct,
    ops::{Deref, DerefMut},
};

pub trait Lock {
    type Target;

    unsafe fn get(&self) -> &Self::Target;
    unsafe fn get_mut(&self) -> &mut Self::Target;
    unsafe fn raw_lock(&self);
    unsafe fn raw_unlock(&self);

    fn lock(&self) -> LockGuard<Self>
    where
        Self: Sized,
    {
        unsafe { self.raw_lock() };
        LockGuard { lock: self }
    }

    fn unlock<'a>(_guard: LockGuard<'a, Self>)
    where
        Self: Sized,
        LockGuard<'a, Self>: ~const Destruct,
    {
        // drop(_guard)
    }

    fn unlock_temporarily<R>(guard: &mut LockGuard<Self>, f: impl FnOnce() -> R) -> R
    where
        Self: Sized,
    {
        let lock = guard.lock;

        unsafe { core::ptr::drop_in_place(guard) };
        let ret = f();
        unsafe { core::ptr::write(guard, lock.lock()) };
        ret
    }

    fn with<R>(&self, f: impl FnOnce(&mut Self::Target) -> R) -> R
    where
        Self: Sized,
    {
        let mut guard = self.lock();
        f(guard.deref_mut())
    }

    fn get_lock_ref<'a>(guard: &LockGuard<'a, Self>) -> &'a Self
    where
        Self: Sized,
    {
        guard.lock
    }
}

#[derive(Debug)]
pub struct LockGuard<'a, L: Lock> {
    lock: &'a L,
}

impl<'a, L: Lock> LockGuard<'a, L> {
    pub const unsafe fn new(lock: &'a L) -> Self {
        Self { lock }
    }
}

impl<'a, L: ~const Lock> const Deref for LockGuard<'a, L> {
    type Target = L::Target;

    fn deref(&self) -> &Self::Target {
        unsafe { self.lock.get() }
    }
}

impl<'a, L: ~const Lock> const DerefMut for LockGuard<'a, L> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.lock.get_mut() }
    }
}

impl<'a, L: ~const Lock> const Drop for LockGuard<'a, L> {
    fn drop(&mut self) {
        unsafe { self.lock.raw_unlock() }
    }
}
