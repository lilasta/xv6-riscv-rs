use core::{ffi::c_void, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    config::NOFILE,
    process::allocate_pagetable,
    riscv::paging::{PageTable, PGSIZE},
};

use super::{
    context::CPUContext, free_pagetable, kernel_stack::kstack_allocator, trapframe::TrapFrame,
};

#[derive(Debug)]
pub enum Process<M, C, T, X> {
    Invalid,
    Unused,
    Runnable(M, C),
    Running(M),
    Sleeping(M, C, T),
    Zombie(M, X),
}

impl<M, C, T, X> Process<M, C, T, X> {
    pub const fn is_unused(&self) -> bool {
        matches!(self, Self::Unused)
    }

    pub const fn is_sleeping(&self) -> bool {
        matches!(self, Self::Sleeping(_, _, _))
    }

    pub fn is_sleeping_on(&self, token: T) -> bool
    where
        T: PartialEq,
    {
        matches!(self, Self::Sleeping(_,_, on) if *on == token)
    }

    pub const fn is_runnable(&self) -> bool {
        matches!(self, Self::Runnable(_, _))
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

    pub fn setup(&mut self, metadata: M, context: C) -> Result<(), (M, C)> {
        self.transition(|this| match this {
            Self::Unused => (Self::Runnable(metadata, context), Ok(())),
            other => (other, Err((metadata, context))),
        })
    }

    pub fn run(&mut self) -> Result<C, ()> {
        self.transition(|this| match this {
            Self::Runnable(metadata, context) => (Self::Running(metadata), Ok(context)),
            other => (other, Err(())),
        })
    }

    pub fn sleep(&mut self, context: C, token: T) -> Result<(), C> {
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

    pub fn pause(&mut self, context: C) -> Result<(), C> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Runnable(metadata, context), Ok(())),
            other => (other, Err(context)),
        })
    }

    pub fn die(&mut self, exit_status: X) -> Result<(), ()> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Zombie(metadata, exit_status), Ok(())),
            other => (other, Err(())),
        })
    }

    pub fn clear(&mut self) -> Result<M, ()> {
        self.transition(|this| match this {
            Self::Running(metadata) => (Self::Unused, Ok(metadata)),
            other => (other, Err(())),
        })
    }

    pub const fn metadata(&self) -> Option<&M> {
        match self {
            Self::Invalid | Self::Unused => None,
            Self::Runnable(metadata, _)
            | Self::Running(metadata)
            | Self::Sleeping(metadata, _, _)
            | Self::Zombie(metadata, _) => Some(metadata),
        }
    }

    pub const fn metadata_mut(&mut self) -> Option<&mut M> {
        match self {
            Self::Invalid | Self::Unused => None,
            Self::Runnable(metadata, _)
            | Self::Running(metadata)
            | Self::Sleeping(metadata, _, _)
            | Self::Zombie(metadata, _) => Some(metadata),
        }
    }

    pub const fn context(&self) -> Option<&C> {
        match self {
            Self::Invalid | Self::Unused | Self::Running(_) | Self::Zombie(_, _) => None,
            Self::Runnable(_, context) | Self::Sleeping(_, context, _) => Some(context),
        }
    }

    pub const fn context_mut(&mut self) -> Option<&mut C> {
        match self {
            Self::Invalid | Self::Unused | Self::Running(_) | Self::Zombie(_, _) => None,
            Self::Runnable(_, context) | Self::Sleeping(_, context, _) => Some(context),
        }
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
        context.sp = (jump as usize + PGSIZE) as u64;

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

    pub fn resize_memory(&mut self, n: isize) -> Result<(), ()> {
        if n == 0 {
            return Ok(());
        }

        let old_size = self.sz;
        let new_size = self.sz.wrapping_add_signed(n);
        if n > 0 {
            self.sz = self.pagetable.grow(old_size, new_size)?;
        } else {
            self.sz = self.pagetable.shrink(old_size, new_size)?;
        }
        return Ok(());
    }
}

impl Drop for ProcessContext {
    fn drop(&mut self) {
        KernelAllocator::get().deallocate(self.trapframe);
        free_pagetable(self.pagetable, self.sz);
    }
}
