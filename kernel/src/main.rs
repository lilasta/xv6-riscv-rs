#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(asm_const)]
#![feature(decl_macro)]

mod context;
mod entry;
mod kernelvec;
mod riscv;
mod syscall;
mod trampoline;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
