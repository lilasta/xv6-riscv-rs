use core::ops::{Deref, DerefMut};

use crate::{
    config::{NCPU, ROOTDEV},
    lock::Lock,
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled, read_reg},
};

use super::{context::CPUContext, Process};

// Per-CPU state.
#[derive(Debug)]
pub struct CPU {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    pub process: *mut Process,

    // swtch() here to enter scheduler().
    pub context: CPUContext,

    // Depth of push_off() nesting.
    pub disable_interrupt_depth: usize,

    // Were interrupts enabled before push_off()?
    pub is_interrupt_enabled_before: bool,
}

impl CPU {
    const fn new() -> Self {
        Self {
            process: core::ptr::null_mut(),
            context: CPUContext::zeroed(),
            disable_interrupt_depth: 0,
            is_interrupt_enabled_before: false,
        }
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

pub struct CurrentCPU;

impl CurrentCPU {
    unsafe fn get_raw() -> &'static mut CPU {
        assert!(!is_interrupt_enabled());
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
    assert!(unsafe { !is_interrupt_enabled() });
    unsafe { read_reg!(tp) as usize }
}

pub fn current() -> CurrentCPU {
    CurrentCPU
}

pub unsafe fn process() -> &'static mut Process {
    without_interrupt(|| &mut *current().process)
}

pub fn push_disabling_interrupt() {
    // TODO: おそらく順序が大事?
    let is_enabled = unsafe { is_interrupt_enabled() };

    unsafe {
        disable_interrupt();
    }

    let mut cpu = current();

    if cpu.disable_interrupt_depth == 0 {
        cpu.is_interrupt_enabled_before = is_enabled;
    }

    cpu.disable_interrupt_depth += 1;
}

pub fn pop_disabling_interrupt() {
    assert!(
        unsafe { !is_interrupt_enabled() },
        "pop_disabling_interrupt: interruptible"
    );

    let mut cpu = current();

    assert!(
        cpu.disable_interrupt_depth > 0,
        "pop_disabling_interrupt: not pushed before"
    );

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

// Per-CPU state.
#[repr(C)]
pub struct CPUGlue {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    process: *mut *mut Process,

    // swtch() here to enter scheduler().
    context: *mut CPUContext,

    // Depth of push_off() nesting.
    disable_interrupt_depth: *mut usize,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: *mut bool,
}

impl CPUGlue {
    pub const fn from_cpu(cpu: &mut CPU) -> Self {
        Self {
            process: &mut cpu.process,
            context: &mut cpu.context,
            disable_interrupt_depth: &mut cpu.disable_interrupt_depth,
            is_interrupt_enabled_before: &mut cpu.is_interrupt_enabled_before,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn forkret() {
    static mut FIRST: bool = true;

    process().lock.raw_unlock();

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
