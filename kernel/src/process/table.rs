use core::sync::atomic::AtomicUsize;

use arrayvec::ArrayVec;

use crate::{
    config::NPROC,
    spinlock::{SpinLock, SpinLockGuard},
};

use super::Process;

#[derive(Debug)]
struct Parent {
    parent_pid: usize,
    child_pid: usize,
}

#[repr(C)]
#[derive(Debug)]
pub struct ProcessTable {
    procs: [SpinLock<Process>; NPROC],
    parent_maps: SpinLock<ArrayVec<Parent, NPROC>>,
    next_pid: AtomicUsize,
}

impl ProcessTable {
    pub const fn new() -> Self {
        Self {
            procs: [const { SpinLock::new(Process::unused()) }; _],
            parent_maps: SpinLock::new(ArrayVec::new_const()),
            next_pid: AtomicUsize::new(1),
        }
    }

    pub fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }

    pub fn allocate_process(&self) -> Option<SpinLockGuard<Process>> {
        for process in self.procs.iter() {
            let mut process = process.lock();
            if process.state.is_unused() {
                process.state.allocate().unwrap();
                process.pid = self.allocate_pid();
                return Some(process);
            }
        }
        None
    }

    pub fn wakeup(&self, token: usize, current: Option<&SpinLock<Process>>) {
        for process in self.procs.iter() {
            if let Some(current) = current {
                if core::ptr::eq(process, current) {
                    continue;
                }
            }

            let mut process = process.lock();
            if process.state.is_sleeping_on(token) {
                process.state.wakeup().unwrap();
            }
        }
    }

    pub fn kill(&self, pid: usize) -> bool {
        for process in self.procs.iter() {
            let mut process = process.lock();
            if process.pid == pid {
                process.killed = true;
                if process.state.is_sleeping() {
                    process.state.wakeup().unwrap();
                }
                return true;
            }
        }
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpinLock<Process>> {
        self.procs.iter()
    }
}

pub fn get() -> &'static ProcessTable {
    static TABLE: ProcessTable = ProcessTable::new();
    &TABLE
}

pub fn wait_lock() -> &'static mut SpinLock<()> {
    unsafe { &mut *(&get().parent_maps as *const _ as *mut _) }
}
