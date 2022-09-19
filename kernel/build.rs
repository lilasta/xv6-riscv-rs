use std::path::PathBuf;
use std::process::Command;

const TOOLPREFIX_DEFAULT: &'static str = "riscv64-elf-";

const CFLAGS: &[&'static str] = &[
    "-Wall",
    "-Werror",
    "-O",
    "-fno-omit-frame-pointer",
    "-ggdb",
    "-gdwarf-2",
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
    print_rerun();
    print_ldflags();

    let prefix = std::env::var("TOOLPREFIX").unwrap_or(TOOLPREFIX_DEFAULT.to_string());
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_dir = PathBuf::from(out_dir);
    build_initcode(
        &format!("{prefix}gcc"),
        &format!("{prefix}ld"),
        &format!("{prefix}objdump"),
        &out_dir,
    );
}

fn print_rerun() {
    let paths = std::fs::read_dir("asm").unwrap();
    for path in paths {
        println!(
            "cargo:rerun-if-changed=asm/{}",
            path.unwrap().file_name().to_str().unwrap()
        );
    }
}

fn print_ldflags() {
    for flag in LDFLAGS {
        println!("cargo:rustc-link-arg={}", flag);
    }
    println!("cargo:rustc-link-arg=-T");
    println!("cargo:rustc-link-arg=linker.ld");
}

fn build_initcode(cc: &str, ld: &str, objcopy: &str, out_path: &PathBuf) {
    println!("cargo:rerun-if-changed=asm/initcode.S");

    Command::new(cc)
        .args(CFLAGS)
        .args(&["-nostdinc", "-I.", "-Ikernel", "-c", "asm/initcode.S", "-o"])
        .arg(out_path.join("initcode.o"))
        .status()
        .unwrap();

    Command::new(ld)
        .args(LDFLAGS)
        .args(&["-N", "-e", "start", "-Ttext", "0", "-o"])
        .arg(out_path.join("initcode.out"))
        .arg(out_path.join("initcode.o"))
        .status()
        .unwrap();

    Command::new(objcopy)
        .args(&["-S", "-O", "binary"])
        .arg(out_path.join("initcode.out"))
        .arg(out_path.join("initcode"))
        .status()
        .unwrap();
}
