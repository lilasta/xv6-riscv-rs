use core::{
    ffi::c_void,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    config::NINODE,
    lock::{sleep::SleepLock, spin::SpinLock, Lock},
    log::{initlog, LogGuard},
};

const FSMAGIC: u32 = 0x10203040;

static mut FS: MaybeUninit<FileSystem> = MaybeUninit::uninit();

struct Inode {}

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
    //inodes: SpinLock<[SleepLock<Inode>; NINODE]>,
}

impl FileSystem {
    const BITMAP_SIZE: usize = BSIZE * 8;

    const fn bitmap_at(&self, index: usize) -> usize {
        self.superblock.bmapstart as usize + index / Self::BITMAP_SIZE
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
                    buffer::release(bitmap_buf);
                    return None;
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
                    buffer::release(bitmap_buf);
                    continue;
                }
            }
        }
        None
    }

    pub fn deallocate_block(&self, device: usize, block: usize, log: &LogGuard) {
        let mut bitmap_buf = buffer::get(device, self.bitmap_at(block)).unwrap();
        let bitmap = unsafe {
            bitmap_buf
                .as_uninit_mut::<Bitmap<{ Self::BITMAP_SIZE }>>()
                .assume_init_mut()
        };

        bitmap.deallocate(block % Self::BITMAP_SIZE).unwrap();

        log.write(&bitmap_buf);

        buffer::release(bitmap_buf);
    }
}

#[no_mangle]
extern "C" fn fsinit(device: u32) {
    fn read_superblock(device: usize) -> Option<SuperBlock> {
        let buf = buffer::get(device, 1)?;
        let val = unsafe { buf.as_uninit().assume_init_read() };
        buffer::release(buf);
        Some(val)
    }

    let superblock = read_superblock(device as usize).unwrap();
    assert!(superblock.magic == FSMAGIC);
    unsafe { initlog(device as u32, &superblock) };

    unsafe { FS.write(FileSystem { superblock }) };
}

#[no_mangle]
extern "C" fn sb() -> *mut SuperBlock {
    unsafe { &mut FS.assume_init_mut().superblock }
}

#[no_mangle]
extern "C" fn balloc(dev: u32) -> u32 {
    let guard = LogGuard;
    let ret = unsafe {
        FS.assume_init_ref()
            .allocate_block(dev as _, &guard)
            .unwrap_or(0)
    };
    core::mem::forget(guard);
    ret as u32
}

#[no_mangle]
extern "C" fn bfree(dev: u32, block: u32) {
    let guard = LogGuard;
    unsafe {
        FS.assume_init_ref()
            .deallocate_block(dev as _, block as _, &guard)
    };
    core::mem::forget(guard);
}

extern "C" {
    fn ilock(ip: *mut c_void);
    fn iunlockput(ip: *mut c_void);
}

const NDIRECT: usize = 12;

#[repr(C)]
struct DiskInode {
    kind: u16,
    major: u16,
    minor: u16,
    nlink: u16,
    size: u32,
    addrs: [u32; NDIRECT + 1],
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
