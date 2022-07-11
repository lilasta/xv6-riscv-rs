//! Format of an ELF executable file

// File header
#[repr(C)]
pub struct ELFHeader {
    magic: u32,
    elf: [u8; 12],
    kind: u16,
    machine: u16,
    version: u32,
    entry: usize,
    phoff: usize,
    shoff: usize,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

// Program section header
#[repr(C)]
pub struct ProgramHeader {
    kind: u32,
    flags: u32,
    off: usize,
    vaddr: usize,
    paddr: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
}

impl ProgramHeader {
    pub const KIND_LOAD: u32 = 1;
    pub const FLAG_EXEC: u32 = 1;
    pub const FLAG_WRITE: u32 = 2;
    pub const FLAG_READ: u32 = 4;
}
