use crate::lock::{spin::SpinLock, Lock, LockGuard};

#[derive(Debug)]
pub enum CPU<C, P: 'static> {
    Invalid,
    Ready,
    Dispatching(C, LockGuard<'static, SpinLock<P>>),
    Running(C, &'static SpinLock<P>),
    Pausing(C),
    Preempting(LockGuard<'static, SpinLock<P>>),
}

impl<C, P> CPU<C, P> {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_, _))
    }

    pub const fn assigned_process(&self) -> Option<&'static SpinLock<P>> {
        match self {
            Self::Running(_, process) => Some(process),
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
        context: C,
        process: LockGuard<'static, SpinLock<P>>,
    ) -> Result<(), LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Dispatching(context, process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn finish_dispatch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Dispatching(context, process) => {
                (Self::Running(context, Lock::unlock(process)), Ok(()))
            }
            other => (other, Err(())),
        })
    }

    pub fn pause(&mut self) -> Result<LockGuard<'static, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Running(context, process) => (Self::Pausing(context), Ok(process.lock())),
            other => (other, Err(())),
        })
    }

    pub fn start_preemption(
        &mut self,
        process: LockGuard<'static, SpinLock<P>>,
    ) -> Result<C, LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Pausing(context) => (Self::Preempting(process), Ok(context)),
            other => (other, Err(process)),
        })
    }

    pub fn finish_preemption(&mut self) -> Result<LockGuard<'static, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Preempting(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}
