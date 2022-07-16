use crate::process::cpu;

use super::{spin::SpinLock, Lock};

#[derive(Debug)]
struct Inner<T> {
    pub locked: bool,
    pub value: T,
    pub pid: u64,
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
            let cpu = cpu::current();
            let token = self.wakeup_token();
            cpu.sleep(token, &mut inner);
        }

        extern "C" {
            fn pid() -> u64;
        }

        let mut inner = self.inner.lock();
        inner.locked = true;
        inner.pid = pid();
        SpinLock::unlock(inner);
    }

    unsafe fn raw_unlock(&self) {
        let mut inner = self.inner.lock();
        inner.locked = false;
        inner.pid = 0;

        let cpu = cpu::current();
        let token = self.wakeup_token();
        cpu.wakeup(token);

        SpinLock::unlock(inner);
    }
}
