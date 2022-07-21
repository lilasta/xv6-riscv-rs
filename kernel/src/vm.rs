//! the kernel's page table.

use crate::{
    memory_layout::{symbol_addr, KERNBASE, PHYSTOP, PLIC, TRAMPOLINE, UART0, VIRTIO0},
    process::kernel_stack::kstack_allocator,
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
    kstack_allocator().initialize(&mut pagetable);

    pagetable
}

pub mod binding {
    use core::{arch::riscv64::sfence_vma, ptr::NonNull};

    use crate::{
        allocator::KernelAllocator,
        riscv::{
            paging::{pg_rounddown, pg_roundup},
            satp::make_satp,
            write_csr,
        },
    };

    use super::*;

    static mut KERNEL_PAGETABLE: PageTable = PageTable::invalid();

    #[no_mangle]
    unsafe extern "C" fn kvminit() {
        KERNEL_PAGETABLE = make_pagetable_for_kernel();
    }

    // Switch h/w page table register to the kernel's page table,
    // and enable paging.
    #[no_mangle]
    unsafe extern "C" fn kvminithart() {
        write_csr!(satp, make_satp(KERNEL_PAGETABLE.as_u64()));
        sfence_vma(0, 0);
    }

    #[no_mangle]
    unsafe extern "C" fn walk(pagetable: PageTable, va: usize, alloc: i32) -> *mut PTE {
        match pagetable.search_entry(va, if alloc != 0 { true } else { false }) {
            Ok(pte) => pte,
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

    #[no_mangle]
    unsafe extern "C" fn uvmunmap(mut pagetable: PageTable, va: usize, npages: usize, free: i32) {
        pagetable.unmap(va, npages, if free != 0 { true } else { false })
    }

    // create an empty user page table.
    // returns 0 if out of memory.
    #[no_mangle]
    unsafe extern "C" fn uvmcreate() -> u64 {
        PageTable::allocate().map(|t| t.as_u64()).unwrap_or(0)
    }

    // Load the user initcode into address 0 of pagetable,
    // for the very first process.
    // sz must be less than a page.
    #[no_mangle]
    pub unsafe extern "C" fn uvminit(mut pagetable: PageTable, src: *const u8, size: usize) {
        assert!(size < PGSIZE);

        let mem: NonNull<u8> = KernelAllocator::get().allocate().unwrap();
        core::ptr::write_bytes(mem.as_ptr(), 0, PGSIZE);

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

    #[no_mangle]
    unsafe extern "C" fn uvmcopy(pagetable: PageTable, mut to: PageTable, size: usize) -> i32 {
        match pagetable.copy(&mut to, size) {
            Ok(_) => 0,
            Err(_) => -1,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn uvmalloc(
        mut pagetable: PageTable,
        old_size: usize,
        new_size: usize,
    ) -> usize {
        match pagetable.grow(old_size, new_size) {
            Ok(sz) => sz,
            Err(_) => 0,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn uvmdealloc(
        mut pagetable: PageTable,
        old_size: usize,
        new_size: usize,
    ) -> usize {
        match pagetable.shrink(old_size, new_size) {
            Ok(sz) => sz,
            Err(_) => 0,
        }
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
        let pte = pagetable.search_entry(va, false).unwrap();
        pte.set_user_access(false);
    }

    // Copy from kernel to user.
    // Copy len bytes from src to virtual address dstva in a given page table.
    // Return 0 on success, -1 on error.
    #[no_mangle]
    pub unsafe extern "C" fn copyout(
        pagetable: PageTable,
        mut dst_va: usize,
        mut src: usize,
        mut len: usize,
    ) -> i32 {
        while len > 0 {
            let va0 = pg_rounddown(dst_va);
            let Some(pa0) = pagetable.virtual_to_physical(va0) else {
                return -1;
            };

            let offset = dst_va - va0;
            let n = (PGSIZE - offset).min(len);

            core::ptr::copy(
                <*const u8>::from_bits(src),
                <*mut u8>::from_bits(pa0 + offset),
                n,
            );

            len -= n;
            src += n;
            dst_va = va0 + PGSIZE;
        }
        0
    }

    // Copy from user to kernel.
    // Copy len bytes to dst from virtual address srcva in a given page table.
    // Return 0 on success, -1 on error.
    #[no_mangle]
    pub unsafe extern "C" fn copyin(
        pagetable: PageTable,
        mut dst: usize,
        mut src_va: usize,
        mut len: usize,
    ) -> i32 {
        while len > 0 {
            let va0 = pg_rounddown(src_va);
            let Some(pa0) = pagetable.virtual_to_physical(va0) else {
                return -1;
            };

            let offset = src_va - va0;
            let n = (PGSIZE - offset).min(len);

            core::ptr::copy(
                <*const u8>::from_bits(pa0 + offset),
                <*mut u8>::from_bits(dst),
                n,
            );

            len -= n;
            dst += n;
            src_va = va0 + PGSIZE;
        }
        0
    }

    // Copy a null-terminated string from user to kernel.
    // Copy bytes to dst from virtual address srcva in a given page table,
    // until a '\0', or max.
    // Return 0 on success, -1 on error.
    #[no_mangle]
    unsafe extern "C" fn copyinstr(
        pagetable: PageTable,
        mut dst: usize,
        mut src_va: usize,
        mut len: usize,
    ) -> i32 {
        unsafe fn strcpy(src: *const u8, dst: *mut u8, len: usize) -> bool {
            for i in 0..len {
                *dst.add(i) = *src.add(i);

                if *src.add(i) == ('\0' as u8) {
                    return true;
                }
            }
            false
        }

        while len > 0 {
            let va0 = pg_rounddown(src_va);
            let Some(pa0) = pagetable.virtual_to_physical(va0) else {
                return -1;
            };

            let offset = src_va - va0;
            let n = (PGSIZE - offset).min(len);

            let got_null = strcpy(
                <*const u8>::from_bits(pa0 + offset),
                <*mut u8>::from_bits(dst),
                n,
            );

            if got_null {
                return 0;
            }

            len -= n;
            dst += n;
            src_va = va0 + PGSIZE;
        }
        -1
    }
}
