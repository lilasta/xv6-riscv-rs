use crate::{
    config::{NCPU, ROOTDEV},
    interrupt::{self, InterruptGuard},
    lock::{spin_c::SpinLockC, Lock, LockGuard},
    riscv::read_reg,
};

use super::{context::CPUContext, Process};

#[derive(Debug)]
pub enum CPU {
    Invalid,
    Ready,
    Starting(CPUContext, LockGuard<'static, SpinLockC<Process>>),
    Running(CPUContext, &'static SpinLockC<Process>),
    Stopping1(CPUContext, &'static SpinLockC<Process>),
    Stopping2(LockGuard<'static, SpinLockC<Process>>),
}

impl CPU {
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

    pub fn assigned_process(&self) -> Option<&'static SpinLockC<Process>> {
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
        process: LockGuard<'static, SpinLockC<Process>>,
    ) -> Result<*mut CPUContext, LockGuard<'static, SpinLockC<Process>>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Starting(CPUContext::zeroed(), process), Ok(())),
            other => (other, Err(process)),
        })?;

        let Self::Starting(context, _) = self else {
            unreachable!();
        };

        Ok(context)
    }

    pub fn complete_switch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Starting(context, process) => {
                (Self::Running(context, Lock::get_lock_ref(&process)), Ok(()))
            }
            other => (other, Err(())),
        })
    }

    pub fn stop1(&mut self) -> Result<LockGuard<'static, SpinLockC<Process>>, ()> {
        self.transition(|this| match this {
            Self::Running(context, process) => {
                (Self::Stopping1(context, process), Ok(process.lock()))
            }
            other => (other, Err(())),
        })
    }

    pub fn stop2(
        &mut self,
        process: LockGuard<'static, SpinLockC<Process>>,
    ) -> Result<CPUContext, LockGuard<'static, SpinLockC<Process>>> {
        self.transition(|this| match this {
            Self::Stopping1(context, _) => (Self::Stopping2(process), Ok(context)),
            other => (other, Err(process)),
        })
    }

    pub fn end(&mut self) -> Result<LockGuard<'static, SpinLockC<Process>>, ()> {
        self.transition(|this| match this {
            Self::Stopping2(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

pub fn id() -> usize {
    assert!(!interrupt::is_enabled());
    unsafe { read_reg!(tp) as usize }
}

pub fn current() -> InterruptGuard<&'static mut CPU> {
    InterruptGuard::with(|| unsafe { current_raw() })
}

pub unsafe fn current_raw() -> &'static mut CPU {
    assert!(!interrupt::is_enabled());
    assert!(id() < NCPU);

    static mut CPUS: [CPU; NCPU] = [const { CPU::Ready }; _];
    &mut CPUS[id()]
}

#[no_mangle]
pub unsafe extern "C" fn forkret() {
    static mut FIRST: bool = true;

    current().complete_switch().unwrap();

    if FIRST {
        FIRST = false;

        extern "C" {
            fn fsinit(dev: i32);
        }

        fsinit(ROOTDEV as _);
    }

    extern "C" {
        fn usertrapret();
    }

    usertrapret();
}
