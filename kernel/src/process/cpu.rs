use crate::lock::{spin::SpinLock, Lock, LockGuard};

#[derive(Debug)]
pub enum CPU<C, P: 'static> {
    Invalid,
    Ready,
    Starting(C, LockGuard<'static, SpinLock<P>>),
    Running(C, &'static SpinLock<P>),
    Stopping1(C, &'static SpinLock<P>),
    Stopping2(LockGuard<'static, SpinLock<P>>),
}

impl<C, P> CPU<C, P> {
    pub const fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }

    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn is_starting(&self) -> bool {
        matches!(self, Self::Starting(_, _))
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_, _))
    }

    pub const fn is_stopping(&self) -> bool {
        matches!(self, Self::Stopping1(_, _) | Self::Stopping2(_))
    }

    pub const fn assigned_process(&self) -> Option<&'static SpinLock<P>> {
        match self {
            Self::Invalid => None,
            Self::Ready => None,
            Self::Starting(_, _) => None,
            Self::Running(_, process) => Some(process),
            Self::Stopping1(_, process) => Some(process),
            Self::Stopping2(_) => None,
        }
    }

    fn transition<S, E>(&mut self, f: impl FnOnce(Self) -> (Self, Result<S, E>)) -> Result<S, E> {
        let mut tmp = Self::Invalid;
        core::mem::swap(self, &mut tmp);

        let (mut this, res) = f(tmp);
        core::mem::swap(self, &mut this);

        res
    }

    pub fn start(
        &mut self,
        context: C,
        process: LockGuard<'static, SpinLock<P>>,
    ) -> Result<(), LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Starting(context, process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn complete_switch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Starting(context, process) => {
                (Self::Running(context, Lock::get_lock_ref(&process)), Ok(()))
            }
            other => (other, Err(())),
        })
    }

    pub fn stop1(&mut self) -> Result<LockGuard<'static, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Running(context, process) => {
                (Self::Stopping1(context, process), Ok(process.lock()))
            }
            other => (other, Err(())),
        })
    }

    pub fn stop2(
        &mut self,
        process: LockGuard<'static, SpinLock<P>>,
    ) -> Result<C, LockGuard<'static, SpinLock<P>>> {
        self.transition(|this| match this {
            Self::Stopping1(context, _) => (Self::Stopping2(process), Ok(context)),
            other => (other, Err(process)),
        })
    }

    pub fn end(&mut self) -> Result<LockGuard<'static, SpinLock<P>>, ()> {
        self.transition(|this| match this {
            Self::Stopping2(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}
