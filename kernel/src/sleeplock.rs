use core::ops::{Deref, DerefMut};

use crate::{process, spinlock::SpinLock};

#[derive(Debug)]
struct SleepLockInner<T> {
    locked: bool,
    value: T,
}

#[derive(Debug)]
pub struct SleepLock<T> {
    inner: SpinLock<SleepLockInner<T>>,
}

impl<T> SleepLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(SleepLockInner {
                locked: false,
                value,
            }),
        }
    }

    fn wakeup_token(&'static self) -> usize {
        core::ptr::addr_of!(*self).addr()
    }

    pub fn lock(&'static self) -> SleepLockGuard<T> {
        let mut inner = self.inner.lock();
        while inner.locked {
            process::sleep(self.wakeup_token(), &mut inner);
        }
        inner.locked = true;

        SleepLockGuard::new(self)
    }

    unsafe fn unlock(&'static self) {
        let mut inner = self.inner.lock();
        inner.locked = false;

        process::wakeup(self.wakeup_token());
    }
}

#[derive(Debug)]
pub struct SleepLockGuard<T: 'static> {
    lock: &'static SleepLock<T>,
}

impl<T> SleepLockGuard<T> {
    const fn new(lock: &'static SleepLock<T>) -> Self {
        Self { lock }
    }
}

impl<T> Deref for SleepLockGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &self.lock.inner.get().value }
    }
}

impl<T> DerefMut for SleepLockGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut self.lock.inner.get_mut().value }
    }
}

impl<T> Drop for SleepLockGuard<T> {
    fn drop(&mut self) {
        unsafe { self.lock.unlock() }
    }
}
