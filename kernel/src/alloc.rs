//! Physical memory allocator, for user processes,
//! kernel stacks, page-table pages,
//! and pipe buffers. Allocates whole 4096-byte pages.

use core::ptr::NonNull;

use crate::{
    memory_layout::PHYSTOP,
    riscv::paging::{pg_roundup, PGSIZE},
};

fn end() -> usize {
    extern "C" {
        // first address after kernel.
        // defined by kernel.ld.
        fn end();
    }

    end as usize
}

pub struct Block {
    next: *mut Block,
}

pub struct Allocator {
    head: *mut Block,
}

impl Allocator {
    pub fn init() -> Self {
        let mut this = Self {
            head: core::ptr::null_mut(),
        };
        this.free_range(end(), PHYSTOP);
        this
    }

    fn free_range(&mut self, pa_start: usize, pa_end: usize) {
        let pa_start = pg_roundup(pa_start);
        let range = pa_start..=(pa_end - PGSIZE);

        for p in range.step_by(PGSIZE) {
            self.free(p);
        }
    }

    // Free the page of physical memory pointed at by v,
    // which normally should have been returned by a
    // call to kalloc().  (The exception is when
    // initializing the allocator; see kinit above.)
    fn free(&mut self, pa: usize) {
        assert!(pa % PGSIZE == 0);
        assert!(pa >= end());
        assert!(pa < PHYSTOP);

        let pa = pa as *mut Block;

        // Fill with junk to catch dangling refs.
        unsafe {
            core::ptr::write_bytes(pa.cast::<u8>(), 1, PGSIZE);
        }

        unsafe {
            let block = &mut *pa;
            block.next = self.head;
            self.head = block;
        }
    }

    // Allocate one 4096-byte page of physical memory.
    // Returns a pointer that the kernel can use.
    // Returns 0 if the memory cannot be allocated.
    pub fn allocate_page(&mut self) -> Option<NonNull<u8>> {
        let page = self.head;

        if page.is_null() {
            return None;
        }

        let next = unsafe { (*page).next };
        self.head = next;

        // fill with junk
        unsafe { core::ptr::write_bytes(page.cast::<u8>(), 5, PGSIZE) };

        NonNull::new(page.cast::<u8>())
    }
}

mod binding {
    use crate::lock::{spin::SpinLock, Lock};

    use super::*;

    #[allow(non_upper_case_globals)]
    static mut allocator: Option<SpinLock<Allocator>> = None;

    #[no_mangle]
    unsafe extern "C" fn kinit() {
        allocator = Some(SpinLock::new(Allocator::init()));
    }

    #[no_mangle]
    unsafe extern "C" fn kfree(pa: usize) {
        allocator.as_mut().unwrap().lock().free(pa);
    }

    #[no_mangle]
    unsafe extern "C" fn kalloc() -> usize {
        match allocator.as_mut().unwrap().lock().allocate_page() {
            Some(ptr) => ptr.addr().into(),
            None => 0,
        }
    }
}
