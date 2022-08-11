use core::{
    ffi::{c_char, c_void},
    ptr::NonNull,
};

use crate::{
    allocator::KernelAllocator,
    config::NOFILE,
    log::LogGuard,
    process::allocate_pagetable,
    riscv::paging::{PageTable, PGSIZE},
};

use super::{
    context::CPUContext, free_pagetable, kernel_stack::kstack_allocator, trapframe::TrapFrame,
};

#[derive(Debug)]
pub enum ProcessState {
    Invalid,
    Unused,
    Used,
    Sleeping(ProcessContext, usize),
    Runnable(ProcessContext),
    Running(ProcessContext),
    Zombie(ProcessContext, i32),
}

impl ProcessState {
    pub const fn is_unused(&self) -> bool {
        matches!(self, Self::Unused)
    }

    pub const fn is_sleeping(&self) -> bool {
        matches!(self, Self::Sleeping(_, _))
    }

    pub fn is_sleeping_on(&self, token: usize) -> bool {
        matches!(self, Self::Sleeping(_, on) if *on == token)
    }

    pub const fn is_runnable(&self) -> bool {
        matches!(self, Self::Runnable(_))
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub const fn is_zombie(&self) -> bool {
        matches!(self, Self::Zombie(_, _))
    }

    fn transition<S, E>(&mut self, f: impl FnOnce(Self) -> (Self, Result<S, E>)) -> Result<S, E> {
        let mut tmp = Self::Invalid;
        core::mem::swap(self, &mut tmp);

        let (mut this, res) = f(tmp);
        core::mem::swap(self, &mut this);

        res
    }

    pub fn allocate(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Unused => (Self::Used, Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn setup(&mut self, context: ProcessContext) -> Result<(), ProcessContext> {
        self.transition(|this| match this {
            Self::Used => (Self::Runnable(context), Ok(())),
            other => (other, Err(context)),
        })
    }

    pub fn run(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Runnable(context) => (Self::Running(context), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn sleep(&mut self, token: usize) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Running(context) => (Self::Sleeping(context, token), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn wakeup(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Sleeping(context, _) => (Self::Runnable(context), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn pause(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Running(context) => (Self::Runnable(context), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn die(&mut self, exit_status: i32) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Running(context) => (Self::Zombie(context, exit_status), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn clear(&mut self) -> Result<ProcessContext, ()> {
        self.transition(|this| match this {
            Self::Running(context) => (Self::Unused, Ok(context)),
            other => (other, Err(())),
        })
    }

    pub const fn context(&mut self) -> Option<&mut ProcessContext> {
        match self {
            Self::Invalid | Self::Unused | Self::Used | Self::Running(_) | Self::Zombie(_, _) => {
                None
            }
            Self::Runnable(context) | Self::Sleeping(context, _) => Some(context),
        }
    }
}

// Per-process state
#[derive(Debug)]
pub struct Process {
    // p->lock must be held when using these:
    pub state: ProcessState, // Process state
    pub killed: i32,         // If non-zero, have been killed
    pub pid: i32,            // Process ID

    // wait_lock must be held when using this:
    pub parent: *mut Process, // Parent process

    // these are private to the process, so p->lock need not be held.
    pub name: [c_char; 16], // Process name (debugging)
}

impl Process {
    pub const fn unused() -> Self {
        Self {
            state: ProcessState::Unused,
            killed: 0,
            pid: 0,
            parent: core::ptr::null_mut(),
            name: [0; _],
        }
    }

    pub fn context(&mut self) -> Option<&mut ProcessContext> {
        match &mut self.state {
            ProcessState::Runnable(context) => Some(context),
            ProcessState::Running(context) => Some(context),
            ProcessState::Sleeping(context, _) => Some(context),
            ProcessState::Zombie(context, _) => Some(context),
            _ => None,
        }
    }

    // free a proc structure and the data hanging from it,
    // including user pages.
    // p->lock must be held.
    pub unsafe fn deallocate(&mut self) {
        self.pid = 0;
        self.parent = core::ptr::null_mut();
        self.name[0] = 0;
        self.killed = 0;
        self.state = ProcessState::Unused;
    }

    pub fn dump(&self) {
        if let ProcessState::Unused = self.state {
            return;
        }

        crate::println!("{} {:?} {:?}", self.pid, self.state, self.name);
    }
}

#[derive(Debug)]
pub struct ProcessContext {
    pub kstack: usize,                 // Virtual address of kernel stack
    pub sz: usize,                     // Size of process memory (bytes)
    pub pagetable: PageTable,          // User page table
    pub trapframe: NonNull<TrapFrame>, // data page for trampoline.S
    pub context: CPUContext,           // swtch() here to run process
    pub ofile: [*mut c_void; NOFILE],  // Open files
    pub cwd: *mut c_void,              // Current directory
}

impl ProcessContext {
    pub fn allocate(jump: extern "C" fn()) -> Result<Self, ()> {
        let trapframe = KernelAllocator::get().allocate().ok_or(())?;
        let pagetable = allocate_pagetable(trapframe.addr().get())?;

        let kstack = kstack_allocator().allocate().ok_or(())?;

        let mut context = CPUContext::zeroed();
        context.ra = jump as u64;
        context.sp = (kstack + PGSIZE) as u64;

        Ok(Self {
            kstack,
            sz: 0,
            pagetable,
            trapframe,
            context,
            ofile: [core::ptr::null_mut(); _],
            cwd: core::ptr::null_mut(),
        })
    }

    // TODO: ProcessFiles
    pub fn release_files(&mut self) {
        extern "C" {
            fn fileclose(fd: *mut c_void);
            fn iput(i: *mut c_void);
        }

        for fd in self.ofile.iter_mut() {
            if !fd.is_null() {
                unsafe { fileclose(*fd) };
                *fd = core::ptr::null_mut();
            }
        }

        let _guard = LogGuard::new();
        unsafe { iput(self.cwd) };
        drop(_guard);
    }

    pub fn resize_memory(&mut self, n: isize) -> Result<usize, ()> {
        let old_size = self.sz;
        let new_size = self.sz.wrapping_add_signed(n);

        if old_size == new_size {
            return Ok(old_size);
        }

        if n > 0 {
            self.sz = self.pagetable.grow(old_size, new_size)?;
        } else {
            self.sz = self.pagetable.shrink(old_size, new_size)?;
        }

        return Ok(old_size);
    }
}

impl Drop for ProcessContext {
    fn drop(&mut self) {
        KernelAllocator::get().deallocate(self.trapframe);
        free_pagetable(self.pagetable, self.sz);
        kstack_allocator().deallocate(self.kstack);
    }
}
