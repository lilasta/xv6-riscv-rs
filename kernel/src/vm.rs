//! the kernel's page table.

use crate::{
    memory_layout::{symbol_addr, KERNBASE, PHYSTOP, PLIC, TRAMPOLINE, UART0, VIRTIO0},
    riscv::paging::{PageTable, PGSIZE, PTE},
};

// kernel.ld sets this to end of kernel code.
fn etext() -> usize {
    symbol_addr!(etext) as usize
}

// trampoline.S
fn trampoline() -> usize {
    symbol_addr!(trampoline) as usize
}

fn make_pagetable_for_kernel() -> PageTable {
    let mut pagetable = PageTable::allocate().unwrap();
    pagetable.clear();

    // uart registers
    pagetable
        .map(
            UART0.addr().get(),
            UART0.addr().get(),
            PGSIZE,
            PTE::R | PTE::W,
        )
        .unwrap();

    // virtio mmio disk interface
    pagetable
        .map(VIRTIO0, VIRTIO0, PGSIZE, PTE::R | PTE::W)
        .unwrap();

    // PLIC
    pagetable
        .map(PLIC, PLIC, 0x400000, PTE::R | PTE::W)
        .unwrap();

    // map kernel text executable and read-only.
    pagetable
        .map(KERNBASE, KERNBASE, etext() - KERNBASE, PTE::R | PTE::X)
        .unwrap();

    // map kernel data and the physical RAM we'll make use of.
    pagetable
        .map(etext(), etext(), PHYSTOP - etext(), PTE::R | PTE::W)
        .unwrap();

    // map the trampoline for trap entry/exit to
    // the highest virtual address in the kernel.
    pagetable
        .map(TRAMPOLINE, trampoline(), PGSIZE, PTE::R | PTE::X)
        .unwrap();

    // map kernel stacks
    extern "C" {
        fn proc_mapstacks(kpgtbl: PageTable);
    }
    unsafe { proc_mapstacks(pagetable) };

    pagetable
}

mod binding {
    use core::{arch::riscv64::sfence_vma, ptr::NonNull};

    use crate::{
        allocator::KernelAllocator,
        lock::Lock,
        riscv::{paging::pg_roundup, satp::make_satp, write_csr},
    };

    use super::*;

    static mut KERNEL_PAGETABLE: PageTable = PageTable::invalid();

    #[no_mangle]
    unsafe extern "C" fn kvminit() {
        KERNEL_PAGETABLE = make_pagetable_for_kernel();
    }

    #[no_mangle]
    unsafe extern "C" fn kvmmap(
        mut kpgtbl: PageTable,
        va: usize,
        pa: usize,
        size: usize,
        flags: i32,
    ) {
        kpgtbl.map(va, pa, size, flags as u64).unwrap();
    }

    // Create PTEs for virtual addresses starting at va that refer to
    // physical addresses starting at pa. va and size might not
    // be page-aligned. Returns 0 on success, -1 if walk() couldn't
    // allocate a needed page-table page.
    #[no_mangle]
    unsafe extern "C" fn mappages(
        mut kpgtbl: PageTable,
        va: usize,
        size: usize,
        pa: usize,
        flags: i32,
    ) -> i32 {
        match kpgtbl.map(va, pa, size, flags as u64) {
            Ok(_) => 0,
            Err(_) => -1,
        }
    }

    // Switch h/w page table register to the kernel's page table,
    // and enable paging.
    #[no_mangle]
    unsafe extern "C" fn kvminithart() {
        write_csr!(satp, make_satp(KERNEL_PAGETABLE.as_u64()));
        sfence_vma(0, 0);
    }

    // TODO: make private PageTable::walk
    #[no_mangle]
    unsafe extern "C" fn walk(pagetable: PageTable, va: usize, alloc: i32) -> *mut PTE {
        match PageTable::walk(pagetable, va, if alloc != 0 { true } else { false }) {
            Ok(mut_ref) => mut_ref,
            Err(_) => core::ptr::null_mut(),
        }
    }

    // Look up a virtual address, return the physical address,
    // or 0 if not mapped.
    // Can only be used to look up user pages.
    #[no_mangle]
    unsafe extern "C" fn walkaddr(pagetable: PageTable, va: usize) -> usize {
        pagetable.virtual_to_physical(va).unwrap_or(0)
    }

    #[no_mangle]
    unsafe extern "C" fn freewalk(mut pagetable: PageTable) {
        pagetable.deallocate();
    }

    #[no_mangle]
    unsafe extern "C" fn uvmunmap(mut pagetable: PageTable, va: usize, npages: usize, free: i32) {
        pagetable.unmap(va, npages, if free != 0 { true } else { false })
    }

    // create an empty user page table.
    // returns 0 if out of memory.
    #[no_mangle]
    unsafe extern "C" fn uvmcreate() -> PageTable {
        PageTable::allocate().unwrap_or(PageTable::invalid())
    }

    // Free user memory pages,
    // then free page-table pages.
    #[no_mangle]
    unsafe extern "C" fn uvmfree(mut pagetable: PageTable, size: usize) {
        if size > 0 {
            pagetable.unmap(0, pg_roundup(size) / PGSIZE, true);
        }
        pagetable.deallocate();
    }

    // mark a PTE invalid for user access.
    // used by exec for the user stack guard page.
    #[no_mangle]
    unsafe extern "C" fn uvmclear(pagetable: PageTable, va: usize) {
        let pte = PageTable::walk(pagetable, va, false).unwrap();
        pte.set_user_access(false);
    }

    // Load the user initcode into address 0 of pagetable,
    // for the very first process.
    // sz must be less than a page.
    #[no_mangle]
    unsafe extern "C" fn uvminit(mut pagetable: PageTable, src: *mut u8, size: usize) {
        assert!(size < PGSIZE);
        let mem: NonNull<u8> = KernelAllocator::get().lock().allocate().unwrap();
        core::ptr::write_bytes(mem.as_ptr(), 0, PGSIZE);
        extern "C" {
            fn glue(pagetable: PageTable, src: *mut u8, size: u32, mem: *const u8);
        }

        pagetable
            .map(
                0,
                mem.addr().get(),
                PGSIZE,
                PTE::W | PTE::R | PTE::X | PTE::U,
            )
            .unwrap();

        core::ptr::copy_nonoverlapping(src, mem.as_ptr(), size);
    }
}
