//! Physical memory allocator, for user processes,
//! kernel stacks, page-table pages,
//! and pipe buffers. Allocates whole 4096-byte pages.

use core::{alloc::GlobalAlloc, ptr::NonNull};

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
    pub const fn uninit() -> Self {
        Self { head: None }
    }

    // Singleton
    pub fn get() -> SpinLockGuard<'static, KernelAllocator> {
        #[global_allocator]
        static mut ALLOCATOR: SpinLock<KernelAllocator> = SpinLock::new(KernelAllocator::uninit());
        unsafe { ALLOCATOR.lock() }
    }

    pub const fn is_initialized(&self) -> bool {
        !self.head.is_none()
    }

    pub fn initialize(&mut self) {
        assert!(!self.is_initialized());

        let phy_start = symbol_addr!(end);
        let phy_end = PHYSTOP;

        self.register_blocks(phy_start, phy_end);
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

    pub fn allocate<T>(&mut self) -> Option<NonNull<T>> {
        assert!(core::mem::size_of::<T>() <= PGSIZE);
        self.allocate_page().map(NonNull::cast::<T>)
    }

    pub fn deallocate<T>(&mut self, pa: NonNull<T>) {
        assert!(core::mem::size_of::<T>() <= PGSIZE);
        self.deallocate_page(pa.cast());
    }
}

unsafe impl GlobalAlloc for SpinLock<KernelAllocator> {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        if PGSIZE < layout.size() {
            return core::ptr::null_mut();
        }

        if PGSIZE % layout.align() != 0 {
            return core::ptr::null_mut();
        }

        match self.lock().allocate_page() {
            Some(ptr) => ptr.as_ptr(),
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _: core::alloc::Layout) {
        self.lock().deallocate_page(NonNull::new_unchecked(ptr));
    }
}

pub fn initialize() {
    KernelAllocator::get().initialize();
}
