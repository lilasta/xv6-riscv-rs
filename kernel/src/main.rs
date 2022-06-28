#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(asm_const)]
#![feature(const_mut_refs)]
#![feature(const_nonnull_new)]
#![feature(const_nonnull_slice_from_raw_parts)]
#![feature(const_option)]
#![feature(const_ptr_as_ref)]
#![feature(const_ptr_is_null)]
#![feature(const_ptr_write)]
#![feature(const_slice_index)]
#![feature(const_try)]
#![feature(decl_macro)]
#![feature(generic_arg_infer)]
#![feature(let_else)]
#![feature(nonnull_slice_from_raw_parts)]
#![feature(nonzero_ops)]
#![feature(ptr_to_from_bits)]
#![feature(slice_ptr_get)]
#![feature(strict_provenance)]

mod allocator;
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
mod vm;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
