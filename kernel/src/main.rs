#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(asm_const)]
#![feature(const_mut_refs)]
#![feature(const_nonnull_new)]
#![feature(const_option)]
#![feature(decl_macro)]
#![feature(generic_arg_infer)]
#![feature(nonzero_ops)]
#![feature(ptr_to_from_bits)]
#![feature(strict_provenance)]

mod alloc;
mod config;
mod context;
mod entry;
mod kernelvec;
mod lock;
mod memory_layout;
mod plic;
mod process;
mod riscv;
mod syscall;
mod trampoline;
mod uart;
mod virtio;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
