//! Physical memory allocator, for user processes,
//! kernel stacks, page-table pages,
//! and pipe buffers. Allocates whole 4096-byte pages.

use core::{
    alloc::{AllocError, Allocator, GlobalAlloc, Layout},
    ptr::NonNull,
};

use crate::{
    memory_layout::{symbol_addr, PHYSTOP},
    riscv::paging::{pg_roundup, PGSIZE},
    spinlock::{SpinLock, SpinLockGuard},
};

struct Block {
    next: Option<NonNull<Block>>,
}

pub struct KernelAllocator {
    head: Option<NonNull<Block>>,
}

impl KernelAllocator {
    pub const fn empty() -> Self {
        Self { head: None }
    }

    fn register_blocks(&mut self, phy_start: usize, phy_end: usize) {
        let phy_start = pg_roundup(phy_start);
        let range = phy_start..=(phy_end - PGSIZE);

        for page in range.step_by(PGSIZE) {
            let page = page as *mut u8;
            let page = NonNull::new(page).unwrap();
            self.deallocate_page(page);
        }
    }

    // Free the page of physical memory pointed at by pa,
    // which normally should have been returned by a
    // call to kalloc().  (The exception is when
    // initializing the allocator; see kinit above.)
    pub fn deallocate_page(&mut self, pa: NonNull<u8>) {
        assert!(pa.addr().get() % PGSIZE == 0);
        assert!(pa.addr().get() >= symbol_addr!(end));
        assert!(pa.addr().get() < PHYSTOP);

        // Fill with junk to catch dangling refs.
        unsafe {
            core::ptr::write_bytes(pa.as_ptr(), 1, PGSIZE);
        }

        unsafe {
            let mut block: NonNull<Block> = pa.cast();
            block.as_mut().next = self.head;
            self.head = Some(block);
        }
    }

    // Allocate one 4096-byte page of physical memory.
    // Returns a pointer that the kernel can use.
    // Returns 0 if the memory cannot be allocated.
    pub fn allocate_page(&mut self) -> Option<NonNull<u8>> {
        let page = self.head?;

        self.head = unsafe { page.as_ref().next };

        let page: NonNull<u8> = page.cast();

        // fill with junk
        unsafe { core::ptr::write_bytes(page.as_ptr(), 5, PGSIZE) };

        Some(page)
    }
}

unsafe impl Allocator for SpinLock<KernelAllocator> {
    fn allocate(&self, layout: core::alloc::Layout) -> Result<NonNull<[u8]>, AllocError> {
        if PGSIZE < layout.size() {
            return Err(AllocError);
        }

        if PGSIZE % layout.align() != 0 {
            return Err(AllocError);
        }

        match self.lock().allocate_page() {
            Some(ptr) => Ok(NonNull::from_raw_parts(ptr.cast(), PGSIZE)),
            None => Err(AllocError),
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, _: Layout) {
        self.lock().deallocate_page(ptr);
    }
}

unsafe impl GlobalAlloc for SpinLock<KernelAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.allocate(layout) {
            Ok(ptr) => ptr.as_mut_ptr().cast(),
            Err(_) => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.deallocate(NonNull::new_unchecked(ptr), layout)
    }
}

pub fn initialize() {
    let mut allocator = get();
    assert!(allocator.head.is_none());

    let phy_start = symbol_addr!(end);
    let phy_end = PHYSTOP;
    allocator.register_blocks(phy_start, phy_end);
}

pub fn get() -> SpinLockGuard<'static, KernelAllocator> {
    #[global_allocator]
    static ALLOCATOR: SpinLock<KernelAllocator> = SpinLock::new(KernelAllocator::empty());
    ALLOCATOR.lock()
}
