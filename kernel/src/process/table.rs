use core::sync::atomic::AtomicUsize;

use arrayvec::ArrayVec;

use crate::{
    config::NPROC,
    lock::{spin::SpinLock, Lock},
    memory_layout::kstack,
};

use super::{cpu, Process, ProcessState};

#[derive(Debug)]
struct Parent {
    parent_pid: usize,
    child_pid: usize,
}

#[repr(C)]
#[derive(Debug)]
pub struct ProcessTable {
    procs: [Process; NPROC],
    parent_maps: SpinLock<ArrayVec<Parent, NPROC>>,
    next_pid: AtomicUsize,
}

impl ProcessTable {
    pub const fn new() -> Self {
        Self {
            procs: [const { Process::unused() }; _],
            parent_maps: SpinLock::new(ArrayVec::new_const()),
            next_pid: AtomicUsize::new(1),
        }
    }

    pub fn init(&mut self) {
        for (i, process) in self.procs.iter_mut().enumerate() {
            process.kstack = kstack(i);
        }
    }

    pub fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }

    pub fn allocate_process(&mut self) -> *mut Process {
        for process in self.procs.iter_mut() {
            unsafe { process.lock.raw_lock() };
            if process.state == ProcessState::Unused {
                unsafe { process.allocate() };
                if process.state == ProcessState::Used {
                    return process;
                } else {
                    return core::ptr::null_mut();
                }
            }
            unsafe { process.lock.raw_unlock() };
        }
        core::ptr::null_mut()
    }

    pub fn wakeup(&mut self, token: usize) {
        for process in self.procs.iter_mut() {
            if core::ptr::eq(process, unsafe { cpu::process() }) {
                continue;
            }

            let _guard = process.lock.lock();
            if process.state == ProcessState::Sleeping && process.chan == token {
                process.state = ProcessState::Runnable;
            }
        }
    }

    pub fn kill(&mut self, pid: usize) -> bool {
        for process in self.procs.iter_mut() {
            let _guard = process.lock.lock();
            if process.pid == pid as i32 {
                process.killed = 1;
                if process.state == ProcessState::Sleeping {
                    process.state = ProcessState::Runnable;
                }
                return true;
            }
        }
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = &Process> {
        self.procs.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Process> {
        self.procs.iter_mut()
    }
}

pub fn table() -> &'static mut ProcessTable {
    static mut TABLE: ProcessTable = ProcessTable::new();
    unsafe { &mut TABLE }
}
