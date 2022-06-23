#![no_std]
#![no_main]
#![allow(dead_code)]
#![feature(asm_const)]

mod context;
mod entry;
mod kernelvec;
mod syscall;
mod trampoline;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
