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
    use super::*;

    #[no_mangle]
    extern "C" fn kvmmake() -> PageTable {
        make_pagetable_for_kernel()
    }
}
