//! the kernel's page table.

use core::arch::riscv64::sfence_vma;

use crate::{
    memory_layout::{symbol_addr, KERNBASE, PHYSTOP, PLIC, TRAMPOLINE, UART0, VIRTIO0},
    process,
    riscv::{
        paging::{PageTable, PGSIZE, PTE},
        satp::make_satp,
        write_csr,
    },
};

static mut KERNEL_PAGETABLE: Option<PageTable> = None;

pub unsafe fn initialize() {
    let mut pagetable = PageTable::allocate().unwrap();
    let etext = symbol_addr!(etext);
    let trampoline = symbol_addr!(trampoline);

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
        .map(KERNBASE, KERNBASE, etext - KERNBASE, PTE::R | PTE::X)
        .unwrap();

    // map kernel data and the physical RAM we'll make use of.
    pagetable
        .map(etext, etext, PHYSTOP - etext, PTE::R | PTE::W)
        .unwrap();

    // map the trampoline for trap entry/exit to
    // the highest virtual address in the kernel.
    pagetable
        .map(TRAMPOLINE, trampoline, PGSIZE, PTE::R | PTE::X)
        .unwrap();

    // map kernel stacks
    process::initialize_kstack(&mut pagetable);

    KERNEL_PAGETABLE = Some(pagetable);
}

// Switch h/w page table register to the kernel's page table,
// and enable paging.
pub unsafe fn initialize_for_core() {
    sfence_vma(0, 0);
    write_csr!(satp, make_satp(KERNEL_PAGETABLE.as_ref().unwrap().as_u64()));
    sfence_vma(0, 0);
}
