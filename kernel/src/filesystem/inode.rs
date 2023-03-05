use crate::filesystem::NDIRECT;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeKind {
    Unused = 0,
    Directory = 1,
    File = 2,
    Device = 3,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct Inode {
    pub kind: InodeKind,
    pub major: u16,
    pub minor: u16,
    pub nlink: u16,
    pub size: u32,
    pub addrs: [u32; NDIRECT],
    pub chain: u32,
}

impl Inode {
    pub const fn zeroed() -> Self {
        Self {
            kind: InodeKind::Unused,
            major: 0,
            minor: 0,
            nlink: 0,
            size: 0,
            addrs: [0; _],
            chain: 0,
        }
    }
}
