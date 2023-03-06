use crate::filesystem::buffer::BSIZE;
use crate::filesystem::inode::Inode;

pub mod buffer;
pub mod cache;
pub mod directory_entry;
pub mod inode;
pub mod log;
pub mod stat;
pub mod superblock;

pub const BITMAP_BITS: usize = BSIZE * (u8::BITS as usize);
pub const INODES_PER_BLOCK: usize = BSIZE / core::mem::size_of::<Inode>();

pub const ROOTINO: usize = 1; // root i-number
pub const NDIRECT: usize = 12;
pub const NINDIRECT: usize = BSIZE / core::mem::size_of::<u32>();

pub const MAXFILE: usize = NDIRECT + NINDIRECT;
pub const FSMAGIC: u32 = 0x10203040;
