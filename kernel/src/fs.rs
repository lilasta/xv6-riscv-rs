use core::{
    ffi::c_void,
    ops::{Deref, DerefMut},
};

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    log::LogGuard,
};

const FSMAGIC: u32 = 0x10203040;

#[repr(C)]
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

pub struct FileSystem {
    superblock: SuperBlock,
}

impl FileSystem {
    const BITMAP_SIZE: usize = BSIZE * 8;

    const fn bitmap_at(&self, index: usize) -> usize {
        self.superblock.bmapstart as usize + index
    }

    pub fn allocate_block(&self, device: usize, log: &LogGuard) -> Option<usize> {
        for bitmap_at in (0..(self.superblock.size as usize)).step_by(Self::BITMAP_SIZE) {
            let mut bitmap_buf = buffer::get(device, self.bitmap_at(bitmap_at)).unwrap();

            let bitmap = unsafe {
                bitmap_buf
                    .as_uninit_mut::<Bitmap<{ Self::BITMAP_SIZE }>>()
                    .assume_init_mut()
            };

            match bitmap.allocate() {
                Some(index) if (bitmap_at + index) >= self.superblock.size as usize => {
                    bitmap.deallocate(index).unwrap();
                    continue;
                }
                Some(index) => {
                    log.write(&mut bitmap_buf);

                    let block = bitmap_at + index;
                    {
                        let mut buf = buffer::get(device, block).unwrap();
                        buf.write_zeros();
                        buffer::release(buf);
                    }
                    buffer::release(bitmap_buf);
                    return Some(block);
                }
                None => {
                    continue;
                }
            }
        }
        None
    }
}

fn fsinit(device: usize) -> FileSystem {
    fn read_superblock(device: usize) -> Option<SuperBlock> {
        let buf = buffer::get(device, 1)?;
        let val = unsafe { buf.as_uninit().assume_init_read() };
        buffer::release(buf);
        Some(val)
    }

    let superblock = read_superblock(device).unwrap();
    assert!(superblock.magic == FSMAGIC);
    // initlog(device, superblock);
    todo!()
}

extern "C" {
    fn ilock(ip: *mut c_void);
    fn iunlockput(ip: *mut c_void);
}

pub struct InodeLockGuard {
    inode: *mut c_void,
}

impl InodeLockGuard {
    pub fn new(inode: *mut c_void) -> Self {
        unsafe { ilock(inode) };
        Self { inode }
    }
}

impl Deref for InodeLockGuard {
    type Target = *mut c_void;

    fn deref(&self) -> &Self::Target {
        &self.inode
    }
}

impl DerefMut for InodeLockGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inode
    }
}

impl Drop for InodeLockGuard {
    fn drop(&mut self) {
        unsafe { iunlockput(self.inode) };
    }
}
