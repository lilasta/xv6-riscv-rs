//! the riscv Platform Level Interrupt Controller (PLIC).

use crate::{
    memory_layout::{plic_sclaim, plic_senable, plic_spriority, PLIC, UART0_IRQ, VIRTIO0_IRQ},
    riscv::read_reg,
};

pub unsafe fn initialize() {
    // set desired IRQ priorities non-zero (otherwise disabled).
    core::ptr::from_exposed_addr_mut::<u32>(PLIC + UART0_IRQ * 4).write(1);
    core::ptr::from_exposed_addr_mut::<u32>(PLIC + VIRTIO0_IRQ * 4).write(1);
}

pub unsafe fn initialize_for_core() {
    let hart = read_reg!(tp);

    // set uart's enable bit for this hart's S-mode.
    core::ptr::from_exposed_addr_mut::<u32>(plic_senable(hart))
        .write((1 << UART0_IRQ) | (1 << VIRTIO0_IRQ));

    // set this hart's S-mode priority threshold to 0.
    core::ptr::from_exposed_addr_mut::<u32>(plic_spriority(hart)).write(0);
}

// ask the PLIC what interrupt we should serve.
pub unsafe fn plic_claim() -> u32 {
    let hart = read_reg!(tp);
    core::ptr::from_exposed_addr::<u32>(plic_sclaim(hart)).read()
}

// tell the PLIC we've served this IRQ.
pub unsafe fn plic_complete(irq: u32) {
    let hart = read_reg!(tp);
    core::ptr::from_exposed_addr_mut::<u32>(plic_sclaim(hart)).write(irq);
}
