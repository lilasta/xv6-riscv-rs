use crate::filesystem::buffer::BSIZE;
use crate::fs::Inode;

pub mod buffer;
pub mod log;
pub mod superblock;

pub const BITMAP_BITS: usize = BSIZE * (u8::BITS as usize);
pub const INODES_PER_BLOCK: usize = BSIZE / core::mem::size_of::<Inode>();
