#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(asm_const)]
#![feature(decl_macro)]

mod config;
mod context;
mod entry;
mod kernelvec;
mod lock;
mod process;
mod riscv;
mod syscall;
mod trampoline;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
