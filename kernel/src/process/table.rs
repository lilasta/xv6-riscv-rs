use core::sync::atomic::AtomicUsize;

use arrayvec::ArrayVec;

use crate::{
    config::NPROC,
    process,
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

    pub fn allocate_process(&mut self) -> Option<SpinLockGuard<Process>> {
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

    pub fn wakeup(&mut self, token: usize) {
        for process in self.procs.iter_mut() {
            if let Some(current) = process::current() {
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

    pub fn kill(&mut self, pid: usize) -> bool {
        for process in self.procs.iter_mut() {
            let mut process = process.lock();
            if process.pid == pid {
                process.killed = 1;
                if process.state.is_sleeping() {
                    process.state.wakeup().unwrap();
                }
                return true;
            }
        }
        false
    }

    pub fn register_parent(&mut self, parent_pid: usize, child_pid: usize) {
        self.parent_maps.lock().push(Parent {
            parent_pid,
            child_pid,
        });
    }

    pub fn remove_parent(&mut self, parent_pid: usize) {
        unsafe {
            for map in self.parent_maps.get_mut().iter_mut() {
                if map.parent_pid == parent_pid {
                    map.parent_pid = 1;
                }
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &SpinLock<Process>> {
        self.procs.iter()
    }
}

pub fn get() -> &'static mut ProcessTable {
    static mut TABLE: ProcessTable = ProcessTable::new();
    unsafe { &mut TABLE }
}

pub fn wait_lock() -> *mut SpinLock<()> {
    &mut get().parent_maps as *mut _ as *mut _
}
