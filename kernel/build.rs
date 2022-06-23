use std::path::PathBuf;
use std::process::Command;

const CC: &'static str = "riscv64-elf-gcc";
const LD: &'static str = "riscv64-elf-ld";
const OBJCOPY: &'static str = "riscv64-elf-objcopy";

const SRCS: &[&str] = &[
    "start.c",
    "console.c",
    "printf.c",
    "uart.c",
    "kalloc.c",
    "spinlock.c",
    "string.c",
    "main.c",
    "vm.c",
    "proc.c",
    "trap.c",
    "syscall.c",
    "sysproc.c",
    "bio.c",
    "fs.c",
    "log.c",
    "sleeplock.c",
    "file.c",
    "pipe.c",
    "exec.c",
    "sysfile.c",
    "plic.c",
    "virtio_disk.c",
];

const CFLAGS: &[&'static str] = &[
    "-Wall",
    "-Werror",
    "-O",
    "-fno-omit-frame-pointer",
    "-ggdb",
    "-MD",
    "-mcmodel=medany",
    "-ffreestanding",
    "-fno-common",
    "-nostdlib",
    "-mno-relax",
    "-fno-stack-protector",
    "-fno-pie",
    "-no-pie",
    "-march=rv64gc",
    "-mabi=lp64d",
];

const LDFLAGS: &[&'static str] = &["-z", "max-page-size=4096"];

fn main() {
    std::env::set_var("CFLAGS", "");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir = PathBuf::from(out_dir);
    build_initcode(&out_dir);
    build_c();
}

fn build_initcode(out_path: &PathBuf) {
    println!("cargo:rerun-if-changed=c/initcode.S");

    Command::new(CC)
        .args(CFLAGS)
        .args(&["-nostdinc", "-I.", "-Ikernel", "-c", "c/initcode.S", "-o"])
        .arg(out_path.join("initcode.o"))
        .status()
        .unwrap();

    Command::new(LD)
        .args(LDFLAGS)
        .args(&["-N", "-e", "start", "-Ttext", "0", "-o"])
        .arg(out_path.join("initcode.out"))
        .arg(out_path.join("initcode.o"))
        .status()
        .unwrap();

    Command::new(OBJCOPY)
        .args(&["-S", "-O", "binary"])
        .arg(out_path.join("initcode.out"))
        .arg(out_path.join("initcode"))
        .status()
        .unwrap();
}

fn build_c() {
    let mut build = cc::Build::new();

    // set compiler
    build.compiler(CC);

    // enable flags
    for flag in CFLAGS {
        build.flag(flag);
    }

    // set include dir
    build.include("c/");

    // compile
    for file in SRCS {
        let name = file.split('.').next().unwrap();
        build
            .clone()
            .file(&format!("c/{}", file))
            .compile(&format!("lib{}.a", name));
        println!("cargo:rustc-link-lib=static={}", name);
        println!("cargo:rerun-if-changed=c/{}", file);
    }
}
