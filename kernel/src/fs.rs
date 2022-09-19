use core::{
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
};

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    cache::CacheRc,
    config::{NINODE, ROOTDEV},
    log::{self, LogGuard},
    process::{self, copyin_either, copyout_either},
    sleeplock::{SleepLock, SleepLockGuard},
    spinlock::SpinLock,
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
#[derive(Debug, Clone)]
pub struct Stat {
    device: u32,
    inode: u32,
    kind: InodeKind,
    nlink: u16,
    size: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InodeKey {
    device: usize,
    index: usize,
}

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
    kind: InodeKind,
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

#[derive(Debug)]
pub struct InodeEntry {
    inode: Inode,
    device: usize,
    inode_number: usize,
    is_initialized: bool,
}

impl InodeEntry {
    pub const fn zeroed() -> Self {
        Self {
            inode: Inode::zeroed(),
            device: 0,
            inode_number: 0,
            is_initialized: false,
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(self.inode.kind, InodeKind::Directory)
    }

    pub fn is_file(&self) -> bool {
        matches!(self.inode.kind, InodeKind::File)
    }

    pub fn is_device(&self) -> bool {
        matches!(self.inode.kind, InodeKind::Device)
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

        let mut block = buffer::get(self.device, block_index).unwrap();
        self.inode = unsafe { block.read_array::<Inode>()[in_block_index].clone() };

        assert!(matches!(self.inode.kind, InodeKind::Unused) == false);
    }

    fn offset_to_block(&mut self, offset: usize, log: Option<&LogGuard>) -> Option<usize> {
        let index = offset / BSIZE;

        if let Some(addr) = self.inode.addrs.get_mut(index) {
            if *addr == 0 {
                *addr = unsafe { SUPERBLOCK.allocate_block(self.device, log?)? as u32 };
            }
            return Some(*addr as usize);
        }

        if NDIRECT <= index && index < NDIRECT + NINDIRECT {
            let index = index - NDIRECT;

            if self.inode.chain == 0 {
                self.inode.chain = unsafe { SUPERBLOCK.allocate_block(self.device, log?)? as u32 };
            }

            let mut buf = buffer::get(self.device, self.inode.chain as usize)?;
            let addrs = unsafe { buf.read::<[u32; NINDIRECT]>() };
            let addr = if addrs[index] == 0 {
                let log = log?;
                let allocated = unsafe { SUPERBLOCK.allocate_block(self.device, log)? };
                addrs[index] = allocated as u32;
                log.write(&buf);
                allocated
            } else {
                addrs[index] as usize
            };
            return Some(addr);
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

            let block = self.offset_to_block(offset, None).ok_or(())?;

            let mut buf = buffer::get(self.device, block).unwrap();

            let offset_in_block = offset % buf.size();
            let len = (n - read).min(buf.size() - offset_in_block);

            let src = unsafe { &mut buf.read_array::<u8>()[offset_in_block..][..len] };
            let is_copied = unsafe { copyout_either(is_dst_user, dst + read, src) };

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

    pub fn lookup(&mut self, name: &str) -> Option<(InodeReference<'static>, usize)> {
        if !self.is_directory() {
            return None;
        }

        for offset in (0..self.size()).step_by(core::mem::size_of::<DirectoryEntry>()) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                continue;
            }

            let mut cmp = [0; DirectoryEntry::NAME_LENGTH];
            let len = name.len().min(DirectoryEntry::NAME_LENGTH);
            cmp[..len].copy_from_slice(&name.as_bytes()[..len]);

            if cmp == entry.name {
                return INODE_ALLOC
                    .get(self.device, entry.inode_number as usize)
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

    pub fn copy_from<T>(
        &mut self,
        is_src_user: bool,
        src: usize,
        offset: usize,
        count: usize,
        log: &LogGuard,
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

            let block = self.offset_to_block(offset, Some(log)).ok_or(())?;

            let mut buf = buffer::get(self.device, block).unwrap();
            let offset_in_block = offset % buf.size();
            let len = (write_size - wrote).min(BSIZE - offset_in_block);
            let is_copied = unsafe {
                let dst = &mut buf.read_array::<u8>()[offset_in_block..][..len];
                copyin_either(dst, is_src_user, src + wrote)
            };
            if is_copied {
                log.write(&buf);
            } else {
                break;
            }

            wrote += len;
        }

        self.inode.size = self.inode.size.max((offset + wrote) as u32);
        self.update(log);

        Ok(wrote)
    }

    pub fn write<T>(&mut self, value: T, offset: usize, log: &LogGuard) -> Result<(), ()> {
        let wrote = self.copy_from::<T>(false, <*const T>::addr(&value), offset, 1, log)?;
        if wrote == core::mem::size_of::<T>() {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn link(&mut self, name: &str, inode_number: usize, log: &LogGuard) -> Result<(), ()> {
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

            let len = name.len().min(DirectoryEntry::NAME_LENGTH);
            entry.name[..len].copy_from_slice(&name.as_bytes()[..len]);
            entry
        };

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for offset in (0..self.size()).step_by(entry_size) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                self.write(new_entry, offset, log).unwrap();
                return Ok(());
            }
        }

        let insert_offset = self.size() - self.size() % entry_size;
        self.write(new_entry, insert_offset, log).or(Err(()))
    }

    pub fn update(&mut self, log: &LogGuard) {
        let inode_start = unsafe { SUPERBLOCK.inodestart as usize };

        let block_index = inode_start + self.inode_number / INODES_PER_BLOCK;
        let in_block_index = self.inode_number % INODES_PER_BLOCK;

        let mut block = buffer::get(self.device, block_index).unwrap();
        let inodes = unsafe { block.read_array::<Inode>() };
        inodes[in_block_index] = self.inode.clone();
        log.write(&block);
    }

    pub fn truncate(&mut self, log: &LogGuard) {
        for addr in self.inode.addrs {
            if addr != 0 {
                unsafe { SUPERBLOCK.deallocate_block(self.device, addr as usize, log) };
            }
        }
        self.inode.addrs.fill(0);

        if self.inode.chain != 0 {
            let mut buf = buffer::get(self.device, self.inode.chain as usize).unwrap();
            let addrs = unsafe { buf.read::<[u32; NINDIRECT]>() };
            for addr in addrs {
                if *addr != 0 {
                    unsafe { SUPERBLOCK.deallocate_block(self.device, *addr as usize, log) };
                }
            }
            unsafe { SUPERBLOCK.deallocate_block(self.device, self.inode.chain as usize, log) };
            self.inode.chain = 0;
        }

        self.inode.size = 0;
        self.update(log);
    }

    pub fn as_ref(&self) -> InodeReference<'static> {
        INODE_ALLOC.get(self.device, self.inode_number).unwrap()
    }
}

#[derive(Debug)]
pub struct InodeReference<'a> {
    cache_index: usize,
    entry: &'a SleepLock<InodeEntry>,
}

impl<'a> InodeReference<'a> {
    pub fn lock(&self) -> InodeGuard<'a> {
        let mut guard = InodeGuard {
            cache_index: self.cache_index,
            entry: ManuallyDrop::new(self.entry.lock()),
        };

        INODE_ALLOC.duplicate(self.cache_index);

        if !guard.is_initialized {
            guard.initialize();
            guard.is_initialized = true;
        }

        guard
    }
}

impl<'a> Clone for InodeReference<'a> {
    fn clone(&self) -> Self {
        INODE_ALLOC.duplicate(self.cache_index);
        Self { ..*self }
    }
}

impl<'a> Drop for InodeReference<'a> {
    fn drop(&mut self) {
        let log = log::start();
        INODE_ALLOC.release(self.cache_index, &log);
    }
}

#[derive(Debug)]
pub struct InodeGuard<'a> {
    cache_index: usize,
    entry: ManuallyDrop<SleepLockGuard<'a, InodeEntry>>,
}

impl<'a> InodeGuard<'a> {
    pub fn drop_with_lock(mut self, log: &LogGuard) {
        unsafe { ManuallyDrop::drop(&mut self.entry) };
        INODE_ALLOC.release(self.cache_index, log);
        core::mem::forget(self);
    }
}

impl<'a> Deref for InodeGuard<'a> {
    type Target = InodeEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl<'a> DerefMut for InodeGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

impl<'i> Drop for InodeGuard<'i> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.entry) };

        let log = log::start();
        INODE_ALLOC.release(self.cache_index, &log);
    }
}

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

    fn inode_block_at(&self, index: usize) -> usize {
        self.inodestart as usize + index / INODES_PER_BLOCK
    }

    fn bitmap_at(&self, index: usize) -> usize {
        self.bmapstart as usize + index / BITMAP_BITS
    }

    pub unsafe fn allocate_block(&self, device: usize, log: &LogGuard) -> Option<usize> {
        for bi in (0..(self.size as usize)).step_by(BITMAP_BITS) {
            let mut bitmap_buf = buffer::get(device, self.bitmap_at(bi)).unwrap();

            let bitmap = unsafe { bitmap_buf.read::<Bitmap<{ BITMAP_BITS }>>() };
            match bitmap.allocate() {
                Some(index) if (bi + index) < self.size as usize => {
                    log.write(&mut bitmap_buf);

                    let block = bi + index;
                    write_zeros_to_block(device, block, log);
                    return Some(block);
                }
                Some(index) => {
                    bitmap.deallocate(index).unwrap();
                    return None;
                }
                None => {
                    continue;
                }
            }
        }
        None
    }

    pub unsafe fn deallocate_block(&self, device: usize, block: usize, log: &LogGuard) {
        let mut bitmap_buf = buffer::get(device, self.bitmap_at(block)).unwrap();

        let bitmap = unsafe { bitmap_buf.read::<Bitmap<{ BITMAP_BITS }>>() };
        bitmap.deallocate(block % BITMAP_BITS).unwrap();
        assert!(bitmap.get(block % BITMAP_BITS) == Some(false));

        log.write(&bitmap_buf);
    }

    pub fn allocate_inode(
        &self,
        device: usize,
        kind: InodeKind,
        log: &LogGuard,
    ) -> Result<usize, ()> {
        for inum in 1..(self.ninodes as usize) {
            let block_index = self.inode_block_at(inum);
            let in_block_index = inum % INODES_PER_BLOCK;

            let mut block = buffer::get(device, block_index).unwrap();
            let inodes = unsafe { block.read_array::<Inode>() };

            let inode = &mut inodes[in_block_index];
            if inode.kind == InodeKind::Unused {
                inode.kind = kind;
                log.write(&mut block);

                return Ok(inum);
            }
        }

        Err(())
    }
}

pub struct InodeAllocator<const N: usize> {
    cache: SpinLock<CacheRc<InodeKey, N>>,
    inodes: [SleepLock<InodeEntry>; N],
}

impl<const N: usize> InodeAllocator<N> {
    pub const fn new() -> Self {
        Self {
            cache: SpinLock::new(CacheRc::new()),
            inodes: [const { SleepLock::new(InodeEntry::zeroed()) }; _],
        }
    }

    pub fn get(&self, device: usize, inode_number: usize) -> Option<InodeReference> {
        let mut cache = self.cache.lock();

        let (cache_index, is_new) = cache.get(InodeKey {
            device,
            index: inode_number,
        })?;

        if is_new {
            let mut entry = self.inodes[cache_index].lock();
            entry.device = device;
            entry.inode_number = inode_number;
            entry.is_initialized = false;
        }

        SpinLock::unlock(cache);

        Some(InodeReference {
            cache_index,
            entry: &self.inodes[cache_index],
        })
    }

    pub fn duplicate(&self, index: usize) {
        self.cache.lock().duplicate(index);
    }

    pub fn release(&self, index: usize, log: &LogGuard) {
        let mut cache = self.cache.lock();

        let reference = cache.reference_count(index).unwrap();
        if reference == 1 {
            let mut entry = self.inodes[index].lock();
            if entry.inode.nlink == 0 && entry.is_initialized {
                SpinLock::unlock_temporarily(&mut cache, move || {
                    assert!(entry.inode.nlink == 0);
                    entry.truncate(log);
                    entry.inode.kind = InodeKind::Unused;
                    entry.update(log);
                    entry.is_initialized = false;
                    drop(entry);
                });
            }
        }

        cache.release(index).unwrap();
    }
}

static mut SUPERBLOCK: SuperBlock = SuperBlock::zeroed();

static INODE_ALLOC: InodeAllocator<NINODE> = InodeAllocator::new();

fn write_zeros_to_block(device: usize, block: usize, log: &LogGuard) {
    let mut buf = buffer::get(device, block).unwrap();
    buf.clear();
    log.write(&mut buf);
}

pub fn initialize(device: usize) {
    fn read_superblock(device: usize) -> Option<SuperBlock> {
        let mut buf = buffer::get(device, 1)?;
        let val = unsafe { buf.read::<SuperBlock>().clone() };
        Some(val)
    }

    let superblock = read_superblock(device).unwrap();
    assert!(superblock.magic == FSMAGIC);
    log::initialize(device, &superblock);
    unsafe { SUPERBLOCK = superblock };
}

pub fn create(
    path: &str,
    kind: InodeKind,
    major: u16,
    minor: u16,
    log: &LogGuard,
) -> Result<InodeGuard<'static>, ()> {
    let (dir, name) = search_parent_inode(path).ok_or(())?;
    let mut dir = dir.lock();

    match dir.lookup(name) {
        Some((inode_ref, _)) => {
            drop(dir);
            let inode = inode_ref.lock();
            return if kind == InodeKind::File && (inode.is_file() || inode.is_device()) {
                Ok(inode)
            } else {
                Err(())
            };
        }
        None => {
            let inode_number = unsafe { SUPERBLOCK.allocate_inode(dir.device, kind, log)? };
            let inode_ref = get(dir.device, inode_number).unwrap();
            let mut inode = inode_ref.lock();
            inode.inode.major = major;
            inode.inode.minor = minor;
            inode.inode.nlink = 1;
            inode.update(log);

            let bad = |mut inode: InodeGuard| {
                inode.inode.nlink = 0;
                inode.update(log);
                Err(())
            };

            if kind == InodeKind::Directory {
                let this = inode.inode_number;
                let parent = dir.inode_number;
                if inode.link(".", this, log).is_err() || inode.link("..", parent, log).is_err() {
                    return bad(inode);
                }
            }

            if dir.link(name, inode.inode_number, log).is_err() {
                return bad(inode);
            }

            if kind == InodeKind::Directory {
                dir.increment_link();
                dir.update(log);
            }

            drop(dir);
            Ok(inode)
        }
    }
}

pub fn link(new: &str, old: &str) -> Result<(), ()> {
    let log = log::start();
    let inode_ref = search_inode(old).ok_or(())?;
    let mut ip = inode_ref.lock();

    if ip.is_directory() {
        return Err(());
    }

    let dev = ip.device;
    let inum = ip.inode_number;
    ip.increment_link();
    ip.update(&log);
    ip.drop_with_lock(&log);

    let bad = |inode_ref: InodeReference, log| {
        let mut inode = inode_ref.lock();
        inode.decrement_link();
        inode.update(&log);
        inode.drop_with_lock(&log);
        drop(inode_ref);
        drop(log);
        Err(())
    };

    let Some((dir, name)) = search_parent_inode(new) else {
        return bad(inode_ref, log);
    };
    let mut dir = dir.lock();

    if dir.device != dev {
        drop(dir);
        return bad(inode_ref, log);
    }

    if dir.link(name, inum, &log).is_err() {
        drop(dir);
        return bad(inode_ref, log);
    }

    dir.drop_with_lock(&log);
    drop(inode_ref);
    drop(log);
    Ok(())
}

pub fn unlink(path: &str) -> Result<(), ()> {
    let log = log::start();

    let (dir, name) = search_parent_inode(path).ok_or(())?;
    let mut dir = dir.lock();

    if name == "." || name == ".." {
        return Err(());
    }

    let (ip, offset) = dir.lookup(name).ok_or(())?;
    let mut ip = ip.lock();
    assert!(ip.counf_of_link() > 0);

    if ip.is_empty() == Some(false) {
        return Err(());
    }

    let entry = DirectoryEntry::unused();
    dir.write(entry, offset, &log).unwrap();

    if ip.is_directory() {
        dir.decrement_link();
        dir.update(&log);
    }
    dir.drop_with_lock(&log);

    ip.decrement_link();
    ip.update(&log);
    ip.drop_with_lock(&log);
    drop(log);
    Ok(())
}

pub fn make_directory(path: &str) -> Result<(), ()> {
    let log = log::start();
    let _ = create(path, InodeKind::Directory, 0, 0, &log)?;
    Ok(())
}

pub fn make_special_file(path: &str, major: u16, minor: u16) -> Result<(), ()> {
    let log = log::start();
    let _ = create(path, InodeKind::Device, major, minor, &log)?;
    Ok(())
}

pub fn get(device: usize, inode: usize) -> Option<InodeReference<'static>> {
    INODE_ALLOC.get(device, inode)
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

        let mut inode = inode_ref.lock();
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

        let mut inode = inode_ref.lock();
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
