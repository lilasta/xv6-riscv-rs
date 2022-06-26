pub mod sleep;
pub mod spin;

use core::ops::{Deref, DerefMut};

pub trait Lock<T> {
    unsafe fn get(&self) -> &T;
    unsafe fn get_mut(&self) -> &mut T;
    unsafe fn raw_lock(&self);
    unsafe fn raw_unlock(&self);

    fn lock(&self) -> LockGuard<T>
    where
        Self: Sized,
    {
        unsafe { self.raw_lock() };
        LockGuard { lock: self }
    }

    fn unlock(guard: LockGuard<T>)
    where
        Self: Sized,
    {
        drop(guard);
    }
}

pub struct LockGuard<'a, T> {
    lock: &'a dyn Lock<T>,
}

impl<'a, T> LockGuard<'a, T> {
    const fn new(lock: &'a dyn Lock<T>) -> Self {
        Self { lock }
    }
}

impl<'a, T> Deref for LockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.lock.get() }
    }
}

impl<'a, T> DerefMut for LockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.lock.get_mut() }
    }
}

impl<'a, T> Drop for LockGuard<'a, T> {
    fn drop(&mut self) {
        unsafe { self.lock.raw_unlock() }
    }
}
