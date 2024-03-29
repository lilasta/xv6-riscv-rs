use crate::{
    clock, cpu,
    memory_layout::{symbol_addr, TRAMPOLINE, UART0_IRQ, VIRTIO0_IRQ},
    plic::{plic_claim, plic_complete},
    println, process,
    riscv::{self, paging::PGSIZE, read_csr, read_reg, satp::make_satp, sstatus, write_csr},
    syscall::syscall,
    uart::uartintr,
    virtio,
};

unsafe fn set_kernel_trap() {
    let vec = symbol_addr!(kernelvec);
    unsafe { write_csr!(stvec, vec) };
}

unsafe fn set_user_trap() {
    let user_trap_handler = symbol_addr!(user_trap_handler);
    let trampoline = symbol_addr!(trampoline);
    let vec = TRAMPOLINE + (user_trap_handler - trampoline);
    unsafe { write_csr!(stvec, vec) };
}

pub fn initialize() {
    unsafe { set_kernel_trap() };
}

extern "C" fn usertrap() {
    assert!(unsafe { read_csr!(sstatus) & sstatus::SPP == 0 });

    unsafe { set_kernel_trap() };

    let context = process::context().unwrap();
    context.trapframe.epc = unsafe { read_csr!(sepc) };

    let mut which_device = 0;

    let cause = unsafe { read_csr!(scause) };
    if cause == 8 {
        // system call

        if process::is_killed() == Some(true) {
            unsafe { process::exit(-1) };
        }

        // sepc points to the ecall instruction,
        // but we want to return to the next instruction.
        context.trapframe.epc += 4;

        // an interrupt will change sstatus &c registers,
        // so don't enable until done with those registers.
        unsafe { riscv::enable_interrupt() };

        let index = context.trapframe.a7 as usize;
        context.trapframe.a0 = unsafe { syscall(index).unwrap_or(u64::MAX) };
    } else {
        which_device = device_interrupt_handler();

        if which_device == 0 {
            unsafe {
                println!(
                    "Trap(user): unexpected scause {:x} pid={}",
                    cause,
                    process::id().unwrap()
                );
                println!(
                    "            sepc={:x} stval={:x}",
                    read_csr!(sepc),
                    read_csr!(stval)
                );
                process::set_killed().unwrap();
            }
        }
    }

    if process::is_killed() == Some(true) {
        unsafe { process::exit(-1) };
    }

    if which_device == 2 {
        process::pause()
    }

    usertrapret();
}

pub fn usertrapret() {
    let context = process::context().unwrap();

    // we're about to switch the destination of traps from
    // kerneltrap() to usertrap(), so turn off interrupts until
    // we're back in user space, where usertrap() is correct.
    unsafe { riscv::disable_interrupt() };

    unsafe { set_user_trap() };

    unsafe {
        // set up trapframe values that user_trap_handler will need when
        // the process next re-enters the kernel.
        context.trapframe.kernel_satp = read_csr!(satp);
        context.trapframe.kernel_sp = (context.kstack + PGSIZE) as u64;
        context.trapframe.kernel_trap = usertrap as u64;
        context.trapframe.kernel_hartid = read_reg!(tp);
    }

    unsafe {
        let mut x = read_csr!(sstatus);
        x &= !sstatus::SPP;
        x |= sstatus::SPIE;
        write_csr!(sstatus, x);
    }

    unsafe {
        write_csr!(sepc, context.trapframe.epc);
    }

    let satp = make_satp(context.pagetable.as_u64());

    let kernel_to_user = symbol_addr!(kernel_to_user);
    let trampoline = symbol_addr!(trampoline);
    let trampoline_userret = TRAMPOLINE + (kernel_to_user - trampoline);

    let f: extern "C" fn(u64) = unsafe { core::mem::transmute(trampoline_userret) };
    f(satp)
}

#[no_mangle]
fn kerneltrap() {
    assert!(unsafe { read_csr!(sstatus) & sstatus::SPP != 0 });
    assert!(unsafe { !riscv::is_interrupt_enabled() });

    let which_device = device_interrupt_handler();
    if which_device == 0 {
        unsafe {
            println!("scause {:x}", read_csr!(scause));
            println!("sepc={:x} stval={:x}", read_csr!(sepc), read_csr!(stval));
            panic!("kerneltrap");
        }
    }

    let sepc = unsafe { read_csr!(sepc) };
    let sstatus = unsafe { read_csr!(sstatus) };

    if which_device == 2 && process::is_running() {
        process::pause();
    }

    // the yield() may have caused some traps to occur,
    // so restore trap registers for use by kernelvec.S's sepc instruction.
    unsafe {
        write_csr!(sepc, sepc);
        write_csr!(sstatus, sstatus);
    }
}

// check if it's an external interrupt or software interrupt,
// and handle it.
// returns 2 if timer interrupt,
// 1 if other device,
// 0 if not recognized.
fn device_interrupt_handler() -> usize {
    let cause = unsafe { read_csr!(scause) };
    if cause & 0x8000000000000000 != 0 && cause & 0xff == 9 {
        // this is a supervisor external interrupt, via PLIC.

        // irq indicates which device interrupted.
        let irq = unsafe { plic_claim() as usize };
        if irq == UART0_IRQ {
            unsafe { uartintr() };
        } else if irq == VIRTIO0_IRQ {
            unsafe { virtio::disk::interrupt_handler() };
        } else if irq != 0 {
            println!("unexpected interrupt irq={}", irq);
        }

        if irq != 0 {
            unsafe { plic_complete(irq as u32) };
        }

        return 1;
    }

    if cause == 0x8000000000000001 {
        if cpu::id() == 0 {
            clock::tick();
        }

        unsafe { write_csr!(sip, read_csr!(sip) & !2) };

        return 2;
    }

    0
}
