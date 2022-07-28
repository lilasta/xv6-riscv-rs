use core::{
    ffi::{c_char, c_void},
    ptr::NonNull,
};

use crate::{
    allocator::KernelAllocator,
    config::NOFILE,
    process::{allocate_pagetable, table},
    riscv::paging::{PageTable, PGSIZE},
};

use super::{context::CPUContext, forkret, free_pagetable, trapframe::TrapFrame};

#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
pub enum ProcessState {
    Unused,
    Used,
    Sleeping,
    Runnable,
    Running,
    Zombie,
}

// Per-process state
#[derive(Debug)]
pub struct Process {
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
    pub pagetable: Option<PageTable>, // User page table
    pub trapframe: *mut TrapFrame,    // data page for trampoline.S
    pub context: CPUContext,          // swtch() here to run process
    pub ofile: [*mut c_void; NOFILE], // Open files
    pub cwd: *mut c_void,             // Current directory
    pub name: [c_char; 16],           // Process name (debugging)
}

impl Process {
    pub const fn unused() -> Self {
        Self {
            state: ProcessState::Unused,
            chan: 0,
            killed: 0,
            xstate: 0,
            pid: 0,
            parent: core::ptr::null_mut(),
            kstack: 0,
            sz: 0,
            pagetable: None,
            trapframe: core::ptr::null_mut(),
            context: CPUContext::zeroed(),
            ofile: [core::ptr::null_mut(); _],
            cwd: core::ptr::null_mut(),
            name: [0; _],
        }
    }

    // Look in the process table for an UNUSED proc.
    // If found, initialize state required to run in the kernel,
    // and return with p->lock held.
    // If there are no free procs, or a memory allocation fails, return 0.
    pub unsafe fn allocate(&mut self) {
        self.pid = table::table().allocate_pid() as _;
        self.state = ProcessState::Used;

        // Allocate a trapframe page.
        match KernelAllocator::get().allocate() {
            Some(trapframe) => self.trapframe = trapframe.as_ptr(),
            None => {
                self.deallocate();
                return;
            }
        }

        // An empty user page table.
        match allocate_pagetable(self.trapframe.addr()) {
            Ok(pagetable) => self.pagetable = Some(pagetable),
            Err(_) => {
                self.deallocate();
                return;
            }
        }

        // Set up new context to start executing at forkret,
        // which returns to user space.
        self.context = CPUContext::zeroed();
        self.context.ra = forkret as u64;
        self.context.sp = (self.kstack + PGSIZE) as u64;
    }

    // free a proc structure and the data hanging from it,
    // including user pages.
    // p->lock must be held.
    pub unsafe fn deallocate(&mut self) {
        if !self.trapframe.is_null() {
            KernelAllocator::get().deallocate(NonNull::new_unchecked(self.trapframe));
            self.trapframe = core::ptr::null_mut();
        }

        if let Some(pagetable) = &self.pagetable {
            free_pagetable(*pagetable, self.sz);
        }

        self.sz = 0;
        self.pid = 0;
        self.parent = core::ptr::null_mut();
        self.name[0] = 0;
        self.chan = 0;
        self.killed = 0;
        self.xstate = 0;
        self.state = ProcessState::Unused;
    }

    pub fn resize_memory(&mut self, n: isize) -> Result<(), ()> {
        if n == 0 {
            return Ok(());
        }

        let old_size = self.sz;
        let new_size = self.sz.wrapping_add_signed(n);
        if n > 0 {
            self.sz = self.pagetable.unwrap().grow(old_size, new_size)?;
        } else {
            self.sz = self.pagetable.unwrap().shrink(old_size, new_size)?;
        }
        return Ok(());
    }

    pub fn dump(&self) {
        if self.state == ProcessState::Unused {
            return;
        }

        crate::println!("{} {:?} {:?}", self.pid, self.state, self.name);
    }
}
