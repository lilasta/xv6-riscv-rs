//! Format of an ELF executable file

// File header
#[repr(C)]
pub struct ELFHeader {
    pub magic: u32,
    pub elf: [u8; 12],
    pub kind: u16,
    pub machine: u16,
    pub version: u32,
    pub entry: usize,
    pub phoff: usize,
    pub shoff: usize,
    pub flags: u32,
    pub ehsize: u16,
    pub phentsize: u16,
    pub phnum: u16,
    pub shentsize: u16,
    pub shnum: u16,
    pub shstrndx: u16,
}

impl ELFHeader {
    pub const fn validate_magic(&self) -> bool {
        self.magic == 0x464C457F
    }
}

// Program section header
#[repr(C)]
pub struct ProgramHeader {
    pub kind: u32,
    pub flags: u32,
    pub off: usize,
    pub vaddr: usize,
    pub paddr: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub align: usize,
}

impl ProgramHeader {
    pub const KIND_LOAD: u32 = 1;
    pub const FLAG_EXEC: u32 = 1;
    pub const FLAG_WRITE: u32 = 2;
    pub const FLAG_READ: u32 = 4;
}
