use core::arch::asm;

pub macro read_csr($csr:ident) {
    {
        let mut x: u64;
        asm!(concat!("csrr {r}, ", stringify!($csr)), r = out(reg) x);
        x
    }
}

pub macro write_csr($csr:ident, $val:expr) {
    asm!(concat!("csrw ", stringify!($csr), ", {r}"), r = in(reg) $val);
}

pub macro read_reg($reg:ident) {
    {
        let mut x: u64;
        asm!(concat!("mv {r}, ", stringify!($reg)), r = out(reg) x);
        x
    }
}

pub macro write_reg($reg:ident, $val:expr) {
    asm!(concat!("mv ", stringify!($reg), ", {r}"), r = in(reg) $val);
}

// Machine Status Register, mstatus
pub mod mstatus {
    // previous mode.
    pub const MPP_MASK: u64 = 3u64 << 11;

    pub const MPP_M: u64 = 3u64 << 11;

    pub const MPP_S: u64 = 1u64 << 11;

    pub const MPP_U: u64 = 0u64 << 11;

    // machine-mode interrupt enable.
    pub const MIE: u64 = 1u64 << 3;
}

// Supervisor Status Register, sstatus
pub mod sstatus {
    // Previous mode, 1=Supervisor, 0=User
    pub const SPP: u64 = 1u64 << 8;

    // Supervisor Previous Interrupt Enable
    pub const SPIE: u64 = 1u64 << 5;

    // User Previous Interrupt Enable
    pub const UPIE: u64 = 1u64 << 4;

    // Supervisor Interrupt Enable
    pub const SIE: u64 = 1u64 << 1;

    // User Interrupt Enable
    pub const UIE: u64 = 1u64 << 0;
}

// Supervisor Interrupt Enable
pub mod sie {
    // external
    pub const SEIE: u64 = 1u64 << 9;

    // timer
    pub const STIE: u64 = 1u64 << 5;

    // software
    pub const SSIE: u64 = 1u64 << 1;
}

// Machine-mode Interrupt Enable
pub mod mie {
    // external
    pub const MEIE: u64 = 1u64 << 11;

    // timer
    pub const MTIE: u64 = 1u64 << 7;

    // software
    pub const MSIE: u64 = 1u64 << 3;
}

pub mod satp {
    pub const SV39: u64 = 8u64 << 60;

    // use riscv's sv39 page table scheme.
    pub const fn make_satp(pagetable: u64) -> u64 {
        SV39 | (pagetable >> 12)
    }
}

pub mod paging {
    pub type PTE = u64;

    // 512 PTEs
    pub type PageTable = *mut u64;

    // bytes per page
    pub const PGSIZE: usize = 4096;

    // bits of offset within a page
    pub const PGSHIFT: usize = 12;

    pub const fn pg_roundup(sz: usize) -> usize {
        (sz + PGSIZE - 1) & !(PGSIZE - 1)
    }

    pub const fn pg_rounddown(a: usize) -> usize {
        a & !(PGSIZE - 1)
    }

    pub const fn pa_to_pte(pa: u64) -> u64 {
        (pa >> 12) << 10
    }

    pub const fn pte_to_pa(pte: u64) -> u64 {
        (pte >> 10) << 12
    }

    pub const fn pte_flags(pte: u64) -> u64 {
        pte & 0x3ff
    }

    // extract the three 9-bit page table indices from a virtual address.
    pub const PXMASK: u64 = 0x1FF;

    pub const fn pxshift(level: usize) -> usize {
        PGSHIFT + 9 * level
    }

    pub const fn px(level: usize, va: u64) -> u64 {
        (va >> pxshift(level)) & PXMASK
    }

    // one beyond the highest possible virtual address.
    // MAXVA is actually one bit less than the max allowed by
    // Sv39, to avoid having to sign-extend virtual addresses
    // that have the high bit set.
    pub const MAXVA: u64 = 1u64 << (9 + 9 + 9 + 12 - 1);

    pub mod pte {
        // valid
        pub const V: u64 = 1u64 << 0;

        pub const R: u64 = 1u64 << 1;

        pub const W: u64 = 1u64 << 2;

        pub const X: u64 = 1u64 << 3;

        // 1 -> user can access
        pub const U: u64 = 1u64 << 4;
    }
}

// enable device interrupts
pub unsafe fn enable_interrupt() {
    write_csr!(sstatus, read_csr!(sstatus) | sstatus::SIE);
}

// disable device interrupts
pub unsafe fn disable_interrupt() {
    write_csr!(sstatus, read_csr!(sstatus) & !sstatus::SIE);
}

// are device interrupts enabled?
pub unsafe fn is_interrupt_enabled() -> bool {
    read_csr!(sstatus) & sstatus::SIE != 0
}

// flush the TLB.
pub unsafe fn sfence_vma() {
    // the zero, zero means flush all TLB entries.
    asm!("sfence.vma zero, zero");
}
