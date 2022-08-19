use crate::process;

use super::{spin::SpinLock, Lock};

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
}

impl<T> Lock for SleepLock<T> {
    type Target = T;

    unsafe fn get(&self) -> &T {
        &self.inner.get().value
    }

    unsafe fn get_mut(&self) -> &mut T {
        &mut self.inner.get_mut().value
    }

    unsafe fn raw_lock(&self) {
        let mut inner = self.inner.lock();
        while inner.locked {
            process::sleep(self.wakeup_token(), &mut inner);
        }
        inner.locked = true;
    }

    unsafe fn raw_unlock(&self) {
        let mut inner = self.inner.lock();
        inner.locked = false;

        process::wakeup(self.wakeup_token());
    }
}
