pub mod cpu;
//pub mod table;
pub mod trapframe;

use core::ffi::{c_char, c_void};

use crate::{
    config::NOFILE,
    context::Context,
    lock::{spin_c::SpinLockC, Lock},
    riscv::paging::PageTable,
};

use self::trapframe::TrapFrame;

#[repr(C)]
#[derive(Debug)]
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
#[derive(Debug)]
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

mod binding {
    use super::*;

    extern "C" {
        fn myproc() -> *mut Process;
        fn sched();
    }

    #[no_mangle]
    extern "C" fn cpuid() -> i32 {
        cpu::id() as i32
    }

    #[no_mangle]
    unsafe extern "C" fn pid() -> usize {
        (*myproc()).pid as usize
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

        let process = (*p).lock.lock();
        (*lock).raw_unlock();

        // Go to sleep.
        (*p).chan = chan;
        (*p).state = ProcessState::Sleeping;

        sched();

        // Tidy up.
        (*p).chan = 0;

        // Reacquire original lock.
        Lock::unlock(process);
        (*lock).raw_lock();
    }
}
