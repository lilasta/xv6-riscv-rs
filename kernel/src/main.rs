#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(incomplete_features)]
#![feature(allocator_api)]
#![feature(alloc_error_handler)]
#![feature(asm_const)]
#![feature(arbitrary_self_types)]
#![feature(const_convert)]
#![feature(const_for)]
#![feature(const_maybe_uninit_uninit_array)]
#![feature(const_maybe_uninit_zeroed)]
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
#![feature(fn_traits)]
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
#![feature(unboxed_closures)]

extern crate alloc;

mod allocator;
mod bitmap;
mod buffer;
mod cache;
mod clock;
mod config;
mod console;
mod context;
mod cpu;
mod elf;
mod entry;
mod exec;
mod file;
mod fs;
mod interrupt;
mod kernelvec;
mod log;
mod memory_layout;
mod pipe;
mod plic;
mod process;
mod riscv;
mod sleeplock;
mod spinlock;
mod start;
mod syscall;
mod trampoline;
mod trap;
mod uart;
mod virtio;
mod vm;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::console::CONSOLE;

pub macro print($($arg:tt)*) {{
    let _ = write!(CONSOLE.lock(), "{}", format_args!($($arg)*));
}}

pub macro println($($arg:tt)*) {{
    let _ = writeln!(CONSOLE.lock(), "{}", format_args!($($arg)*));
}}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = writeln!(CONSOLE.lock(), "{}", info);
    loop {}
}

#[alloc_error_handler]
fn alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Cannot alloc: {:?}", layout);
}

static STARTED: AtomicBool = AtomicBool::new(false);

pub unsafe extern "C" fn main() {
    if cpu::id() == 0 {
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

        println!("hart {} starting", cpu::id());
        vm::initialize_for_core(); // turn on paging
        trap::initialize(); // install kernel trap vector
        plic::initialize_for_core(); // ask PLIC for device interrupts
    }

    process::scheduler();
}
