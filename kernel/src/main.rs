#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(incomplete_features)]
#![feature(alloc_error_handler)]
#![feature(asm_const)]
#![feature(arbitrary_self_types)]
#![feature(const_convert)]
#![feature(const_for)]
#![feature(const_maybe_uninit_uninit_array)]
#![feature(const_mut_refs)]
#![feature(const_nonnull_new)]
#![feature(const_nonnull_slice_from_raw_parts)]
#![feature(const_option)]
#![feature(const_ptr_as_ref)]
#![feature(const_ptr_is_null)]
#![feature(const_ptr_read)]
#![feature(const_ptr_write)]
#![feature(const_slice_index)]
#![feature(const_trait_impl)]
#![feature(const_try)]
#![feature(cstr_from_bytes_until_nul)]
#![feature(decl_macro)]
#![feature(generic_arg_infer)]
#![feature(generic_const_exprs)]
#![feature(inline_const)]
#![feature(inline_const_pat)]
#![feature(let_else)]
#![feature(maybe_uninit_uninit_array)]
#![feature(mixed_integer_ops)]
#![feature(negative_impls)]
#![feature(nonnull_slice_from_raw_parts)]
#![feature(nonzero_ops)]
#![feature(once_cell)]
#![feature(ptr_metadata)]
#![feature(ptr_to_from_bits)]
#![feature(result_option_inspect)]
#![feature(slice_from_ptr_range)]
#![feature(slice_ptr_get)]
#![feature(stdsimd)]
#![feature(strict_provenance)]

extern crate alloc;

mod allocator;
mod bitmap;
mod buffer;
mod cache;
mod config;
mod console;
mod elf;
mod entry;
mod exec;
mod file;
mod fs;
mod interrupt;
mod kernelvec;
mod lock;
mod log;
mod memory_layout;
mod pipe;
mod plic;
mod process;
mod riscv;
mod start;
mod syscall;
mod trampoline;
mod trap;
mod uart;
mod undrop;
mod virtio;
mod vm;

use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
};

use lock::spin::SpinLock;
use lock::Lock;

pub struct Print;

impl core::fmt::Write for Print {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for ch in s.chars() {
            unsafe { console::putc(ch as i32) };
        }

        core::fmt::Result::Ok(())
    }
}

static PRINT: SpinLock<Print> = SpinLock::new(Print);

pub macro print($($arg:tt)*) {{
    let _ = write!(PRINT.lock(), "{}", format_args!($($arg)*));
}}

pub macro println($($arg:tt)*) {{
    let _ = writeln!(PRINT.lock(), "{}", format_args!($($arg)*));
}}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(PRINT.lock(), "{}", info);
    loop {}
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Cannot alloc: {:?}", layout);
}

static STARTED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
unsafe extern "C" fn main() {
    if process::cpuid() == 0 {
        console::initialize();
        println!("");
        println!("xv6 kernel is booting");
        println!("");
        allocator::initialize(); // physical page allocator
        vm::initialize(); // create kernel page table
        vm::initialize_for_core(); // turn on paging
        trap::initialize(); // install kernel trap vector
        plic::initialize(); // set up interrupt controller
        plic::initialize_for_core(); // ask PLIC for device interrupts
        process::setup_init_process(); // first user process
        STARTED.store(true, Ordering::SeqCst);
    } else {
        while !STARTED.load(Ordering::SeqCst) {}

        println!("hart {} starting", process::cpuid());
        vm::initialize_for_core(); // turn on paging
        trap::initialize(); // install kernel trap vector
        plic::initialize_for_core(); // ask PLIC for device interrupts
    }

    process::scheduler();
}
