#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(incomplete_features)]
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
#![feature(slice_from_ptr_range)]
#![feature(slice_ptr_get)]
#![feature(stdsimd)]
#![feature(strict_provenance)]

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

pub struct Print;

impl core::fmt::Write for Print {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        extern "C" {
            fn consputc(c: i32);
        }

        for ch in s.chars() {
            unsafe { consputc(ch as i32) };
        }

        core::fmt::Result::Ok(())
    }
}

pub macro print($($arg:tt)*) {{
    use core::fmt::Write;
    let _ = writeln!(crate::Print, "{}", format_args!($($arg)*));
}}

pub macro println($fmt:expr, $($arg:tt)*) {
    crate::print!(concat!($fmt, "\n"), $($arg)*)
}

pub macro cstr($s:literal) {
    core::ffi::CStr::from_bytes_with_nul_unchecked(concat!($s, '\0').as_bytes())
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let _ = writeln!(Print, "{}", info);
    loop {}
}
