use crate::lock::{spin::SpinLock, Lock, LockGuard};

#[derive(Debug)]
pub enum CPU<CC, P: 'static, PC> {
    Invalid,
    Ready,
    Dispatching(CC, LockGuard<'static, SpinLock<P>>, PC),
    Running(CC, &'static SpinLock<P>, PC),
    Pausing(CC, &'static SpinLock<P>),
    Preempting(LockGuard<'static, SpinLock<P>>),
}

impl<CC, P, PC> CPU<CC, P, PC> {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_, _, _))
    }

    pub const fn process(&self) -> Option<&'static SpinLock<P>> {
        match self {
            Self::Running(_, process, _) => Some(process),
            _ => None,
        }
    }

    pub const fn context(&mut self) -> Option<&mut PC> {
        match self {
            Self::Dispatching(_, _, context) => Some(context),
            Self::Running(_, _, context) => Some(context),
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
        context: CC,
        process: LockGuard<'static, SpinLock<P>>,
        process_context: PC,
    ) -> Result<(), LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Dispatching(context, process, process_context), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn finish_dispatch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Dispatching(context, process, process_context) => (
                Self::Running(context, Lock::unlock(process), process_context),
                Ok(()),
            ),
            other => (other, Err(())),
        })
    }

    pub fn pause(&mut self) -> Result<(LockGuard<'static, SpinLock<P>>, PC), ()> {
        self.transition(|this| match this {
            Self::Running(context, process, process_context) => (
                Self::Pausing(context, process),
                Ok((process.lock(), process_context)),
            ),
            other => (other, Err(())),
        })
    }

    pub fn start_preemption(
        &mut self,
        process: LockGuard<'static, SpinLock<P>>,
    ) -> Result<CC, LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Pausing(context, _) => (Self::Preempting(process), Ok(context)),
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
