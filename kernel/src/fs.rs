use core::{
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    cache::CacheRc,
    config::{NINODE, ROOTDEV},
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    log::{self, initlog, LogGuard},
    process::{self, copyin_either, copyout_either},
};

const ROOTINO: usize = 1; // root i-number
const NDIRECT: usize = 12;
const NINDIRECT: usize = BSIZE / core::mem::size_of::<u32>();

const BITMAP_BITS: usize = BSIZE * (u8::BITS as usize);
const MAXFILE: usize = NDIRECT + NINDIRECT;
const INODES_PER_BLOCK: usize = BSIZE / core::mem::size_of::<Inode>();
const FSMAGIC: u32 = 0x10203040;

#[repr(C)]
#[derive(Debug)]
pub struct DirectoryEntry {
    inode_number: u16,
    name: [u8; Self::NAME_LENGTH],
}

impl DirectoryEntry {
    pub const NAME_LENGTH: usize = 14;

    pub const fn unused() -> Self {
        Self {
            inode_number: 0,
            name: [0; _],
        }
    }
}

#[repr(C)]
pub struct Stat {
    device: u32,
    inode: u32,
    kind: u16,
    nlink: u16,
    size: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InodeKey {
    device: usize,
    index: usize,
}

#[repr(C)]
#[derive(Debug)]
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

#[derive(Debug)]
pub struct InodeReference<'i> {
    device: usize,
    inode_number: usize,
    cache_index: usize,
    inode: &'i SleepLock<Inode>,
    is_initialized: &'i AtomicBool,
}

impl<'i> InodeReference<'i> {
    pub fn lock_ro<'r>(&'r self) -> InodeReadOnlyGuard<'r, 'i> {
        let mut guard = InodeReadOnlyGuard {
            device: self.device,
            inode_number: self.inode_number,
            cache_index: self.cache_index,
            inode: self.inode.lock(),
            reflife: PhantomData,
        };

        if self
            .is_initialized
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            guard.initialize();
        }

        guard
    }

    pub fn lock_rw<'r>(&'r self, log: &'r LogGuard<'r>) -> InodeReadWriteGuard<'r, 'i> {
        InodeReadWriteGuard {
            guard: self.lock_ro(),
            log,
        }
    }

    pub fn drop_with_log(self, log: &LogGuard) {
        INODE_ALLOC.lock().release(&self, &log);
        core::mem::forget(self);
    }
}

impl<'a> Clone for InodeReference<'a> {
    fn clone(&self) -> Self {
        INODE_ALLOC.lock().duplicate(self)
    }
}

impl<'a> Drop for InodeReference<'a> {
    fn drop(&mut self) {
        let log = log::start();
        let mut alloc = INODE_ALLOC.lock();
        alloc.release(self, &log);
        drop(alloc);
        drop(log);
    }
}

impl<'r, 'i> Drop for InodeReadOnlyGuard<'r, 'i> {
    fn drop(&mut self) {}
}

impl<'r, 'i> Drop for InodeReadWriteGuard<'r, 'i> {
    fn drop(&mut self) {}
}

#[derive(Debug)]
pub struct InodeReadOnlyGuard<'r, 'i> {
    device: usize,
    inode_number: usize,
    cache_index: usize,
    inode: LockGuard<'i, SleepLock<Inode>>,
    reflife: PhantomData<&'r ()>,
}

#[derive(Debug)]
pub struct InodeReadWriteGuard<'r, 'i> {
    guard: InodeReadOnlyGuard<'r, 'i>,
    log: &'r LogGuard<'r>,
}

impl<'r, 'i> InodeReadWriteGuard<'r, 'i> {
    pub fn copy_from<T>(
        &mut self,
        is_src_user: bool,
        src: usize,
        offset: usize,
        count: usize,
    ) -> Result<usize, ()> {
        let type_size = core::mem::size_of::<T>();
        let write_size = type_size * count;

        if offset > self.inode.size as usize || offset.checked_add(write_size).is_none() {
            return Err(());
        }

        if offset + write_size > MAXFILE * BSIZE {
            return Err(());
        }

        let mut wrote = 0;
        while wrote < write_size {
            let offset = offset + wrote;

            // TODO: REMOVE THIS HACK:
            let log = unsafe { <*const _>::as_ref(self.log).unwrap() };
            let block = self.offset_to_block(offset, Some(log)).unwrap();

            let mut buf = buffer::get(self.device_number(), block).unwrap();

            let offset_in_block = offset % buf.size();
            let len = (write_size - wrote).min(BSIZE - offset_in_block);
            let dst = unsafe {
                core::slice::from_raw_parts_mut(buf.as_mut_ptr::<u8>().add(offset_in_block), len)
            };

            let is_copied = unsafe { copyin_either(dst, is_src_user, src + wrote) };
            if is_copied {
                self.log.write(&buf);
                buffer::release(buf);
            } else {
                buffer::release(buf);
                break;
            }

            wrote += len;
        }

        self.inode.size = self.inode.size.max((offset + wrote) as u32);
        self.update();

        Ok(wrote)
    }

    pub fn write<T>(&mut self, value: T, offset: usize) -> Result<(), ()> {
        let wrote = self.copy_from::<T>(false, <*const T>::addr(&value), offset, 1)?;
        if wrote == core::mem::size_of::<T>() {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn update(&mut self) {
        let inode_start = unsafe { SUPERBLOCK.inodestart as usize };

        let block_index = inode_start + self.inode_number / INODES_PER_BLOCK;
        let in_block_index = self.inode_number % INODES_PER_BLOCK;

        let mut block = buffer::get(self.device, block_index).unwrap();
        unsafe {
            block
                .as_mut_ptr::<Inode>()
                .add(in_block_index)
                .copy_from(&*self.inode, 1)
        };

        self.log.write(&block);
        buffer::release(block);
    }

    pub fn truncate(&mut self) {
        for addr in self.inode.addrs {
            if addr != 0 {
                unsafe { deallocate_block(self.device_number(), addr as usize, self.log) };
            }
        }
        self.inode.addrs.fill(0);

        if self.inode.chain != 0 {
            let buf = buffer::get(self.device_number(), self.inode.chain as usize).unwrap();
            let addrs = unsafe { core::slice::from_raw_parts(buf.as_ptr::<u32>(), NINDIRECT) };
            for addr in addrs {
                if *addr != 0 {
                    unsafe {
                        deallocate_block(self.device_number(), *addr as usize, self.log);
                    }
                }
            }
            buffer::release(buf);
            unsafe { deallocate_block(self.device_number(), self.inode.chain as usize, self.log) };
            self.inode.chain = 0;
        }

        self.inode.size = 0;
        self.update();
    }

    pub fn link(&mut self, name: &str, inode_number: usize) -> Result<(), ()> {
        if !self.is_directory() {
            return Err(());
        }

        if self.lookup(name).is_some() {
            return Err(());
        }

        let new_entry = {
            let mut entry = DirectoryEntry {
                inode_number: inode_number as u16,
                name: [0; _],
            };
            entry.name[..name.len()].copy_from_slice(name.as_bytes());
            entry
        };

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for offset in (0..self.size()).step_by(entry_size) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                self.write(new_entry, offset).unwrap();
                return Ok(());
            }
        }

        let insert_offset = self.size() - self.size() % entry_size;
        self.write(new_entry, insert_offset).or(Err(()))
    }
}

impl<'r, 'i> Deref for InodeReadWriteGuard<'r, 'i> {
    type Target = InodeReadOnlyGuard<'r, 'i>;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'r, 'i> DerefMut for InodeReadWriteGuard<'r, 'i> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<'r, 'i> InodeReadOnlyGuard<'r, 'i> {
    pub fn is_directory(&self) -> bool {
        self.inode.kind == 1
    }

    pub fn is_file(&self) -> bool {
        self.inode.kind == 2
    }

    pub fn is_device(&self) -> bool {
        self.inode.kind == 3
    }

    pub fn counf_of_link(&self) -> usize {
        self.inode.nlink as usize
    }

    pub fn increment_link(&mut self) {
        self.inode.nlink += 1;
    }

    pub fn decrement_link(&mut self) {
        self.inode.nlink -= 1;
    }

    pub fn size(&self) -> usize {
        self.inode.size as usize
    }

    pub fn device_number(&self) -> usize {
        self.device
    }

    pub fn inode_number(&self) -> usize {
        self.inode_number
    }

    pub fn device_major(&self) -> Option<usize> {
        if self.is_device() {
            Some(self.inode.major as usize)
        } else {
            None
        }
    }

    pub fn device_minor(&self) -> Option<usize> {
        if self.is_device() {
            Some(self.inode.minor as usize)
        } else {
            None
        }
    }

    pub fn stat(&self) -> Stat {
        Stat {
            device: self.device as u32,
            inode: self.inode_number as u32,
            kind: self.inode.kind,
            nlink: self.inode.nlink,
            size: self.inode.size as usize,
        }
    }

    fn initialize(&mut self) {
        let inode_start = unsafe { SUPERBLOCK.inodestart as usize };

        let block_index = inode_start + self.inode_number / INODES_PER_BLOCK;
        let in_block_index = self.inode_number % INODES_PER_BLOCK;

        let block = buffer::get(self.device_number(), block_index).unwrap();
        let inode = unsafe { block.as_ptr::<Inode>().add(in_block_index).read() };
        buffer::release(block);
        *self.inode = inode;

        assert!(self.inode.kind != 0);
    }

    fn offset_to_block(&mut self, offset: usize, log: Option<&LogGuard>) -> Option<usize> {
        let index = offset / BSIZE;
        let device = self.device_number();

        if let Some(addr) = self.inode.addrs.get_mut(index) {
            if *addr == 0 {
                let Some(allocated) = (unsafe { allocate_block(device, log?) }) else {
                    return None;
                };
                *addr = allocated as u32;
            }
            return Some(*addr as usize);
        }

        if NDIRECT <= index && index < NDIRECT + NINDIRECT {
            let index = index - NDIRECT;

            if self.inode.chain == 0 {
                let Some(allocated) = (unsafe { allocate_block(device, log?) }) else {
                    return None;
                };
                self.inode.chain = allocated as u32;
            }

            let mut buf = buffer::get(device, self.inode.chain as usize)?;
            let addrs =
                unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr::<u32>(), NINDIRECT) };
            if let Some(addr) = addrs.get_mut(index) {
                if *addr == 0 {
                    let Some(log) = log else {
                        buffer::release(buf);
                        return None;
                    };
                    let Some(allocated) = (unsafe { allocate_block(device, log) }) else {
                        buffer::release(buf);
                        return None;
                    };
                    *addr = allocated as u32;
                    log.write(&buf);
                }
                buffer::release(buf);
                return Some(*addr as usize);
            }
            buffer::release(buf);
        }
        None
    }

    pub fn copy_to<T>(
        &mut self,
        is_dst_user: bool,
        dst: usize,
        offset: usize,
        count: usize,
    ) -> Result<usize, ()> {
        let type_size = core::mem::size_of::<T>();
        let read_size = type_size * count;

        if offset > self.size() || offset.checked_add(read_size).is_none() {
            return Ok(0);
        }

        let n = if offset + read_size > self.size() {
            self.size() - offset
        } else {
            read_size
        };

        let mut read = 0;
        while read < n {
            let offset = offset + read;

            let block = self.offset_to_block(offset, None).unwrap();

            let buf = buffer::get(self.device_number(), block).unwrap();

            let offset_in_block = offset % buf.size();
            let len = (n - read).min(buf.size() - offset_in_block);
            let bytes = unsafe {
                core::slice::from_raw_parts(buf.as_ptr::<u8>().add(offset_in_block), len)
            };

            let is_copied = unsafe { copyout_either(is_dst_user, dst + read, bytes) };
            buffer::release(buf);

            if !is_copied {
                return Err(());
            }

            read += len;
        }

        Ok(read)
    }

    pub fn read<T>(&mut self, offset: usize) -> Result<T, ()> {
        let mut value = MaybeUninit::<T>::uninit();
        let read = self.copy_to::<T>(false, value.as_mut_ptr().addr(), offset, 1)?;
        if read == core::mem::size_of::<T>() {
            Ok(unsafe { value.assume_init() })
        } else {
            Err(())
        }
    }

    pub fn lookup(&mut self, name: &str) -> Option<(InodeReference<'i>, usize)> {
        if !self.is_directory() {
            return None;
        }

        for offset in (0..self.size()).step_by(core::mem::size_of::<DirectoryEntry>()) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                continue;
            }

            if name.len() > DirectoryEntry::NAME_LENGTH {
                return None;
            }

            let mut cmp = [0; DirectoryEntry::NAME_LENGTH];
            cmp[..name.len()].copy_from_slice(name.as_bytes());

            if cmp == entry.name {
                return INODE_ALLOC
                    .lock()
                    .get(self.device_number(), entry.inode_number as usize)
                    .map(|inode| (inode, offset));
            }
        }

        None
    }

    pub fn is_empty(&mut self) -> Option<bool> {
        if !self.is_directory() {
            return None;
        }

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for off in ((2 * entry_size)..(self.inode.size as usize)).step_by(entry_size) {
            let entry = self.read::<DirectoryEntry>(off).unwrap();
            if entry.inode_number != 0 {
                return Some(false);
            }
        }
        Some(true)
    }
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
}

pub struct InodeAllocator<const N: usize> {
    cache: CacheRc<InodeKey, N>,
    inodes: [SleepLock<Inode>; N],
    is_initialized: [AtomicBool; N],
}

impl<const N: usize> InodeAllocator<N> {
    fn inode_block_at(&self, index: usize) -> usize {
        unsafe { SUPERBLOCK.inodestart as usize + index / INODES_PER_BLOCK }
    }

    pub const fn new() -> Self {
        Self {
            cache: CacheRc::new(),
            inodes: [const { SleepLock::new(Inode::zeroed()) }; _],
            is_initialized: [const { AtomicBool::new(false) }; _],
        }
    }

    fn allocate(
        self: &mut LockGuard<SpinLock<Self>>,
        device: usize,
        kind: u16,
        log: &LogGuard,
    ) -> Result<usize, ()> {
        for inum in 1..(unsafe { SUPERBLOCK.ninodes as usize }) {
            let block_index = self.inode_block_at(inum);
            let in_block_index = inum % INODES_PER_BLOCK;

            let mut block = buffer::get_with_unlock(device, block_index, self).unwrap();
            let inode = unsafe {
                block
                    .as_mut_ptr::<Inode>()
                    .add(in_block_index)
                    .as_mut()
                    .unwrap()
            };

            if inode.kind == 0 {
                inode.kind = kind;
                log.write(&mut block);
                buffer::release_with_unlock(block, self);

                return Ok(inum);
            }

            buffer::release_with_unlock(block, self);
        }

        Err(())
    }

    // TODO:
    pub fn get(
        self: &mut LockGuard<'static, SpinLock<Self>>,
        device: usize,
        inode_number: usize,
    ) -> Option<InodeReference<'static>> {
        let (cache_index, is_new) = self.cache.get(InodeKey {
            device,
            index: inode_number,
        })?;

        if is_new {
            self.is_initialized[cache_index].store(false, Ordering::SeqCst);
        }

        Some(InodeReference {
            device,
            inode_number,
            cache_index,
            inode: unsafe { <*const _>::as_ref(&self.inodes[cache_index]).unwrap() },
            is_initialized: unsafe {
                <*const _>::as_ref(&self.is_initialized[cache_index]).unwrap()
            },
        })
    }

    pub fn duplicate<'a>(
        self: &mut LockGuard<'a, SpinLock<Self>>,
        inode: &InodeReference<'a>,
    ) -> InodeReference<'a> {
        self.cache
            .get(InodeKey {
                device: inode.device,
                index: inode.inode_number,
            })
            .unwrap();
        InodeReference { ..*inode }
    }

    pub fn release(
        self: &mut LockGuard<SpinLock<Self>>,
        inode_ref: &InodeReference,
        log: &LogGuard,
    ) {
        let refcnt = self.cache.reference_count(inode_ref.cache_index).unwrap();
        let mut inode = inode_ref.lock_rw(log);
        if refcnt == 1
            && inode_ref.is_initialized.load(Ordering::SeqCst)
            && inode.counf_of_link() == 0
        {
            // TODO: FIX
            assert!(inode.inode.nlink == 0);
            Lock::unlock_temporarily(self, move || {
                inode.truncate();
                inode.inode.kind = 0;
                inode.update();
                inode_ref.is_initialized.store(false, Ordering::SeqCst);
                drop(inode);
            });
        }
        self.cache.release(inode_ref.cache_index).unwrap();
    }
}

static mut SUPERBLOCK: SuperBlock = SuperBlock::zeroed();

fn bitmap_at(index: usize) -> usize {
    unsafe { SUPERBLOCK.bmapstart as usize + index / BITMAP_BITS }
}

unsafe fn allocate_block(device: usize, log: &LogGuard) -> Option<usize> {
    for bi in (0..(SUPERBLOCK.size as usize)).step_by(BITMAP_BITS) {
        let mut bitmap_buf = buffer::get(device, bitmap_at(bi)).unwrap();

        let bitmap = unsafe {
            bitmap_buf
                .as_uninit_mut::<Bitmap<{ BITMAP_BITS }>>()
                .assume_init_mut()
        };

        match bitmap.allocate() {
            Some(index) if (bi + index) >= SUPERBLOCK.size as usize => {
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

unsafe fn deallocate_block(device: usize, block: usize, log: &LogGuard) {
    let mut bitmap_buf = buffer::get(device, bitmap_at(block)).unwrap();
    let bitmap = unsafe {
        bitmap_buf
            .as_uninit_mut::<Bitmap<{ BITMAP_BITS }>>()
            .assume_init_mut()
    };

    bitmap.deallocate(block % BITMAP_BITS).unwrap();
    assert!(bitmap.get(block % BITMAP_BITS) == Some(false));

    log.write(&bitmap_buf);

    buffer::release(bitmap_buf);
}

static INODE_ALLOC: SpinLock<InodeAllocator<NINODE>> = SpinLock::new(InodeAllocator::new());

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
    unsafe { SUPERBLOCK = superblock };
}

pub trait InodeOps<'a> {
    fn create(
        &self,
        path: &str,
        kind: u16,
        major: u16,
        minor: u16,
    ) -> Result<InodeReference<'a>, ()>;
}

impl<'a> InodeOps<'a> for LogGuard<'a> {
    fn create(
        &self,
        path: &str,
        kind: u16,
        major: u16,
        minor: u16,
    ) -> Result<InodeReference<'a>, ()> {
        let (dir, name) = search_parent_inode(path).ok_or(())?;
        let mut dir = dir.lock_rw(self);

        match dir.lookup(name) {
            // TODO: T_FILE
            Some((inode_ref, _)) => {
                drop(dir);
                let inode = inode_ref.lock_ro();
                return if kind == 2 && (inode.is_file() || inode.is_device()) {
                    drop(inode);
                    Ok(inode_ref)
                } else {
                    Err(())
                };
            }
            None => {
                let inode_number = INODE_ALLOC
                    .lock()
                    .allocate(dir.device_number(), kind, self)?;

                let inode_ref = get(dir.device_number(), inode_number).unwrap();
                let mut inode = inode_ref.lock_rw(self);
                inode.inode.major = major;
                inode.inode.minor = minor;
                inode.inode.nlink = 1;
                inode.update();

                let bad = |mut inode: InodeReadWriteGuard| {
                    inode.inode.nlink = 0;
                    inode.update();
                    Err(())
                };

                // TODO: T_DIR
                if kind == 1 {
                    if inode.link(".", inode.inode_number()).is_err()
                        || inode.link("..", dir.inode_number()).is_err()
                    {
                        return bad(inode);
                    }
                }

                if dir.link(name, inode.inode_number()).is_err() {
                    return bad(inode);
                }

                // TODO: T_DIR
                if kind == 1 {
                    dir.increment_link();
                    dir.update();
                }

                drop(inode);
                drop(dir);
                Ok(inode_ref)
            }
        }
    }
}

pub fn link(new: &str, old: &str) -> Result<(), ()> {
    let log = log::start();
    let inode_ref = search_inode(old).ok_or(())?;
    let mut ip = inode_ref.lock_rw(&log);

    if ip.is_directory() {
        return Err(());
    }

    let dev = ip.device_number();
    let inum = ip.inode_number();
    ip.increment_link();
    ip.update();
    drop(ip);

    let bad = |inode_ref: InodeReference, log| {
        let mut inode = inode_ref.lock_rw(&log);
        inode.decrement_link();
        inode.update();
        drop(inode);
        drop(inode_ref);
        drop(log);
        Err(())
    };

    let Some((dir, name)) = search_parent_inode(new) else {
        return bad(inode_ref, log);
    };
    let mut dir = dir.lock_rw(&log);

    if dir.device_number() != dev {
        drop(dir);
        return bad(inode_ref, log);
    }

    if dir.link(name, inum).is_err() {
        drop(dir);
        return bad(inode_ref, log);
    }

    drop(dir);
    drop(log);
    Ok(())
}

pub fn unlink(path: &str) -> Result<(), ()> {
    let log = log::start();

    let (dir, name) = search_parent_inode(path).ok_or(())?;
    let mut dir = dir.lock_rw(&log);

    if name == "." || name == ".." {
        return Err(());
    }

    let (ip, offset) = dir.lookup(name).ok_or(())?;
    let mut ip = ip.lock_rw(&log);
    assert!(ip.counf_of_link() > 0);

    if ip.is_empty() == Some(false) {
        return Err(());
    }

    let entry = DirectoryEntry::unused();
    dir.write(entry, offset).unwrap();

    if ip.is_directory() {
        dir.decrement_link();
        dir.update();
    }
    drop(dir);

    ip.decrement_link();
    ip.update();
    drop(ip);
    drop(log);
    Ok(())
}

pub fn make_directory(path: &str) -> Result<(), ()> {
    let log = log::start();
    log.create(path, 1, 0, 0)?; // TODO: 1 == T_DIR
    Ok(())
}

pub fn make_special_file(path: &str, major: u16, minor: u16) -> Result<(), ()> {
    let log = log::start();
    log.create(path, 3, major, minor)?; // TODO: 3 == T_DEVICE
    Ok(())
}

pub fn get(device: usize, inode: usize) -> Option<InodeReference<'static>> {
    INODE_ALLOC.lock().get(device, inode)
}

pub fn search_inode(path: &str) -> Option<InodeReference<'static>> {
    let mut inode_ref = if path.starts_with("/") {
        get(ROOTDEV, ROOTINO).unwrap()
    } else {
        process::context().unwrap().cwd.as_ref().cloned().unwrap()
    };

    for element in path.split("/") {
        if element.is_empty() {
            continue;
        }

        let mut inode = inode_ref.lock_ro();
        if !inode.is_directory() {
            return None;
        }

        match inode.lookup(element) {
            Some((next, _)) => {
                drop(inode);
                inode_ref = next;
            }
            _ => return None,
        }
    }

    Some(inode_ref)
}

pub fn search_parent_inode(path: &str) -> Option<(InodeReference<'static>, &str)> {
    let mut inode_ref = if path.starts_with("/") {
        get(ROOTDEV, ROOTINO).unwrap()
    } else {
        process::context().unwrap().cwd.as_ref().cloned().unwrap()
    };

    let mut iter = path.split("/").peekable();
    while let Some(element) = iter.next() {
        if element.is_empty() {
            continue;
        }

        let mut inode = inode_ref.lock_ro();
        if !inode.is_directory() {
            return None;
        }

        if iter.peek().is_none() {
            drop(inode);
            return Some((inode_ref, element));
        }

        match inode.lookup(element) {
            Some((next, _)) => {
                drop(inode);
                inode_ref = next;
            }
            _ => return None,
        }
    }

    None
}
