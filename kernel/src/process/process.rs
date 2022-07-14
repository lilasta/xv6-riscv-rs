use core::{ffi::c_void, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    config::NOFILE,
    lock::Lock,
    memory_layout::{TRAMPOLINE, TRAPFRAME},
    riscv::paging::{PageTable, PGSIZE, PTE},
    trampoline::trampoline,
};

use super::{
    context::CPUContext, cpu::forkret, kernel_stack::KernelStackAllocator, trapframe::TrapFrame,
};

#[derive(Debug)]
pub enum Process {
    Invalid,
    Unused,
    Runnable(ProcessMetadata, ProcessContext),
    Running(ProcessMetadata),
    Sleeping(ProcessMetadata, ProcessContext, usize),
    Zombie(ProcessMetadata),
}

impl Process {
    pub const fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }

    pub const fn is_unused(&self) -> bool {
        matches!(self, Self::Unused)
    }

    pub const fn is_sleeping(&self) -> bool {
        matches!(self, Self::Sleeping(_, _, _))
    }

    pub const fn is_sleeping_on(&self, token: usize) -> bool {
        matches!(self, Self::Sleeping(_,_, on) if *on == token)
    }

    pub const fn is_runnable(&self) -> bool {
        matches!(self, Self::Runnable(_, _))
    }

    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running(_))
    }

    pub const fn is_zombie(&self) -> bool {
        matches!(self, Self::Zombie(_))
    }

    fn transition<S, E>(&mut self, f: impl FnOnce(Self) -> (Self, Result<S, E>)) -> Result<S, E> {
        let mut tmp = Self::Invalid;
        core::mem::swap(self, &mut tmp);

        let (mut this, res) = f(tmp);
        core::mem::swap(self, &mut this);

        res
    }

    pub fn setup(
        &mut self,
        metadata: ProcessMetadata,
        context: ProcessContext,
    ) -> Result<(), (ProcessMetadata, ProcessContext)> {
        self.transition(|this| match this {
            Self::Unused => (Self::Runnable(metadata, context), Ok(())),
            other => (other, Err((metadata, context))),
        })
    }

    pub fn run(&mut self) -> Result<ProcessContext, ()> {
        self.transition(|this| match this {
            Self::Runnable(metadata, context) => (Self::Running(metadata), Ok(context)),
            other => (other, Err(())),
        })
    }

    pub fn sleep(&mut self, context: ProcessContext, token: usize) -> Result<(), ProcessContext> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Sleeping(metadata, context, token), Ok(())),
            other => (other, Err(context)),
        })
    }

    pub fn wakeup(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Sleeping(metadata, context, _) => (Self::Runnable(metadata, context), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn pause(&mut self, context: ProcessContext) -> Result<(), ProcessContext> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Runnable(metadata, context), Ok(())),
            other => (other, Err(context)),
        })
    }

    pub fn die(&mut self) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Zombie(metadata), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn clear(&mut self) -> Result<ProcessMetadata, ()> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Unused, Ok(metadata)),
            other => (other, Err(())),
        })
    }

    pub const fn metadata(&self) -> Option<&ProcessMetadata> {
        match self {
            Self::Invalid | Self::Unused => None,
            Self::Runnable(metadata, _)
            | Self::Running(metadata)
            | Self::Sleeping(metadata, _, _)
            | Self::Zombie(metadata) => Some(metadata),
        }
    }

    pub const fn metadata_mut(&mut self) -> Option<&mut ProcessMetadata> {
        match self {
            Self::Invalid | Self::Unused => None,
            Self::Runnable(metadata, _)
            | Self::Running(metadata)
            | Self::Sleeping(metadata, _, _)
            | Self::Zombie(metadata) => Some(metadata),
        }
    }

    pub const fn context(&self) -> Option<&ProcessContext> {
        match self {
            Self::Invalid | Self::Unused | Self::Running(_) | Self::Zombie(_) => None,
            Self::Runnable(_, context) | Self::Sleeping(_, context, _) => Some(context),
        }
    }

    pub const fn context_mut(&mut self) -> Option<&mut ProcessContext> {
        match self {
            Self::Invalid | Self::Unused | Self::Running(_) | Self::Zombie(_) => None,
            Self::Runnable(_, context) | Self::Sleeping(_, context, _) => Some(context),
        }
    }
}

impl PartialEq for Process {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Invalid, Self::Invalid) => true,
            (Self::Unused, Self::Unused) => true,
            (Self::Runnable(meta1, _), Self::Runnable(meta2, _))
            | (Self::Running(meta1), Self::Running(meta2))
            | (Self::Zombie(meta1), Self::Zombie(meta2)) => meta1.pid == meta2.pid,
            _ => false,
        }
    }
}

impl Eq for Process {}

#[derive(Debug)]
pub struct ProcessMetadata {
    pub pid: usize,
    pub name: [char; 16],
    pub killed: bool,
    pub exit_status: i32, // Exit status to be returned to parent's wait
}

impl ProcessMetadata {
    pub const fn new(pid: usize, name: [char; 16]) -> Self {
        Self {
            pid,
            name,
            killed: false,
            exit_status: 0,
        }
    }
}

#[derive(Debug)]
pub struct ProcessContext {
    pub kstack: usize,
    pub size: usize,
    pub pagetable: PageTable,
    pub trapframe: NonNull<TrapFrame>,
    pub context: CPUContext,
    pub ofile: [*mut c_void; NOFILE],
    pub cwd: *mut c_void,
}

impl ProcessContext {
    pub fn allocate() -> Option<Self> {
        let trapframe = KernelAllocator::get().lock().allocate()?;
        let pagetable = match Self::allocate_pagetable(trapframe.addr().get()) {
            Ok(mem) => mem,
            Err(_) => {
                KernelAllocator::get().lock().deallocate(trapframe);
                return None;
            }
        };

        let kstack = KernelStackAllocator::get().allocate();

        let mut context = CPUContext::zeroed();
        context.ra = forkret as u64;
        context.sp = (kstack + PGSIZE) as u64;

        Some(Self {
            kstack,
            size: 0,
            pagetable,
            trapframe,
            context,
            ofile: [core::ptr::null_mut(); _],
            cwd: core::ptr::null_mut(),
        })
    }

    pub fn duplicate(&self) -> Result<Self, ()> {
        let mut pagetable = PageTable::allocate()?;

        // Copy user memory from parent to child.
        if self.pagetable.copy(&mut pagetable, self.size).is_err() {
            pagetable.deallocate();
            return Err(());
        }

        let mut trapframe = KernelAllocator::get().lock().allocate().unwrap();
        unsafe {
            *trapframe.as_ptr() = self.trapframe.as_ptr().read();
            trapframe.as_mut().a0 = 0;
        }

        // increment reference counts on open file descriptors.
        extern "C" {
            fn filedup(fd: *mut c_void) -> *mut c_void;
            fn idup(fd: *mut c_void) -> *mut c_void;
        }

        let mut ofile = [core::ptr::null_mut(); _];
        for (f, nf) in self.ofile.iter().zip(ofile.iter_mut()) {
            if !f.is_null() {
                *nf = unsafe { filedup(*f) };
            }
        }

        let cwd = unsafe { idup(self.cwd) };

        let kstack = KernelStackAllocator::get().allocate();

        Ok(Self {
            kstack,
            size: self.size,
            pagetable,
            trapframe,
            context: CPUContext::zeroed(),
            ofile,
            cwd,
        })
    }

    pub fn resize_memory(&mut self, n: isize) -> Result<(), ()> {
        if n == 0 {
            return Ok(());
        }

        let old_size = self.size;
        let new_size = self.size.wrapping_add_signed(n);
        if n > 0 {
            self.size = self.pagetable.grow(old_size, new_size)?;
        } else {
            self.size = self.pagetable.shrink(old_size, new_size)?;
        }
        return Ok(());
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
        pagetable.deallocate();
    }
}

impl Drop for ProcessContext {
    fn drop(&mut self) {
        let allocator = KernelAllocator::get();

        allocator.lock().deallocate(self.trapframe);
        Self::free_pagetable(self.pagetable, self.size);

        KernelStackAllocator::get().deallocate(self.kstack);

        // Close all open files.
        for fd in 0..NOFILE {
            if !self.ofile[fd].is_null() {
                extern "C" {
                    fn fileclose(fd: *mut c_void);
                }
                unsafe {
                    fileclose(self.ofile[fd]);
                    self.ofile[fd] = core::ptr::null_mut();
                }
            }
        }

        extern "C" {
            fn begin_op();
            fn iput(cwd: *mut c_void);
            fn end_op();
        }
        unsafe {
            begin_op();
            iput(self.cwd);
            end_op();
            self.cwd = core::ptr::null_mut();
        }
    }
}
