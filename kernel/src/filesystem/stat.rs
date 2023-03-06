use crate::filesystem::inode::InodeKind;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct Stat {
    pub device: u32,
    pub inode: u32,
    pub kind: InodeKind,
    pub nlink: u16,
    pub size: usize,
}
