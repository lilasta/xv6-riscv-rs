//! メモリアロケータ

use core::{
    alloc::{AllocError, Allocator, GlobalAlloc, Layout},
    ptr::NonNull,
};

use crate::{
    memory_layout::{symbol_addr, PHYSTOP},
    riscv::paging::{pg_roundup, PGSIZE},
    runtime,
    spinlock::SpinLock,
};

/// 空きページ。
struct UnusedPage {
    /// 次の空きページへのリンク
    next: Option<NonNull<Self>>,
}

/// カーネルで使用されるメモリアロケータ。
///
/// 連続したメモリ領域を4096バイトのページ単位に分割して配布する。
#[derive(Debug)]
pub struct KernelAllocator {
    /// 先頭の空きページ
    head: Option<NonNull<UnusedPage>>,
}

impl KernelAllocator {
    /// アロケータを作成する。
    pub const fn empty() -> Self {
        Self { head: None }
    }

    /// メモリ領域をページ単位に分割し
    /// アロケータに登録する。
    fn register_pages(&mut self, addr_start: usize, addr_end: usize) {
        let addr_start = pg_roundup(addr_start);
        let range = addr_start..=(addr_end - PGSIZE);

        for page in range.step_by(PGSIZE) {
            let page = <*mut u8>::from_bits(page);
            let page = NonNull::new(page).unwrap();
            self.deallocate_page(page);
        }
    }

    /// 空きページを返す。
    pub const fn allocate_page(&mut self) -> Option<NonNull<u8>> {
        let page = self.head?;

        self.head = unsafe { page.as_ref().next };

        Some(page.cast::<u8>())
    }

    /// ページを解放する。
    pub const fn deallocate_page(&mut self, page: NonNull<u8>) {
        runtime!({
            let addr = page.addr().get();
            assert!(addr % PGSIZE == 0);
            assert!(addr >= symbol_addr!(end));
            assert!(addr < PHYSTOP);
        });

        unsafe {
            let mut page = page.cast::<UnusedPage>();
            page.as_mut().next = self.head;
            self.head = Some(page);
        }
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
    let mut allocator = get().lock();
    assert!(allocator.head.is_none());

    let start = symbol_addr!(end);
    let end = PHYSTOP;
    allocator.register_pages(start, end);
}

pub fn get() -> &'static SpinLock<KernelAllocator> {
    #[global_allocator]
    static ALLOCATOR: SpinLock<KernelAllocator> = SpinLock::new(KernelAllocator::empty());
    &ALLOCATOR
}
