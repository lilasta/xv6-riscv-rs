#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(incomplete_features)]
#![deny(clippy::disallowed_methods)]
#![feature(allocator_api)]
#![feature(alloc_error_handler)]
#![feature(asm_const)]
#![feature(arbitrary_self_types)]
#![feature(const_convert)]
#![feature(const_eval_select)]
#![feature(const_for)]
#![feature(const_maybe_uninit_uninit_array)]
#![feature(const_maybe_uninit_zeroed)]
#![feature(const_mut_refs)]
#![feature(const_nonnull_new)]
#![feature(const_nonnull_slice_from_raw_parts)]
#![feature(const_option)]
#![feature(const_option_cloned)]
#![feature(const_option_ext)]
#![feature(const_ptr_as_ref)]
#![feature(const_ptr_is_null)]
#![feature(const_ptr_read)]
#![feature(const_ptr_write)]
#![feature(const_slice_index)]
#![feature(const_trait_impl)]
#![feature(const_try)]
#![feature(core_intrinsics)]
#![feature(cstr_from_bytes_until_nul)]
#![feature(decl_macro)]
#![feature(fn_traits)]
#![feature(generic_arg_infer)]
#![feature(generic_const_exprs)]
#![feature(inline_const)]
#![feature(inline_const_pat)]
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

pub mod allocator;
pub mod bitmap;
pub mod buffer;
pub mod cache;
pub mod clock;
pub mod config;
pub mod console;
pub mod context;
pub mod cpu;
pub mod elf;
pub mod entry;
pub mod exec;
pub mod file;
pub mod fs;
pub mod interrupt;
pub mod kernelvec;
pub mod log;
pub mod memory_layout;
pub mod pipe;
pub mod plic;
pub mod process;
pub mod riscv;
pub mod sleeplock;
pub mod spinlock;
pub mod start;
pub mod syscall;
pub mod trampoline;
pub mod trap;
pub mod uart;
pub mod virtio;
pub mod vm;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::console::CONSOLE;

pub macro print($($arg:tt)*) {{
    let _ = write!(CONSOLE.lock(), "{}", format_args!($($arg)*));
}}

pub macro println($($arg:tt)*) {{
    let _ = writeln!(CONSOLE.lock(), "{}", format_args!($($arg)*));
}}

pub macro runtime($e:expr) {{
    use core::intrinsics::const_eval_select;
    use core::marker::Destruct;

    const fn nop<F>(_: F)
    where
        F: FnOnce(),
        F: ~const Destruct,
    {
    }

    fn run<F>(f: F)
    where
        F: FnOnce(),
    {
        f();
    }

    let assert = || $e;
    unsafe { const_eval_select((assert,), nop, run) };
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

pub unsafe extern "C" fn main() {
    static STARTED: AtomicBool = AtomicBool::new(false);

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
        while !STARTED.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }

        println!("hart {} starting", cpu::id());
        vm::initialize_for_core(); // turn on paging
        trap::initialize(); // install kernel trap vector
        plic::initialize_for_core(); // ask PLIC for device interrupts
    }

    process::scheduler();
}
