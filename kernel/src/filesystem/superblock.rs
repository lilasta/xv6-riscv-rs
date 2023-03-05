use crate::filesystem::{BITMAP_BITS, INODES_PER_BLOCK};

#[repr(C)]
#[derive(Clone)]
pub struct SuperBlock {
    pub magic: u32,      // Must be FSMAGIC
    pub size: u32,       // Size of file system image (blocks)
    pub nblocks: u32,    // Number of data blocks
    pub ninodes: u32,    // Number of inodes.
    pub nlog: u32,       // Number of log blocks
    pub logstart: u32,   // Block number of first log block
    pub inodestart: u32, // Block number of first inode block
    pub bmapstart: u32,  // Block number of first free map block
}

impl SuperBlock {
    pub const fn zeroed() -> Self {
        Self {
            magic: 0,
            size: 0,
            nblocks: 0,
            ninodes: 0,
            nlog: 0,
            logstart: 0,
            inodestart: 0,
            bmapstart: 0,
        }
    }

    pub const fn inode_block_at(&self, index: usize) -> usize {
        self.inodestart as usize + index / INODES_PER_BLOCK
    }

    pub const fn bitmap_at(&self, index: usize) -> usize {
        self.bmapstart as usize + index / BITMAP_BITS
    }
}
