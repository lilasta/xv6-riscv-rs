use core::ptr::NonNull;

use crate::allocator::KernelAllocator;

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

    const fn index(level: usize, va: usize) -> usize {
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
#[derive(Debug, Clone, Copy)] // TODO: Remove clone copy
pub struct PageTable(NonNull<PTE>);

impl PageTable {
    pub const LEN: usize = 512;

    pub fn allocate() -> Result<Self, ()> {
        let ptr: NonNull<PTE> = KernelAllocator::get().allocate().ok_or(())?;
        let this = Self::from_ptr(ptr);
        this.clear();
        Ok(this)
    }

    pub fn deallocate(&mut self) {
        for i in 0..Self::LEN {
            let pte = self.get_mut(i);
            if pte.is_valid() {
                assert!(!pte.is_readable());
                assert!(!pte.is_writable());
                assert!(!pte.is_executable());

                let child = pte.get_physical_addr();
                let child = child as *mut PTE;
                let child = NonNull::new(child).unwrap();

                Self::from_ptr(child).deallocate();

                pte.clear();
            }
        }
        KernelAllocator::get().deallocate_page(self.0.cast());
    }

    pub fn copy(&self, mut to: Self, size: usize) -> Result<(), ()> {
        for i in (0..size).step_by(PGSIZE) {
            let pte = self.search_entry(i, false).unwrap();
            assert!(pte.is_valid());

            let mem = match KernelAllocator::get().allocate_page() {
                Some(mem) => mem,
                None => {
                    to.unmap(0, i / PGSIZE, true);
                    return Err(());
                }
            };

            let pa = pte.get_physical_addr();
            let flags = pte.get_flags();

            unsafe { core::ptr::copy(<*const u8>::from_bits(pa), mem.as_ptr(), PGSIZE) };

            if let Err(_) = to.map(i, mem.addr().get(), PGSIZE, flags) {
                KernelAllocator::get().deallocate_page(mem);
                to.unmap(0, i / PGSIZE, true);
                return Err(());
            }
        }

        Ok(())
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
            let pte = self.search_entry(va, true)?;

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

    // Remove npages of mappings starting from va. va must be
    // page-aligned. The mappings must exist.
    // Optionally free the physical memory.
    pub fn unmap(&mut self, va: usize, npages: usize, free: bool) {
        assert!(va % PGSIZE == 0);

        for va in (va..).step_by(PGSIZE).take(npages) {
            let pte = self.search_entry(va, false).unwrap();
            assert!(pte.is_valid());
            assert!(pte.get_flags() != PTE::V);

            if free {
                let ptr = pte.get_physical_addr();
                let ptr = <*mut _>::from_bits(ptr);
                let ptr = NonNull::new(ptr).unwrap();
                KernelAllocator::get().deallocate_page(ptr);
            }

            pte.clear();
        }
    }

    // Allocate PTEs and physical memory to grow process from oldsz to
    // newsz, which need not be page aligned.  Returns new size or 0 on error.
    pub fn grow(&mut self, old_size: usize, new_size: usize) -> Result<usize, ()> {
        if new_size < old_size {
            return Ok(old_size);
        }

        let grow_start = pg_roundup(old_size);
        let grow_end = new_size;
        for a in (grow_start..grow_end).step_by(PGSIZE) {
            // TODO: Rustの仕様
            // https://github.com/rust-lang/rust/issues/87335#issuecomment-1169479987
            // https://github.com/rust-lang/rust/issues/93883
            let mem = KernelAllocator::get().allocate_page();
            let Some(mem) = mem else {
                self.shrink(a, old_size).unwrap();
                return Err(());
            };

            unsafe {
                core::ptr::write_bytes(mem.as_ptr(), 0, PGSIZE);
            }

            let result = self.map(
                a,
                mem.addr().get(),
                PGSIZE,
                PTE::W | PTE::X | PTE::R | PTE::U,
            );

            if result.is_err() {
                KernelAllocator::get().deallocate_page(mem);
                self.shrink(a, old_size).unwrap();
                return Err(());
            }
        }

        Ok(new_size)
    }

    // Deallocate user pages to bring the process size from oldsz to
    // newsz.  oldsz and newsz need not be page-aligned, nor does newsz
    // need to be less than oldsz.  oldsz can be larger than the actual
    // process size.  Returns the new process size.
    pub fn shrink(&mut self, old_size: usize, new_size: usize) -> Result<usize, ()> {
        if new_size >= old_size {
            return Ok(old_size);
        }

        let shrink_start = pg_roundup(new_size);
        let shrink_end = pg_roundup(old_size);
        if shrink_start != shrink_end {
            let npages = (shrink_end - shrink_start) / PGSIZE;
            self.unmap(shrink_start, npages, true);
        }

        Ok(new_size)
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
    pub fn search_entry(&self, va: usize, alloc: bool) -> Result<&'static mut PTE, ()> {
        assert!(va < MAXVA);

        let mut table = Self(self.0);

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

                table = Self::allocate()?;
                table.clear();

                pte.clear();
                pte.set_physical_addr(table.as_ptr() as usize);
                pte.set_valid(true);
            }
        }

        Ok(table.get_mut(PTE::index(0, va)))
    }

    pub fn virtual_to_physical(&self, va: usize) -> Option<usize> {
        if va >= MAXVA {
            return None;
        }

        let pte = self.search_entry(va, false).ok()?;

        if !pte.is_valid() {
            return None;
        }

        // TODO: Ring selection
        if !pte.can_user_access() {
            return None;
        }

        Some(pte.get_physical_addr())
    }
}
