use core::sync::atomic::AtomicUsize;

use arrayvec::ArrayVec;

use crate::{config::NPROC, lock::spin::SpinLock, memory_layout::kstack};

use super::Process;

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

    fn allocate_pid(&self) -> usize {
        use core::sync::atomic::Ordering::AcqRel;
        self.next_pid.fetch_add(1, AcqRel)
    }
}

pub fn table() -> &'static mut ProcessTable {
    static mut TABLE: ProcessTable = ProcessTable::new();
    unsafe { &mut TABLE }
}

mod binding {
    use super::*;

    #[no_mangle]
    extern "C" fn allocpid() -> i32 {
        table().allocate_pid() as i32
    }

    #[no_mangle]
    extern "C" fn procinit() {
        table().init();
    }

    #[no_mangle]
    extern "C" fn proc(index: i32) -> *mut Process {
        &mut table().procs[index as usize]
    }
}
