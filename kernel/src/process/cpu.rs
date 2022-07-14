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
        match self.state {
            CPUState::Running(proc, _) => Some(proc),
            _ => None,
        }
    }

    pub fn process_context(&mut self) -> Option<&mut ProcessContext> {
        match self.state {
            CPUState::Running(_, ref mut context) | CPUState::Starting(_, ref mut context) => {
                Some(context)
            }
            _ => None,
        }
    }

    pub fn run_process(&mut self, mut process: LockGuard<'static, SpinLock<Process>>) {
        // Switch to chosen process.  It is the process's job
        // to release its lock and then reacquire it
        // before jumping back to us.
        let context = process.run().unwrap();
        self.state.start(process, context).unwrap();

        unsafe { swtch(&mut self.context, &self.process_context().unwrap().context) };
    }

    // Switch to scheduler.  Must hold only p->lock
    // and have changed proc->state. Saves and restores
    // intena because intena is a property of this
    // kernel thread, not this CPU. It should
    // be proc->intena and proc->noff, but that would
    // break in the few places where a lock is held but
    // there's no process.
    fn stop_process(&mut self, mut process: LockGuard<SpinLock<Process>>) {
        static mut DUMMY_CONTEXT: CPUContext = CPUContext::zeroed();

        assert!(self.disable_interrupt_depth == 1);
        assert!(self.state.is_ready());
        assert!(self.process().is_none());
        assert!(self.process_context().is_none());
        assert!(unsafe { is_interrupt_enabled() == false });

        let context = process.context_mut();

        let save_at = match context {
            Some(context) => &mut context.context,
            None => unsafe { &mut DUMMY_CONTEXT },
        };

        unsafe {
            let intena = self.is_interrupt_enabled_before;
            swtch(save_at, &self.context);
            self.is_interrupt_enabled_before = intena;
        }
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
        let (process, context) = self.state.pause().unwrap();
        let mut process = process.lock();
        process.pause(context).unwrap();
        self.stop_process(process);
    }

    pub fn sleep<L: Lock>(&mut self, token: usize, guard: &mut LockGuard<L>) {
        let (proc, context) = self.state.pause().unwrap();
        let mut proc = proc.lock();
        Lock::unlock_temporarily(guard, move || {
            proc.sleep(context, token).unwrap();
            self.stop_process(proc);
        });
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

pub enum CPUState<'a> {
    Invalid,
    Ready,
    Starting(LockGuard<'a, SpinLock<Process>>, ProcessContext),
    Running(&'a SpinLock<Process>, ProcessContext),
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

    fn transition<S, E>(&mut self, f: impl FnOnce(Self) -> (Self, Result<S, E>)) -> Result<S, E> {
        let mut tmp = Self::Invalid;
        core::mem::swap(self, &mut tmp);

        let (mut this, res) = f(tmp);
        core::mem::swap(self, &mut this);

        res
    }

    pub fn start(
        &mut self,
        process: LockGuard<'static, SpinLock<Process>>,
        context: ProcessContext,
    ) -> Result<(), (LockGuard<'static, SpinLock<Process>>, ProcessContext)> {
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

    pub fn pause(&mut self) -> Result<(&'a SpinLock<Process>, ProcessContext), ()> {
        self.transition(|this| match this {
            Self::Running(process, context) => (Self::Ready, Ok((process, context))),
            other => (other, Err(())),
        })
    }
}

// TODO: めっちゃ危ない
pub fn current() -> &'static mut CPU {
    assert!(unsafe { !is_interrupt_enabled() });

    let cpuid = unsafe { read_reg!(tp) as usize };
    assert!(cpuid < NCPU);

    static mut CPUS: [CPU; NCPU] = [const { CPU::new() }; _];
    unsafe { &mut CPUS[cpuid] }
}

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

    assert!(cpu.disable_interrupt_depth > 0,);

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
