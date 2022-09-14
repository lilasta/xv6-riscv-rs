use core::ops::{Deref, DerefMut};

use crate::{process, spinlock::SpinLock};

#[derive(Debug)]
struct Inner<T> {
    pub locked: bool,
    pub value: T,
}

#[derive(Debug)]
pub struct SleepLock<T> {
    inner: SpinLock<Inner<T>>,
}

impl<T> SleepLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(Inner {
                locked: false,
                value,
            }),
        }
    }

    fn wakeup_token(&self) -> usize {
        self as *const _ as usize
    }

    pub fn lock(&self) -> SleepLockGuard<T> {
        let mut inner = self.inner.lock();
        while inner.locked {
            process::sleep(self.wakeup_token(), &mut inner);
        }
        inner.locked = true;

        SleepLockGuard::new(self)
    }

    unsafe fn unlock(&self) {
        let mut inner = self.inner.lock();
        inner.locked = false;

        process::wakeup(self.wakeup_token());
    }
}

#[derive(Debug)]
pub struct SleepLockGuard<'a, T> {
    lock: &'a SleepLock<T>,
}

impl<'a, T> SleepLockGuard<'a, T> {
    const fn new(lock: &'a SleepLock<T>) -> Self {
        Self { lock }
    }
}

impl<'a, T> Deref for SleepLockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &self.lock.inner.get().value }
    }
}

impl<'a, T> DerefMut for SleepLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut self.lock.inner.get_mut().value }
    }
}

impl<'a, T> Drop for SleepLockGuard<'a, T> {
    fn drop(&mut self) {
        unsafe { self.lock.unlock() }
    }
}
