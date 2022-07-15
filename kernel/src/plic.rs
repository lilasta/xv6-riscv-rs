//! the riscv Platform Level Interrupt Controller (PLIC).

use crate::{
    memory_layout::{plic_sclaim, plic_senable, plic_spriority, PLIC, UART0_IRQ, VIRTIO0_IRQ},
    process::cpu,
    riscv::read_reg,
};

#[no_mangle]
unsafe extern "C" fn plicinit() {
    // set desired IRQ priorities non-zero (otherwise disabled).
    <*mut u32>::from_bits(PLIC + UART0_IRQ * 4).write(1);
    <*mut u32>::from_bits(PLIC + VIRTIO0_IRQ * 4).write(1);
}

#[no_mangle]
unsafe extern "C" fn plicinithart() {
    let hart = cpu::id() as u64;

    // set uart's enable bit for this hart's S-mode.
    <*mut u32>::from_bits(plic_senable(hart)).write((1 << UART0_IRQ) | (1 << VIRTIO0_IRQ));

    // set this hart's S-mode priority threshold to 0.
    <*mut u32>::from_bits(plic_spriority(hart)).write(0);
}

// ask the PLIC what interrupt we should serve.
#[no_mangle]
unsafe extern "C" fn plic_claim() -> u32 {
    let hart = cpu::id();
    let irq = <*mut u32>::from_bits(plic_sclaim(hart as u64)).read();
    irq
}

// tell the PLIC we've served this IRQ.
#[no_mangle]
unsafe extern "C" fn plic_complete(irq: u32) {
    let hart = cpu::id();
    <*mut u32>::from_bits(plic_sclaim(hart as u64)).write(irq);
}
