use core::ptr::NonNull;

use crate::{allocator::KernelAllocator, lock::Lock};

#[repr(transparent)]
pub struct PTE(u64);

impl PTE {
    pub const V: u64 = 1u64 << 0; // valid
    pub const R: u64 = 1u64 << 1;
    pub const W: u64 = 1u64 << 2;
    pub const X: u64 = 1u64 << 3;
    pub const U: u64 = 1u64 << 4; // 1 -> user can access

    const fn set_bit(&mut self, b: bool, mask: u64) {
        if b {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }

    pub const fn index(level: usize, va: usize) -> usize {
        // extract the three 9-bit page table indices from a virtual address.
        let mask = 0x1FF;
        let shift = PGSHIFT + 9 * level;
        (va >> shift) & mask
    }

    pub const fn invalid() -> Self {
        PTE(0)
    }

    pub const fn clear(&mut self) {
        *self = Self::invalid();
    }

    pub const fn set_valid(&mut self, b: bool) {
        self.set_bit(b, Self::V);
    }

    pub const fn set_readable(&mut self, b: bool) {
        self.set_bit(b, Self::R);
    }

    pub const fn set_writable(&mut self, b: bool) {
        self.set_bit(b, Self::W);
    }

    pub const fn set_executable(&mut self, b: bool) {
        self.set_bit(b, Self::X);
    }

    pub const fn set_user_access(&mut self, b: bool) {
        self.set_bit(b, Self::U);
    }

    pub const fn is_valid(&self) -> bool {
        self.0 & Self::V != 0
    }

    pub const fn is_readable(&self) -> bool {
        self.0 & Self::R != 0
    }

    pub const fn is_writable(&self) -> bool {
        self.0 & Self::W != 0
    }

    pub const fn is_executable(&self) -> bool {
        self.0 & Self::X != 0
    }

    pub const fn can_user_access(&self) -> bool {
        self.0 & Self::U != 0
    }

    pub const fn set_physical_addr(&mut self, pa: usize) {
        self.0 &= !(!0 << 10 >> 10 >> 10 << 10);
        self.0 |= (pa as u64) >> 12 << 10;
    }

    pub const fn get_physical_addr(&self) -> usize {
        (self.0 as usize) >> 10 << 12
    }

    pub const fn set_flags(&mut self, flags: u64) {
        assert!(flags & 0x3ff == flags);
        self.0 &= !0x3ff;
        self.0 |= flags as u64;
    }

    pub const fn get_flags(&self) -> u64 {
        self.0 & 0x3ff
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct PageTable(NonNull<PTE>);

impl PageTable {
    // 512 PTEs
    pub const LEN: usize = 512;

    pub fn allocate() -> Option<Self> {
        let ptr: NonNull<PTE> = KernelAllocator::get().lock().allocate()?;
        let this = Self::from_ptr(ptr);
        this.clear();
        Some(this)
    }

    pub fn deallocate(&mut self) {
        for i in 0..Self::LEN {
            let pte = self.get(i);
            if pte.is_valid() {
                assert!(!pte.is_readable());
                assert!(!pte.is_writable());
                assert!(!pte.is_executable());

                let child = pte.get_physical_addr();
                let child = child as *mut PTE;
                let child = NonNull::new(child).unwrap();

                Self::from_ptr(child).deallocate();
            }
        }
        KernelAllocator::get().lock().deallocate_page(self.0.cast());
    }

    pub const fn invalid() -> Self {
        Self(NonNull::dangling())
    }

    pub const fn from_ptr(ptr: NonNull<PTE>) -> Self {
        Self(ptr)
    }

    pub const fn as_ptr(&self) -> *mut PTE {
        self.0.as_ptr()
    }

    pub fn as_u64(&self) -> u64 {
        self.0.as_ptr() as u64
    }

    pub const fn get(&self, index: usize) -> &'static PTE {
        assert!(index < Self::LEN);
        unsafe { self.0.as_ptr().add(index).as_ref().unwrap() }
    }

    pub const fn get_mut(&mut self, index: usize) -> &'static mut PTE {
        assert!(index < Self::LEN);
        unsafe { self.0.as_ptr().add(index).as_mut().unwrap() }
    }

    pub const fn clear(&self) {
        unsafe { core::ptr::write_bytes(self.0.as_ptr(), 0, Self::LEN) };
    }

    // Create PTEs for virtual addresses starting at va that refer to
    // physical addresses starting at pa. va and size might not
    // be page-aligned. Returns 0 on success, -1 if walk() couldn't
    // allocate a needed page-table page.
    pub fn map(&mut self, va: usize, pa: usize, size: usize, flags: u64) -> Result<(), ()> {
        assert!(size > 0);

        let mut pa = pa;
        let mut va = pg_rounddown(va);
        let last = pg_rounddown(va + size - 1);

        loop {
            let pte = Self::walk(self.clone(), va, true)?;

            assert!(!pte.is_valid());

            pte.clear();
            pte.set_physical_addr(pa);
            pte.set_flags(flags);
            pte.set_valid(true);

            if va == last {
                break;
            }

            va += PGSIZE;
            pa += PGSIZE;
        }

        Ok(())
    }

    // Return the address of the PTE in page table pagetable
    // that corresponds to virtual address va.  If alloc!=0,
    // create any required page-table pages.
    //
    // The risc-v Sv39 scheme has three levels of page-table
    // pages. A page-table page contains 512 64-bit PTEs.
    // A 64-bit virtual address is split into five fields:
    //   39..63 -- must be zero.
    //   30..38 -- 9 bits of level-2 index.
    //   21..29 -- 9 bits of level-1 index.
    //   12..20 -- 9 bits of level-0 index.
    //    0..11 -- 12 bits of byte offset within the page.
    pub fn walk(mut table: PageTable, va: usize, alloc: bool) -> Result<&'static mut PTE, ()> {
        assert!(va < MAXVA);

        for level in [2, 1] {
            let index = PTE::index(level, va);
            let pte = table.get_mut(index);

            if pte.is_valid() {
                let nested_table_ptr = pte.get_physical_addr();
                let nested_table_ptr = <*mut _>::from_bits(nested_table_ptr);
                let nested_table_ptr = NonNull::new(nested_table_ptr).unwrap();
                table = Self::from_ptr(nested_table_ptr);
            } else {
                if !alloc {
                    return Err(());
                }

                table = Self::allocate().ok_or(())?;
                table.clear();

                pte.clear();
                pte.set_physical_addr(table.as_ptr() as usize);
                pte.set_valid(true);
            }
        }

        Ok(table.get_mut(PTE::index(0, va)))
    }
}

// bytes per page
pub const PGSIZE: usize = 4096;

// bits of offset within a page
pub const PGSHIFT: usize = 12;

pub const fn pg_roundup(sz: usize) -> usize {
    (sz + PGSIZE - 1) & !(PGSIZE - 1)
}

pub const fn pg_rounddown(a: usize) -> usize {
    a & !(PGSIZE - 1)
}

// one beyond the highest possible virtual address.
// MAXVA is actually one bit less than the max allowed by
// Sv39, to avoid having to sign-extend virtual addresses
// that have the high bit set.
pub const MAXVA: usize = 1usize << (9 + 9 + 9 + 12 - 1);
