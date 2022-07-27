use core::ops::{Deref, DerefMut};

use crate::{
    config::{NCPU, ROOTDEV},
    interrupt,
    lock::{spin_c::SpinLockC, Lock, LockGuard},
    riscv::read_reg,
};

use super::{context::CPUContext, Process};

// Per-CPU state.
#[derive(Debug)]
pub struct CPU {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    state: CPUState<'static>,

    // swtch() here to enter scheduler().
    pub context: CPUContext,
}

impl CPU {
    const fn new() -> Self {
        Self {
            state: CPUState::Ready,
            context: CPUContext::zeroed(),
        }
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

#[derive(Debug)]
pub enum CPUState<'a> {
    Invalid,
    Ready,
    Starting(LockGuard<'a, SpinLockC<Process>>),
    Running(&'a SpinLockC<Process>),
    Stopping1(&'a SpinLockC<Process>),
    Stopping2(LockGuard<'a, SpinLockC<Process>>),
}

impl<'a> CPUState<'a> {
    pub const fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }

    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub const fn is_starting(&self) -> bool {
        matches!(self, Self::Starting(_))
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub const fn is_stopping(&self) -> bool {
        matches!(self, Self::Stopping1(_) | Self::Stopping2(_))
    }

    pub fn assigned_process(&self) -> Option<&'a SpinLockC<Process>> {
        match self {
            Self::Invalid => None,
            Self::Ready => None,
            Self::Starting(_) => None,
            Self::Running(process) => Some(process),
            Self::Stopping1(process) => Some(process),
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
        process: LockGuard<'a, SpinLockC<Process>>,
    ) -> Result<(), LockGuard<'a, SpinLockC<Process>>> {
        self.transition(|this| match this {
            Self::Ready => (Self::Starting(process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn complete_switch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Starting(process) => (Self::Running(Lock::get_lock_ref(&process)), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn stop1(&mut self) -> Result<LockGuard<'a, SpinLockC<Process>>, ()> {
        self.transition(|this| match this {
            Self::Running(process) => (Self::Stopping1(process), Ok(process.lock())),
            other => (other, Err(())),
        })
    }

    pub fn stop2(
        &mut self,
        process: LockGuard<'a, SpinLockC<Process>>,
    ) -> Result<(), LockGuard<'a, SpinLockC<Process>>> {
        self.transition(|this| match this {
            Self::Stopping1(_) => (Self::Stopping2(process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn end(&mut self) -> Result<LockGuard<'a, SpinLockC<Process>>, ()> {
        self.transition(|this| match this {
            Self::Stopping2(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}

pub struct CurrentCPU;

impl CurrentCPU {
    unsafe fn get_raw() -> &'static mut CPU {
        assert!(!interrupt::is_enabled());
        assert!(id() < NCPU);

        static mut CPUS: [CPU; NCPU] = [const { CPU::new() }; _];
        &mut CPUS[id()]
    }
}

impl Deref for CurrentCPU {
    type Target = CPU;

    fn deref(&self) -> &Self::Target {
        unsafe { Self::get_raw() }
    }
}

impl DerefMut for CurrentCPU {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { Self::get_raw() }
    }
}

pub fn id() -> usize {
    assert!(!interrupt::is_enabled());
    unsafe { read_reg!(tp) as usize }
}

pub fn current() -> CurrentCPU {
    CurrentCPU
}

pub fn process() -> Option<&'static SpinLockC<Process>> {
    interrupt::off(|| current().state.assigned_process())
}

pub fn transition<R>(f: impl FnOnce(&mut CPUState<'static>) -> R) -> R {
    interrupt::off(|| f(&mut current().state))
}

#[no_mangle]
pub unsafe extern "C" fn forkret() {
    static mut FIRST: bool = true;

    transition(|state| state.complete_switch().unwrap());

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
