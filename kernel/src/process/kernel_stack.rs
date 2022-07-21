use crate::{
    allocator::KernelAllocator,
    bitmap::Bitmap,
    config::NPROC,
    lock::{spin::SpinLock, Lock, LockGuard},
    memory_layout::{kstack, kstack_index},
    riscv::paging::{PageTable, PGSIZE, PTE},
};

pub struct KernelStackAllocator {
    bitmap: Bitmap<NPROC>,
}

impl KernelStackAllocator {
    pub const fn new() -> Self {
        Self {
            bitmap: Bitmap::new(),
        }
    }

    pub fn initialize(&self, pagetable: &mut PageTable) {
        for i in 0..self.bitmap.bits() {
            let memory = KernelAllocator::get().allocate_page().unwrap();
            let pa = memory.addr().get();
            let va = kstack(i);
            pagetable.map(va, pa, PGSIZE, PTE::R | PTE::W).unwrap();
        }
    }

    pub fn allocate(&mut self) -> Option<usize> {
        for i in 0..self.bitmap.bits() {
            if self.bitmap.get(i) == Some(false) {
                self.bitmap.set(i, true).unwrap();
                return Some(kstack(i));
            }
        }
        None
    }

    pub fn deallocate(&mut self, addr: usize) {
        let index = kstack_index(addr);
        assert!(self.bitmap.get(index) == Some(true));
        self.bitmap.set(index, false).unwrap();
    }
}

pub fn kstack_allocator() -> LockGuard<'static, SpinLock<KernelStackAllocator>> {
    static KSTACK_ALLOCATOR: SpinLock<KernelStackAllocator> =
        SpinLock::new(KernelStackAllocator::new());
    KSTACK_ALLOCATOR.lock()
}
