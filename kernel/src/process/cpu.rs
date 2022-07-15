use core::ops::{Deref, DerefMut};

use crate::{
    config::{NCPU, ROOTDEV},
    lock::{spin::SpinLock, Lock, LockGuard},
    process::context::swtch,
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled, read_reg},
};

use super::{
    context::CPUContext,
    process::{Process, ProcessContext},
    ProcessTable,
};

// Per-CPU state.
#[derive(Debug)]
pub struct CPU {
    // The process running on this cpu, or null.
    state: CPUState<'static>,

    // swtch() here to enter scheduler().
    context: CPUContext,

    // Depth of push_off() nesting.
    disable_interrupt_depth: u64,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: bool,
}

impl CPU {
    const fn new() -> Self {
        Self {
            state: CPUState::Ready,
            context: CPUContext::zeroed(),
            disable_interrupt_depth: 0,
            is_interrupt_enabled_before: false,
        }
    }

    pub fn process(&self) -> Option<&'static SpinLock<Process>> {
        match &self.state {
            CPUState::Invalid => None,
            CPUState::Ready => None,
            CPUState::Starting(process, _) => Some(Lock::ref_from_guard(process)),
            CPUState::Running(process, _) => Some(process),
            CPUState::Stopping1(process) => Some(process),
            CPUState::Stopping2(process) => Some(Lock::ref_from_guard(process)),
        }
    }

    pub fn process_context(&mut self) -> Option<&mut ProcessContext> {
        match &mut self.state {
            CPUState::Invalid => None,
            CPUState::Ready => None,
            CPUState::Starting(_, context) => Some(context),
            CPUState::Running(_, context) => Some(context),
            CPUState::Stopping1(_) => None,
            CPUState::Stopping2(_) => None,
        }
    }

    pub fn run_process(&mut self, mut process: LockGuard<'static, SpinLock<Process>>) {
        // Switch to chosen process.  It is the process's job
        // to release its lock and then reacquire it
        // before jumping back to us.
        let context = process.run().unwrap();
        self.state.start(process, context).unwrap();

        unsafe { swtch(&mut self.context, &self.process_context().unwrap().context) };

        let process = self.state.end().unwrap();
        Lock::unlock(process);
    }

    // Switch to scheduler.  Must hold only p->lock
    // and have changed proc->state. Saves and restores
    // intena because intena is a property of this
    // kernel thread, not this CPU. It should
    // be proc->intena and proc->noff, but that would
    // break in the few places where a lock is held but
    // there's no process.
    fn stop_process(&mut self, mut process: LockGuard<'static, SpinLock<Process>>) {
        static mut DUMMY_CONTEXT: CPUContext = CPUContext::zeroed();

        assert!(self.disable_interrupt_depth == 1);
        assert!(!self.state.is_running());
        //assert!(self.state.is_ready());
        //assert!(self.process().is_none());
        //assert!(self.process_context().is_none());
        assert!(unsafe { is_interrupt_enabled() == false });

        let save_at = match process.context_mut() {
            Some(context) => &mut context.context as *mut _,
            None => unsafe { &mut DUMMY_CONTEXT },
        };

        self.state.stop2(process).unwrap();

        unsafe {
            let intena = self.is_interrupt_enabled_before;
            swtch(save_at, &self.context);
            self.is_interrupt_enabled_before = intena;
        }

        self.state.complete_switch().unwrap();
    }

    // Exit the current process.  Does not return.
    // An exited process remains in the zombie state
    // until its parent calls wait().
    pub fn exit(&mut self, status: i32) {
        let process = self.process().unwrap();
        let process = ProcessTable::get().deallocate_process(process, status);

        // Jump into the scheduler, never to return.
        self.stop_process(process);

        unreachable!("zombie exit");
    }

    // Give up the CPU for one scheduling round.
    pub fn pause(&mut self) {
        let (mut process, context) = self.state.stop1().unwrap();
        process.pause(context).unwrap();
        self.stop_process(process);
    }

    pub fn sleep<L: Lock>(&mut self, token: usize, guard: &mut LockGuard<L>) {
        let (mut process, context) = self.state.stop1().unwrap();
        Lock::unlock_temporarily(guard, move || {
            process.sleep(context, token).unwrap();
            self.stop_process(process);
        });
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

#[derive(Debug)]
pub enum CPUState<'a> {
    Invalid,
    Ready,
    Starting(LockGuard<'a, SpinLock<Process>>, ProcessContext),
    Running(&'a SpinLock<Process>, ProcessContext),
    Stopping1(&'a SpinLock<Process>),
    Stopping2(LockGuard<'a, SpinLock<Process>>),
}

impl<'a> CPUState<'a> {
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
        matches!(self, Self::Stopping1(_) | Self::Stopping2(_))
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
        process: LockGuard<'a, SpinLock<Process>>,
        context: ProcessContext,
    ) -> Result<(), (LockGuard<'a, SpinLock<Process>>, ProcessContext)> {
        self.transition(|this| match this {
            Self::Ready => (Self::Starting(process, context), Ok(())),
            other => (other, Err((process, context))),
        })
    }

    pub fn complete_switch(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Starting(process, context) => (
                Self::Running(SpinLock::ref_from_guard(&process), context),
                Ok(()),
            ),
            other => (other, Err(())),
        })
    }

    pub fn stop1(&mut self) -> Result<(LockGuard<'a, SpinLock<Process>>, ProcessContext), ()> {
        self.transition(|this| match this {
            Self::Running(process, context) => {
                (Self::Stopping1(process), Ok((process.lock(), context)))
            }
            Self::Starting(process, context) => (
                Self::Stopping1(Lock::ref_from_guard(&process)),
                Ok((process, context)),
            ),
            other => (other, Err(())),
        })
    }

    pub fn stop2(
        &mut self,
        process: LockGuard<'a, SpinLock<Process>>,
    ) -> Result<(), LockGuard<'a, SpinLock<Process>>> {
        self.transition(|this| match this {
            Self::Stopping1(_) => (Self::Stopping2(process), Ok(())),
            other => (other, Err(process)),
        })
    }

    pub fn end(&mut self) -> Result<LockGuard<'a, SpinLock<Process>>, ()> {
        self.transition(|this| match this {
            Self::Stopping2(process) => (Self::Ready, Ok(process)),
            other => (other, Err(())),
        })
    }
}

pub struct CPUInterruptGuard<'a> {
    cpu: &'a mut CPU,
}

impl<'a> CPUInterruptGuard<'a> {
    fn new() -> Self {
        push_disabling_interrupt();
        Self { cpu: current() }
    }
}

impl<'a> Deref for CPUInterruptGuard<'a> {
    type Target = CPU;

    fn deref(&self) -> &Self::Target {
        self.cpu
    }
}

impl<'a> DerefMut for CPUInterruptGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.cpu
    }
}

impl<'a> Drop for CPUInterruptGuard<'a> {
    fn drop(&mut self) {
        pop_disabling_interrupt();
    }
}

pub fn id() -> usize {
    unsafe { read_reg!(tp) as usize }
}

// TODO: めっちゃ危ない
pub fn current() -> &'static mut CPU {
    assert!(unsafe { !is_interrupt_enabled() });
    assert!(id() < NCPU);

    static mut CPUS: [CPU; NCPU] = [const { CPU::new() }; _];
    unsafe { &mut CPUS[id()] }
}

/*
pub fn current() -> CPUInterruptGuard<'static> {
    CPUInterruptGuard::new()
}
*/

pub fn push_disabling_interrupt() {
    // TODO: おそらく順序が大事?
    let is_enabled = unsafe { is_interrupt_enabled() };

    unsafe { disable_interrupt() };

    let cpu = current();

    if cpu.disable_interrupt_depth == 0 {
        cpu.is_interrupt_enabled_before = is_enabled;
    }

    cpu.disable_interrupt_depth += 1;
}

pub fn pop_disabling_interrupt() {
    assert!(unsafe { !is_interrupt_enabled() },);

    let cpu = current();

    assert!(cpu.disable_interrupt_depth > 0);

    cpu.disable_interrupt_depth -= 1;

    if cpu.disable_interrupt_depth == 0 {
        if cpu.is_interrupt_enabled_before {
            unsafe { enable_interrupt() }
        }
    }
}

pub fn without_interrupt<R>(f: impl FnOnce() -> R) -> R {
    push_disabling_interrupt();
    let ret = f();
    pop_disabling_interrupt();
    ret
}

// A fork child's very first scheduling by scheduler()
// will swtch to forkret.
pub extern "C" fn forkret() {
    static mut FIRST: bool = true;

    let cpu = current();
    cpu.state.complete_switch().unwrap();

    if unsafe { FIRST } {
        // File system initialization must be run in the context of a
        // regular process (e.g., because it calls sleep), and thus cannot
        // be run from main().
        unsafe { FIRST = false };

        extern "C" {
            fn fsinit(rootdev: usize);
        }
        unsafe { fsinit(ROOTDEV) };
    }

    extern "C" {
        fn usertrapret();
    }

    unsafe { usertrapret() }
}
