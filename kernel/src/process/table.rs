use core::{ffi::c_char, sync::atomic::AtomicUsize};

use arrayvec::ArrayVec;

use crate::{
    config::NPROC,
    lock::{spin::SpinLock, Lock, LockGuard},
    process,
};

use super::Process;

#[derive(Debug)]
struct Parent {
    parent_pid: usize,
    child_pid: usize,
}

#[derive(Debug)]
pub struct ProcessMetadata {
    pub pid: usize,
    pub name: [c_char; 16],
    pub killed: bool,
    pub parent: *mut SpinLock<Process>,
}

impl ProcessMetadata {
    pub const fn new(pid: usize, name: [c_char; 16]) -> Self {
        Self {
            pid,
            name,
            killed: false,
            parent: core::ptr::null_mut(),
        }
    }
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
            procs: [const { SpinLock::new(Process::Unused) }; _],
            parent_maps: SpinLock::new(ArrayVec::new_const()),
            next_pid: AtomicUsize::new(1),
        }
    }

    pub fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }

    pub fn allocate_process(&mut self) -> Option<LockGuard<SpinLock<Process>>> {
        for process in self.procs.iter() {
            let process = process.lock();
            if process.is_unused() {
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
            if process.is_sleeping_on(token) {
                process.wakeup().unwrap();
            }
        }
    }

    pub fn kill(&mut self, pid: usize) -> bool {
        for process in self.procs.iter_mut() {
            let mut process = process.lock();
            if let Some(metadata) = process.metadata_mut() {
                if metadata.pid == pid {
                    metadata.killed = true;
                    if process.is_sleeping() {
                        process.wakeup().unwrap();
                    }
                    return true;
                }
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

pub fn table() -> &'static mut ProcessTable {
    static mut TABLE: ProcessTable = ProcessTable::new();
    unsafe { &mut TABLE }
}

pub fn wait_lock() -> *mut SpinLock<()> {
    &mut table().parent_maps as *mut _ as *mut _
}
