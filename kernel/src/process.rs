pub mod cpu;
//pub mod table;
pub mod trapframe;

use core::ffi::{c_char, c_void};

use crate::{config::NOFILE, context::Context, lock::spin_c::SpinLockC, riscv::paging::PageTable};

use crate::{
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PGSIZE, PTE},
    trampoline::trampoline,
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
    pub lock: SpinLockC,

    // p->lock must be held when using these:
    pub state: ProcessState, // Process state
    pub chan: usize,         // If non-zero, sleeping on chan
    pub killed: i32,         // If non-zero, have been killed
    pub xstate: i32,         // Exit status to be returned to parent's wait
    pub pid: i32,            // Process ID

    // wait_lock must be held when using this:
    pub parent: *mut Process, // Parent process

    // these are private to the process, so p->lock need not be held.
    pub kstack: usize,                // Virtual address of kernel stack
    pub sz: usize,                    // Size of process memory (bytes)
    pub pagetable: PageTable,         // User page table
    pub trapframe: *mut TrapFrame,    // data page for trampoline.S
    pub context: Context,             // swtch() here to run process
    pub ofile: [*mut c_void; NOFILE], // Open files
    pub cwd: *mut c_void,             // Current directory
    pub name: [c_char; 16],           // Process name (debugging)
}

pub fn allocate_pagetable(trapframe: usize) -> Result<PageTable, ()> {
    let mut pagetable = PageTable::allocate()?;
    extern "C" {
        fn uvmfree(pt: PageTable, size: usize);
    }

    // map the trampoline code (for system call return)
    // at the highest user virtual address.
    // only the supervisor uses it, on the way
    // to/from user space, so not PTE_U.
    if pagetable
        .map(TRAMPOLINE, trampoline as usize, PGSIZE, PTE::R | PTE::X)
        .is_err()
    {
        unsafe {
            uvmfree(pagetable, 0);
        }
        return Err(());
    }

    // map the trapframe just below TRAMPOLINE, for trampoline.S.
    if pagetable
        .map(TRAPFRAME, trapframe, PGSIZE, PTE::R | PTE::W)
        .is_err()
    {
        pagetable.unmap(TRAMPOLINE, 1, false);
        unsafe {
            uvmfree(pagetable, 0);
        }
        return Err(());
    }

    Ok(pagetable)
}

pub fn free_pagetable(mut pagetable: PageTable, size: usize) {
    extern "C" {
        fn uvmfree(pt: PageTable, size: usize);
    }

    pagetable.unmap(TRAMPOLINE, 1, false);
    pagetable.unmap(TRAPFRAME, 1, false);
    unsafe { uvmfree(pagetable, size) };
}

mod binding {
    use super::*;

    #[no_mangle]
    extern "C" fn proc_pagetable(trapframe: usize) -> u64 {
        match allocate_pagetable(trapframe) {
            Ok(pt) => pt.as_u64(),
            Err(_) => 0,
        }
    }

    #[no_mangle]
    extern "C" fn proc_freepagetable(pagetable: PageTable, size: usize) {
        free_pagetable(pagetable, size)
    }
}
