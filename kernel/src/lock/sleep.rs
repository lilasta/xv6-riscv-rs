use crate::process;

use super::{spin::SpinLock, Lock};

#[derive(Debug)]
struct Inner<T> {
    pub locked: bool,
    pub value: T,
    pub pid: usize,
}

#[derive(Debug)]
pub struct SleepLock<T> {
    inner: SpinLock<Inner<T>>,
}

impl<T> SleepLock<T> {
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
            let token = self.wakeup_token();
            process::sleep(token, &mut inner);
        }

        let mut inner = self.inner.lock();
        inner.locked = true;
        inner.pid = process::current().unwrap().get().metadata().unwrap().pid;
        SpinLock::unlock(inner);
    }

    unsafe fn raw_unlock(&self) {
        let mut inner = self.inner.lock();
        inner.locked = false;
        inner.pid = 0;

        let token = self.wakeup_token();
        process::wakeup(token);

        SpinLock::unlock(inner);
    }
}
