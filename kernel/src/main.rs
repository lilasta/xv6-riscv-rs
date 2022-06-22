#![no_std]
#![no_main]

mod entry;
mod swtch;
mod trampoline;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
