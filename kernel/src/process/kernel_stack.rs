use core::ptr::NonNull;

use crate::{
    allocator::KernelAllocator,
    config::NPROC,
    lock::{spin::SpinLock, Lock},
    memory_layout::{kstack, kstack_index},
    riscv::paging::{PGSIZE, PTE},
    vm::binding::KERNEL_PAGETABLE,
};

pub struct KernelStackAllocator {
    stacks: SpinLock<[Option<NonNull<u8>>; NPROC]>,
}

impl KernelStackAllocator {
    pub const fn new() -> Self {
        Self {
            stacks: SpinLock::new([None; _]),
        }
    }

    pub fn get() -> &'static mut Self {
        static mut ALLOC: KernelStackAllocator = KernelStackAllocator::new();
        unsafe { &mut ALLOC }
    }

    pub fn allocate(&mut self) -> usize {
        let mut stacks = self.stacks.lock();
        let index = stacks.iter().position(Option::is_none).unwrap();

        let pa = KernelAllocator::get().lock().allocate_page().unwrap();
        let va = kstack(index);

        stacks[index] = Some(pa);

        unsafe {
            KERNEL_PAGETABLE
                .map(va, pa.addr().get(), PGSIZE, PTE::R | PTE::W)
                .unwrap();
        }

        va
    }

    pub fn deallocate(&mut self, kstack: usize) {
        let mut stacks = self.stacks.lock();
        let index = kstack_index(kstack);
        let va = stacks[index].unwrap();
        stacks[index] = None;

        unsafe {
            KERNEL_PAGETABLE.unmap(va.addr().get(), 1, false);
        }

        KernelAllocator::get().lock().deallocate_page(va);
    }
}
