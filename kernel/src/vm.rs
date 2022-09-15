//! the kernel's page table.

use core::{arch::riscv64::sfence_vma, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    memory_layout::{symbol_addr, KERNBASE, PHYSTOP, PLIC, TRAMPOLINE, UART0, VIRTIO0},
    process,
    riscv::{
        paging::{pg_rounddown, PageTable, PGSIZE, PTE},
        satp::make_satp,
        write_csr,
    },
};

// kernel.ld sets this to end of kernel code.
fn etext() -> usize {
    symbol_addr!(etext)
}

// trampoline.S
fn trampoline() -> usize {
    symbol_addr!(trampoline)
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
    process::initialize_kstack(&mut pagetable);

    pagetable
}

// Load the user initcode into address 0 of pagetable,
// for the very first process.
// sz must be less than a page.
pub unsafe fn uvminit(pagetable: &mut PageTable, src: *const u8, size: usize) {
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

pub trait PageTableExtension {
    unsafe fn write<T: ?Sized>(&self, dst_va: usize, src: &T) -> Result<(), usize>;
    unsafe fn read<T: ?Sized>(&self, dst: &mut T, src_va: usize) -> Result<(), usize>;
}

impl PageTableExtension for PageTable {
    unsafe fn write<T: ?Sized>(&self, mut dst_va: usize, src: &T) -> Result<(), usize> {
        let src_size = core::mem::size_of_val(src);

        let mut copied = 0;
        while copied < src_size {
            let va0 = pg_rounddown(dst_va);

            let Some(pa0) = self.virtual_to_physical(va0) else {
                return Err(copied);
            };

            let offset = dst_va - va0;
            let remain = src_size - copied;
            let bytes = (PGSIZE - offset).min(remain);

            unsafe {
                core::ptr::copy(
                    <*const T>::cast::<u8>(src).add(copied),
                    <*mut u8>::from_bits(pa0 + offset),
                    bytes,
                );
            }

            copied += bytes;
            dst_va = va0 + PGSIZE;
        }
        Ok(())
    }

    unsafe fn read<T: ?Sized>(&self, dst: &mut T, mut src_va: usize) -> Result<(), usize> {
        let dst_size = core::mem::size_of_val(dst);

        let mut copied = 0;
        while copied < dst_size {
            let va0 = pg_rounddown(src_va);

            let Some(pa0) = self.virtual_to_physical(va0) else {
                return Err(copied);
            };

            let offset = src_va - va0;
            let remain = dst_size - copied;
            let bytes = (PGSIZE - offset).min(remain);

            core::ptr::copy(
                <*const u8>::from_bits(pa0 + offset),
                <*mut T>::cast::<u8>(dst).add(copied),
                bytes,
            );

            copied += bytes;
            src_va = va0 + PGSIZE;
        }
        Ok(())
    }
}

static mut KERNEL_PAGETABLE: PageTable = PageTable::invalid();

pub unsafe fn initialize() {
    KERNEL_PAGETABLE = make_pagetable_for_kernel();
}

// Switch h/w page table register to the kernel's page table,
// and enable paging.
pub unsafe fn initialize_for_core() {
    write_csr!(satp, make_satp(KERNEL_PAGETABLE.as_u64()));
    sfence_vma(0, 0);
}

// Copy a null-terminated string from user to kernel.
// Copy bytes to dst from virtual address srcva in a given page table,
// until a '\0', or max.
// Return 0 on success, -1 on error.
pub unsafe fn copyinstr(
    pagetable: &PageTable,
    mut dst: usize,
    mut src_va: usize,
    mut len: usize,
) -> Result<(), ()> {
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
                return Err(());
            };

        let offset = src_va - va0;
        let n = (PGSIZE - offset).min(len);

        let got_null = strcpy(
            <*const u8>::from_bits(pa0 + offset),
            <*mut u8>::from_bits(dst),
            n,
        );

        if got_null {
            return Ok(());
        }

        len -= n;
        dst += n;
        src_va = va0 + PGSIZE;
    }
    Err(())
}
