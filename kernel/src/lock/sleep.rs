use crate::process::CPU;

use super::{spin::SpinLock, Lock};

struct Inner<T> {
    pub locked: bool,
    pub value: T,
    pub pid: u64,
}

pub struct SleepLock<T> {
    inner: SpinLock<Inner<T>>,
}

impl<T> SleepLock<T> {
    fn wakeup_token(&self) -> usize {
        self as *const _ as usize
    }

    pub fn is_held_by_current_process(&self) -> bool {
        todo!()
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
            let cpu = CPU::get_current();
            let token = self.wakeup_token();
            cpu.sleep(token, &mut inner);
        }

        let mut inner = self.inner.lock();
        inner.locked = true;
        inner.pid = todo!();
        SpinLock::unlock(inner);
    }

    unsafe fn raw_unlock(&self) {
        let mut inner = self.inner.lock();
        inner.locked = false;
        inner.pid = 0;

        let cpu = CPU::get_current();
        let token = self.wakeup_token();
        cpu.wakeup(token);

        SpinLock::unlock(inner);
    }
}