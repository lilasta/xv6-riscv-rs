use core::sync::atomic::AtomicUsize;

use arrayvec::ArrayVec;

use crate::{
    config::NPROC,
    lock::{spin_c::SpinLockC, Lock, LockGuard},
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
    procs: [SpinLockC<Process>; NPROC],
    parent_maps: SpinLockC<ArrayVec<Parent, NPROC>>,
    next_pid: AtomicUsize,
}

impl ProcessTable {
    pub const fn new() -> Self {
        Self {
            procs: [const { SpinLockC::new(Process::unused()) }; _],
            parent_maps: SpinLockC::new(ArrayVec::new_const()),
            next_pid: AtomicUsize::new(1),
        }
    }

    pub fn init(&mut self) {
        for (i, process) in self.procs.iter_mut().enumerate() {
            unsafe { process.get_mut().kstack = kstack(i) };
        }
    }

    pub fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }

    pub fn allocate_process(&mut self) -> Option<LockGuard<SpinLockC<Process>>> {
        for process in self.procs.iter() {
            let mut process = process.lock();
            if process.state == ProcessState::Unused {
                unsafe { process.allocate() };
                if process.state == ProcessState::Used {
                    return Some(process);
                } else {
                    return None;
                }
            }
        }
        None
    }

    pub fn wakeup(&mut self, token: usize) {
        for process in self.procs.iter_mut() {
            if let Some(current) = cpu::process() {
                if core::ptr::eq(process, current) {
                    continue;
                }
            }

            let mut process = process.lock();
            if process.state == ProcessState::Sleeping && process.chan == token {
                process.state = ProcessState::Runnable;
            }
        }
    }

    pub fn kill(&mut self, pid: usize) -> bool {
        for process in self.procs.iter_mut() {
            let mut process = process.lock();
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

    pub fn iter(&self) -> impl Iterator<Item = &SpinLockC<Process>> {
        self.procs.iter()
    }
}

pub fn table() -> &'static mut ProcessTable {
    static mut TABLE: ProcessTable = ProcessTable::new();
    unsafe { &mut TABLE }
}

#[no_mangle]
pub extern "C" fn wait_lock() -> *mut SpinLockC<()> {
    &mut table().parent_maps as *mut _ as *mut _
}
