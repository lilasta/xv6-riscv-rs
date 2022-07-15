pub mod trapframe;

use core::ffi::{c_char, c_void};

use crate::{
    config::NOFILE,
    context::Context,
    lock::{spin_c::SpinLockC, Lock, LockGuard},
    riscv::{disable_interrupt, enable_interrupt, is_interrupt_enabled, paging::PageTable},
};

use self::trapframe::TrapFrame;

// Per-CPU state.
#[repr(C)]
pub struct CPU {
    // The process running on this cpu, or null.
    // TODO: *mut Process
    process: *mut c_void,

    // swtch() here to enter scheduler().
    context: Context,

    // Depth of push_off() nesting.
    disable_interrupt_depth: u32,

    // Were interrupts enabled before push_off()?
    is_interrupt_enabled_before: u32,
}

impl CPU {
    // TODO: めっちゃ危ない
    pub fn get_current() -> &'static mut Self {
        extern "C" {
            fn mycpu() -> *mut CPU;
        }

        assert!(unsafe { !is_interrupt_enabled() });
        unsafe { &mut *mycpu() }
    }

    pub fn without_interrupt<R>(f: impl FnOnce() -> R) -> R {
        Self::push_disabling_interrupt();
        let ret = f();
        Self::pop_disabling_interrupt();
        ret
    }

    pub fn push_disabling_interrupt() {
        // TODO: おそらく順序が大事?
        let is_enabled = unsafe { is_interrupt_enabled() };

        unsafe {
            disable_interrupt();
        }

        let cpu = Self::get_current();

        if cpu.disable_interrupt_depth == 0 {
            cpu.is_interrupt_enabled_before = is_enabled as u32;
        }

        cpu.disable_interrupt_depth += 1;
    }

    pub fn pop_disabling_interrupt() {
        assert!(
            unsafe { !is_interrupt_enabled() },
            "pop_disabling_interrupt: interruptible"
        );

        let cpu = CPU::get_current();

        assert!(
            cpu.disable_interrupt_depth > 0,
            "pop_disabling_interrupt: not pushed before"
        );

        cpu.disable_interrupt_depth -= 1;

        if cpu.disable_interrupt_depth == 0 {
            if cpu.is_interrupt_enabled_before == 1 {
                unsafe { enable_interrupt() }
            }
        }
    }

    pub fn sleep<L: Lock>(&self, wakeup_token: usize, guard: &mut LockGuard<L>) {
        let lock = L::get_lock_ref(guard);
        extern "C" {
            fn sleep_binding1();
            fn sleep_binding2(chan: *const c_void);
        }
        unsafe {
            sleep_binding1();
            core::ptr::drop_in_place(guard);
            sleep_binding2(wakeup_token as *const _);
        }
        unsafe { core::ptr::write(guard, lock.lock()) };
    }

    pub fn wakeup(&self, token: usize) {
        extern "C" {
            fn wakeup(chan: *const c_void);
        }

        unsafe { wakeup(token as *const _) };
    }
}

impl !Sync for CPU {}
impl !Send for CPU {}

#[repr(C)]
pub enum ProcessState {
    Unused,
    USed,
    Sleeping,
    Runnable,
    Running,
    Zombie,
}

// Per-process state
#[repr(C)]
pub struct Process {
    lock: SpinLockC,

    // p->lock must be held when using these:
    state: ProcessState, // Process state
    chan: usize,         // If non-zero, sleeping on chan
    killed: i32,         // If non-zero, have been killed
    xstate: i32,         // Exit status to be returned to parent's wait
    pid: i32,            // Process ID

    // wait_lock must be held when using this:
    parent: *mut Process, // Parent process

    // these are private to the process, so p->lock need not be held.
    kstack: usize,                // Virtual address of kernel stack
    sz: usize,                    // Size of process memory (bytes)
    pagetable: PageTable,         // User page table
    trapframe: *mut TrapFrame,    // data page for trampoline.S
    context: Context,             // swtch() here to run process
    ofile: [*mut c_void; NOFILE], // Open files
    cwd: *mut c_void,             // Current directory
    name: [c_char; 16],           // Process name (debugging)
}

pub mod cpu {
    use crate::riscv::read_reg;

    pub fn id() -> usize {
        unsafe { read_reg!(tp) as usize }
    }
}

mod binding {
    use super::*;

    extern "C" {
        fn myproc() -> *mut Process;
        fn sched();
    }

    #[no_mangle]
    unsafe extern "C" fn sleep(chan: usize, lock: *mut SpinLockC) {
        let p = myproc();

        // Must acquire p->lock in order to
        // change p->state and then call sched.
        // Once we hold p->lock, we can be
        // guaranteed that we won't miss any wakeup
        // (wakeup locks p->lock),
        // so it's okay to release lk.

        (*p).lock.raw_lock(); //DOC: sleeplock1
        (*lock).raw_unlock();

        // Go to sleep.
        (*p).chan = chan;
        (*p).state = ProcessState::Sleeping;

        sched();

        // Tidy up.
        (*p).chan = 0;

        // Reacquire original lock.
        (*p).lock.raw_unlock();
        (*lock).raw_lock();
    }
}
