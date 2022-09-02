use core::{
    ffi::c_void,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    cache::CacheRc,
    config::NINODE,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    log::{initlog, LogGuard},
};

const NDIRECT: usize = 12;
const NINDIRECT: usize = BSIZE / core::mem::size_of::<u32>();

const INODES_PER_BLOCK: usize = BSIZE / core::mem::size_of::<Inode>();
const FSMAGIC: u32 = 0x10203040;

static mut FS: MaybeUninit<FileSystem> = MaybeUninit::uninit();

#[derive(PartialEq, Eq)]
pub struct InodeKey {
    device: usize,
    index: usize,
}

#[repr(C)]
pub struct Inode {
    kind: u16,
    major: u16,
    minor: u16,
    nlink: u16,
    size: u32,
    addrs: [u32; NDIRECT],
    chain: u32,
}

impl Inode {
    pub const fn zeroed() -> Self {
        Self {
            kind: 0,
            major: 0,
            minor: 0,
            nlink: 0,
            size: 0,
            addrs: [0; _],
            chain: 0,
        }
    }
}

pub struct InodeGuard<'a> {
    device: usize,
    inode_index: usize,
    cache_index: usize,
    inode: LockGuard<'a, SleepLock<Inode>>,
}

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

pub struct InodeAllocator {
    inode_start: usize,
    inode_count: usize,
    cache: CacheRc<InodeKey, NINODE>,
    inodes: [SleepLock<Inode>; NINODE],
}

impl InodeAllocator {
    const fn inode_block_at(&self, index: usize) -> usize {
        self.inode_start + index / INODES_PER_BLOCK
    }

    pub const fn new(inode_start: usize, inode_count: usize) -> Self {
        Self {
            inode_start,
            inode_count,
            cache: CacheRc::new(),
            inodes: [const { SleepLock::new(Inode::zeroed()) }; _],
        }
    }

    fn read_inode<L: Lock<Target = Self>>(
        self: &mut LockGuard<L>,
        device: usize,
        index: usize,
    ) -> Option<Inode> {
        let block = buffer::get_with_unlock(device, self.inode_block_at(index), self)?;
        let ptr = unsafe { block.as_ptr::<Inode>().add(index % INODES_PER_BLOCK) };
        let inode = unsafe { ptr.read() };
        buffer::release_with_unlock(block, self);
        Some(inode)
    }

    fn write_inode<L: Lock<Target = Self>>(
        self: &mut LockGuard<L>,
        device: usize,
        index: usize,
        inode: &Inode,
        log: &LogGuard,
    ) {
        let mut block = buffer::get_with_unlock(device, self.inode_block_at(index), self).unwrap();
        let ptr = unsafe { block.as_mut_ptr::<Inode>().add(index % INODES_PER_BLOCK) };
        unsafe { ptr.copy_from(inode, 1) };
        log.write(&block);
        buffer::release_with_unlock(block, self);
    }

    pub fn get<'a, L: Lock<Target = Self>>(
        self: &'a mut LockGuard<L>,
        device: usize,
        index: usize,
    ) -> Option<LockGuard<'a, SleepLock<Inode>>> {
        let (index, is_new) = self.cache.get(InodeKey { device, index })?;

        if is_new {
            let read = self.read_inode(device, index).unwrap();
            *self.inodes[index].lock() = read;
        }

        Some(self.inodes[index].lock())
    }

    // TODO: needs lock?
    pub fn allocate<'a, L: Lock<Target = Self>>(
        self: &'a mut LockGuard<L>,
        device: usize,
        kind: u16,
        log: &LogGuard,
    ) -> Option<LockGuard<'a, SleepLock<Inode>>> {
        for index in 1..(self.inode_count as usize) {
            let mut block =
                buffer::get_with_unlock(device, self.inode_block_at(index), self).unwrap();
            let inode = unsafe {
                block
                    .as_mut_ptr::<Inode>()
                    .add(index % INODES_PER_BLOCK)
                    .as_mut()
                    .unwrap()
            };

            if inode.kind == 0 {
                inode.kind = kind;
                log.write(&mut block);
                buffer::release_with_unlock(block, self);
                return self.get(device, index);
            }

            buffer::release_with_unlock(block, self);
        }

        None
    }

    pub fn deallocate<'a, L: Lock<Target = Self>>(
        self: &'a mut LockGuard<L>,
        mut guard: InodeGuard<'a>,
        log: &LogGuard,
    ) {
        let is_last = self.cache.release(guard.cache_index).unwrap();
        if is_last {
            *guard.inode = Inode::zeroed();
            truncate(&mut guard, log);
            self.write_inode(guard.device, guard.inode_index, &guard.inode, log);
        }
    }
}

pub struct FileSystem {
    superblock: SuperBlock,
    //inode_alloc: SpinLock<InodeAllocator>,
}

impl FileSystem {
    const BITMAP_BITS: usize = BSIZE * (u8::BITS as usize);

    const fn bitmap_at(&self, index: usize) -> usize {
        self.superblock.bmapstart as usize + index / Self::BITMAP_BITS
    }

    pub const fn new(superblock: SuperBlock) -> Self {
        Self {
            /*
            inode_alloc: SpinLock::new(InodeAllocator::new(
                superblock.inodestart as usize,
                superblock.ninodes as usize,
            )),
            */
            superblock,
        }
    }

    pub fn allocate_block(&self, device: usize, log: &LogGuard) -> Option<usize> {
        for bi in (0..(self.superblock.size as usize)).step_by(Self::BITMAP_BITS) {
            let mut bitmap_buf = buffer::get(device, self.bitmap_at(bi)).unwrap();

            let bitmap = unsafe {
                bitmap_buf
                    .as_uninit_mut::<Bitmap<{ Self::BITMAP_BITS }>>()
                    .assume_init_mut()
            };

            match bitmap.allocate() {
                Some(index) if (bi + index) >= self.superblock.size as usize => {
                    bitmap.deallocate(index).unwrap();
                    buffer::release(bitmap_buf);
                    return None;
                }
                Some(index) => {
                    log.write(&mut bitmap_buf);
                    buffer::release(bitmap_buf);

                    let block = bi + index;
                    write_zeros_to_block(device, block, log);
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
                .as_uninit_mut::<Bitmap<{ Self::BITMAP_BITS }>>()
                .assume_init_mut()
        };

        bitmap.deallocate(block % Self::BITMAP_BITS).unwrap();

        log.write(&bitmap_buf);

        buffer::release(bitmap_buf);
    }
}

fn truncate(guard: &mut InodeGuard, log: &LogGuard) {
    for block in guard.inode.addrs.iter_mut() {
        if *block != 0 {
            write_zeros_to_block(guard.device, *block as usize, log);
            *block = 0;
        }
    }

    if guard.inode.chain != 0 {
        let buf = buffer::get(guard.device, guard.inode.chain as usize).unwrap();
        let arr = unsafe { buf.as_uninit::<[u32; NINDIRECT]>().assume_init_ref() };
        for block in arr {
            if *block != 0 {
                write_zeros_to_block(guard.device, *block as usize, log);
            }
        }
        buffer::release(buf);
        write_zeros_to_block(guard.device, guard.inode.chain as usize, log);
        guard.inode.chain = 0;
    }

    guard.inode.size = 0;
    //iupdate();
    todo!()
}

fn write_zeros_to_block(device: usize, block: usize, log: &LogGuard) {
    let mut buf = buffer::get(device, block).unwrap();
    buf.write_zeros();
    log.write(&mut buf);
    buffer::release(buf);
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

    unsafe { FS.write(FileSystem::new(superblock)) };
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
