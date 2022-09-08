use crate::lock::{spin::SpinLock, Lock, LockGuard};

#[derive(Debug)]
pub enum CPU<'a, C, P> {
    Invalid,
    Ready,
    Dispatching(C, LockGuard<'a, SpinLock<P>>),
    Running(C, &'a SpinLock<P>),
    Pausing(C),
    Preempting(LockGuard<'a, SpinLock<P>>),
}

impl<'a, C, P> CPU<'a, C, P> {
    pub const fn assigned_process(&self) -> Option<&'a SpinLock<P>> {
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
        process: LockGuard<'a, SpinLock<P>>,
    ) -> Result<(), LockGuard<'a, SpinLock<P>>> {
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

    pub fn pause(&mut self) -> Result<LockGuard<'a, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Running(context, process) => (Self::Pausing(context), Ok(process.lock())),
            other => (other, Err(())),
        })
    }

    pub fn start_preemption(
        &mut self,
        process: LockGuard<'a, SpinLock<P>>,
    ) -> Result<C, LockGuard<'a, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Pausing(context) => (Self::Preempting(process), Ok(context)),
            other => (other, Err(process)),
        })
    }

    pub fn finish_preemption(&mut self) -> Result<LockGuard<'a, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Preempting(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}
