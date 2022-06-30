pub mod sleep;
pub mod spin;

use core::{
    marker::Destruct,
    ops::{Deref, DerefMut},
};

#[const_trait]
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

    fn get_lock_ref<'a>(guard: &LockGuard<'a, Self>) -> &'a Self
    where
        Self: Sized,
    {
        guard.lock
    }
}

pub struct LockGuard<'a, L: Lock> {
    lock: &'a L,
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
