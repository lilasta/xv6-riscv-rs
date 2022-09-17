use crate::spinlock::{SpinLock, SpinLockGuard};

#[derive(Debug)]
pub enum CPU<'a, P> {
    Invalid,
    Ready,
    Dispatching(SpinLockGuard<'a, P>),
    Running(&'a SpinLock<P>),
    Pausing,
    Preempting(SpinLockGuard<'a, P>),
}

impl<'a, P> CPU<'a, P> {
    pub const fn assigned_process(&self) -> Option<&'a SpinLock<P>> {
        match self {
            Self::Running(process) => Some(process),
            _ => None,
        }
    }

    fn transition<S, E>(&mut self, f: impl FnOnce(Self) -> (Self, Result<S, E>)) -> Result<S, E> {
        let mut tmp = Self::Invalid;
        core::mem::swap(self, &mut tmp);

        let (mut this, res) = f(tmp);
        core::mem::swap(self, &mut this);

        res
    }

    pub fn start_dispatch(
        &mut self,
        process: SpinLockGuard<'a, P>,
    ) -> Result<(), SpinLockGuard<'a, P>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Dispatching(process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn finish_dispatch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Dispatching(process) => (Self::Running(SpinLock::unlock(process)), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn pause(&mut self) -> Result<SpinLockGuard<'a, P>, ()> {
        self.transition(|this| match this {
            Self::Running(process) => (Self::Pausing, Ok(process.lock())),
            other => (other, Err(())),
        })
    }

    pub fn start_preemption(
        &mut self,
        process: SpinLockGuard<'a, P>,
    ) -> Result<(), SpinLockGuard<'a, P>> {
        self.transition(|this| match this {
            Self::Pausing => (Self::Preempting(process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn finish_preemption(&mut self) -> Result<SpinLockGuard<'a, P>, ()> {
        self.transition(|this| match this {
            Self::Preempting(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}
