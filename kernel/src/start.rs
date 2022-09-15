use core::arch::asm;

use crate::{
    config::{ENTRY_STACKSIZE, NCPU},
    memory_layout::{clint_mtimecmp, symbol_addr, CLINT_MTIME},
    riscv::{mie, mstatus, read_csr, sie, write_csr, write_reg},
};

#[repr(C, align(16))]
struct Stack0([u8; ENTRY_STACKSIZE * NCPU]);

impl Stack0 {
    pub const fn zeroed() -> Self {
        Stack0([0; _])
    }
}

// entry.S needs one stack per CPU.
#[used]
#[no_mangle]
static mut STACK0: Stack0 = Stack0::zeroed();

// a scratch area per CPU for machine-mode timer interrupts.
#[used]
#[no_mangle]
static mut TIMER_SCRATCH: [[u64; 5]; NCPU] = [[0; _]; _];

#[no_mangle]
unsafe extern "C" fn start() {
    // set M Previous Privilege mode to Supervisor, for mret.
    let mut x = read_csr!(mstatus);
    x &= !mstatus::MPP_MASK;
    x |= mstatus::MPP_S;
    write_csr!(mstatus, x);

    // set M Exception Program Counter to main, for mret.
    // requires gcc -mcmodel=medany
    write_csr!(mepc, crate::main as usize as u64);

    // disable paging for now.
    write_csr!(satp, 0);

    // delegate all interrupts and exceptions to supervisor mode.
    write_csr!(medeleg, 0xffff);
    write_csr!(mideleg, 0xffff);
    write_csr!(sie, read_csr!(sie) | sie::SEIE | sie::STIE | sie::SSIE);

    // configure Physical Memory Protection to give supervisor mode
    // access to all of physical memory.
    write_csr!(pmpaddr0, 0x3fffffffffffffu64);
    write_csr!(pmpcfg0, 0xf);

    // ask for clock interrupts.
    initialize_timer();

    // keep each CPU's hartid in its tp register, for cpuid().
    let id = read_csr!(mhartid);
    write_reg!(tp, id);

    asm!("mret");
}

// set up to receive timer interrupts in machine mode,
// which arrive at timervec in kernelvec.S,
// which turns them into software interrupts for
// devintr() in trap.c.
unsafe fn initialize_timer() {
    let id = read_csr!(mhartid);

    // ask the CLINT for a timer interrupt.
    let interval = 1000000; // cycles; about 1/10th second in qemu.
    <*mut u64>::from_bits(clint_mtimecmp(id))
        .write(<*const u64>::from_bits(CLINT_MTIME).read() + interval);

    // prepare information in scratch[] for timervec.
    // scratch[0..2] : space for timervec to save registers.
    // scratch[3] : address of CLINT MTIMECMP register.
    // scratch[4] : desired interval (in cycles) between timer interrupts.
    let scratch = &mut TIMER_SCRATCH[id as usize];
    scratch[3] = clint_mtimecmp(id) as u64;
    scratch[4] = interval;
    write_csr!(mscratch, scratch as *mut _ as usize);

    // set the machine-mode trap handler.
    write_csr!(mtvec, symbol_addr!(timervec) as u64);

    // enable machine-mode interrupts.
    write_csr!(mstatus, read_csr!(mstatus) | mstatus::MIE);

    write_csr!(mie, read_csr!(mie) | mie::MTIE);
}
