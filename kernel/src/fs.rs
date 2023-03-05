use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};

use crate::filesystem::superblock::SuperBlock;
use crate::filesystem::{BITMAP_BITS, INODES_PER_BLOCK};
use crate::{
    bitmap::Bitmap,
    cache::RcCache,
    config::{NINODE, ROOTDEV},
    filesystem::buffer::{self, BSIZE},
    filesystem::log::{self, Logger},
    process::{self, copyin_either, copyout_either},
    sleeplock::{SleepLock, SleepLockGuard},
    spinlock::SpinLock,
};

const ROOTINO: usize = 1; // root i-number
const NDIRECT: usize = 12;
const NINDIRECT: usize = BSIZE / core::mem::size_of::<u32>();

const MAXFILE: usize = NDIRECT + NINDIRECT;
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

        assert!(!matches!(self.inode.kind, InodeKind::Unused));
    }

    fn offset_to_block(&mut self, offset: usize, log: Option<&Logger>) -> Option<usize> {
        let index = offset / BSIZE;

        if let Some(addr) = self.inode.addrs.get_mut(index) {
            if *addr == 0 {
                *addr = unsafe { allocate_block(&SUPERBLOCK, self.device, log?)? as u32 };
            }
            return Some(*addr as usize);
        }

        if (NDIRECT..NDIRECT + NINDIRECT).contains(&index) {
            let index = index - NDIRECT;

            if self.inode.chain == 0 {
                self.inode.chain =
                    unsafe { allocate_block(&SUPERBLOCK, self.device, log?)? as u32 };
            }

            let mut buf = buffer::get(self.device, self.inode.chain as usize)?;
            let addrs = unsafe { buf.read::<[u32; NINDIRECT]>() };
            let addr = if addrs[index] == 0 {
                let log = log?;
                let allocated = unsafe { allocate_block(&SUPERBLOCK, self.device, log)? };
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

    pub fn lookup<'log>(
        &mut self,
        name: &str,
        log: &'log Logger,
    ) -> Option<(InodeReference<'log>, usize)> {
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
                    .get(self.device, entry.inode_number as usize, log)
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
        log: &Logger,
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

    pub fn write<T>(&mut self, value: T, offset: usize, log: &Logger) -> Result<(), ()> {
        let wrote = self.copy_from::<T>(false, <*const T>::addr(&value), offset, 1, log)?;
        if wrote == core::mem::size_of::<T>() {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn link(&mut self, name: &str, inode_number: usize, log: &Logger) -> Result<(), ()> {
        if !self.is_directory() {
            return Err(());
        }

        if self.lookup(name, log).is_some() {
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

    pub fn update(&mut self, log: &Logger) {
        let inode_start = unsafe { SUPERBLOCK.inodestart as usize };

        let block_index = inode_start + self.inode_number / INODES_PER_BLOCK;
        let in_block_index = self.inode_number % INODES_PER_BLOCK;

        let mut block = buffer::get(self.device, block_index).unwrap();
        let inodes = unsafe { block.read_array::<Inode>() };
        inodes[in_block_index] = self.inode.clone();
        log.write(&block);
    }

    pub fn truncate(&mut self, log: &Logger) {
        for addr in self.inode.addrs {
            if addr != 0 {
                unsafe { deallocate_block(&SUPERBLOCK, self.device, addr as usize, log) };
            }
        }
        self.inode.addrs.fill(0);

        if self.inode.chain != 0 {
            let mut buf = buffer::get(self.device, self.inode.chain as usize).unwrap();
            let addrs = unsafe { buf.read::<[u32; NINDIRECT]>() };
            for addr in addrs {
                if *addr != 0 {
                    unsafe { deallocate_block(&SUPERBLOCK, self.device, *addr as usize, log) };
                }
            }
            unsafe { deallocate_block(&SUPERBLOCK, self.device, self.inode.chain as usize, log) };
            self.inode.chain = 0;
        }

        self.inode.size = 0;
        self.update(log);
    }
}

#[derive(Debug)]
pub struct InodePin {
    cache_index: usize,
    entry: &'static SleepLock<InodeEntry>,
}

impl InodePin {
    pub fn into_ref<'log>(self, log: &'log Logger) -> InodeReference<'log> {
        let reference = InodeReference {
            cache_index: self.cache_index,
            entry: self.entry,
            log,
        };

        INODE_ALLOC.duplicate(self.cache_index);

        self.drop_with_log(log);

        reference
    }

    pub fn drop_with_log(self, log: &Logger) {
        INODE_ALLOC.release(self.cache_index, log);
        core::mem::forget(self);
    }
}

impl Clone for InodePin {
    fn clone(&self) -> Self {
        INODE_ALLOC.duplicate(self.cache_index);
        Self { ..*self }
    }
}

impl Drop for InodePin {
    fn drop(&mut self) {
        let log = log::start();
        INODE_ALLOC.release(self.cache_index, &log);
    }
}

#[derive(Debug)]
pub struct InodeReference<'log> {
    cache_index: usize,
    entry: &'static SleepLock<InodeEntry>,
    log: &'log Logger,
}

impl<'log> InodeReference<'log> {
    pub fn lock(&self) -> InodeGuard<'log> {
        let mut guard = InodeGuard {
            cache_index: self.cache_index,
            entry: ManuallyDrop::new(self.entry.lock()),
            log: self.log,
        };

        INODE_ALLOC.duplicate(self.cache_index);

        if !guard.is_initialized {
            guard.initialize();
            guard.is_initialized = true;
        }

        guard
    }

    pub fn pin(self) -> InodePin {
        let pin = InodePin {
            cache_index: self.cache_index,
            entry: self.entry,
        };
        core::mem::forget(self);
        pin
    }
}

impl<'log> Clone for InodeReference<'log> {
    fn clone(&self) -> Self {
        INODE_ALLOC.duplicate(self.cache_index);
        Self { ..*self }
    }
}

impl<'log> Drop for InodeReference<'log> {
    fn drop(&mut self) {
        INODE_ALLOC.release(self.cache_index, self.log);
    }
}

#[derive(Debug)]
pub struct InodeGuard<'log> {
    cache_index: usize,
    entry: ManuallyDrop<SleepLockGuard<InodeEntry>>,
    log: &'log Logger,
}

impl<'log> InodeGuard<'log> {
    pub fn as_ref(this: &Self) -> InodeReference<'log> {
        INODE_ALLOC
            .get(this.entry.device, this.entry.inode_number, this.log)
            .unwrap()
    }
}

impl<'log> Deref for InodeGuard<'log> {
    type Target = InodeEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl<'log> DerefMut for InodeGuard<'log> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

impl<'log> Drop for InodeGuard<'log> {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.entry) };
        INODE_ALLOC.release(self.cache_index, self.log);
    }
}

unsafe fn allocate_block(superblock: &SuperBlock, device: usize, log: &Logger) -> Option<usize> {
    for bi in (0..(superblock.size as usize)).step_by(BITMAP_BITS) {
        let mut bitmap_buf = buffer::get(device, superblock.bitmap_at(bi)).unwrap();

        let bitmap = unsafe { bitmap_buf.read::<Bitmap<{ BITMAP_BITS }>>() };
        match bitmap.allocate() {
            Some(index) if (bi + index) < superblock.size as usize => {
                log.write(&bitmap_buf);

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

unsafe fn deallocate_block(superblock: &SuperBlock, device: usize, block: usize, log: &Logger) {
    let mut bitmap_buf = buffer::get(device, superblock.bitmap_at(block)).unwrap();

    let bitmap = unsafe { bitmap_buf.read::<Bitmap<{ BITMAP_BITS }>>() };
    bitmap.deallocate(block % BITMAP_BITS).unwrap();
    assert!(bitmap.get(block % BITMAP_BITS) == Some(false));

    log.write(&bitmap_buf);
}

fn allocate_inode(
    superblock: &SuperBlock,
    device: usize,
    kind: InodeKind,
    log: &Logger,
) -> Result<usize, ()> {
    for inum in 1..(superblock.ninodes as usize) {
        let block_index = superblock.inode_block_at(inum);
        let in_block_index = inum % INODES_PER_BLOCK;

        let mut block = buffer::get(device, block_index).unwrap();
        let inodes = unsafe { block.read_array::<Inode>() };

        let inode = &mut inodes[in_block_index];
        if inode.kind == InodeKind::Unused {
            inode.kind = kind;
            log.write(&block);

            return Ok(inum);
        }
    }

    Err(())
}

pub struct InodeCache<const N: usize> {
    cache: SpinLock<RcCache<InodeKey, N>>,
    inodes: [SleepLock<InodeEntry>; N],
}

impl<const N: usize> InodeCache<N> {
    pub const fn new() -> Self {
        Self {
            cache: SpinLock::new(RcCache::new()),
            inodes: [const { SleepLock::new(InodeEntry::zeroed()) }; _],
        }
    }

    pub fn get<'log>(
        &'static self,
        device: usize,
        inode_number: usize,
        log: &'log Logger,
    ) -> Option<InodeReference<'log>> {
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
            log,
        })
    }

    pub fn duplicate(&self, index: usize) {
        self.cache.lock().duplicate(index);
    }

    pub fn release(&'static self, index: usize, log: &Logger) {
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

static INODE_ALLOC: InodeCache<NINODE> = InodeCache::new();

fn write_zeros_to_block(device: usize, block: usize, log: &Logger) {
    let mut buf = buffer::get(device, block).unwrap();
    buf.clear();
    log.write(&buf);
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

pub fn create<'log>(
    path: &str,
    kind: InodeKind,
    major: u16,
    minor: u16,
    log: &'log Logger,
) -> Result<InodeGuard<'log>, ()> {
    let (dir_ref, name) = search_parent_inode(path, log).ok_or(())?;
    let mut dir = dir_ref.lock();

    match dir.lookup(name, log) {
        Some((inode_ref, _)) => {
            drop(dir);
            let inode = inode_ref.lock();
            if kind == InodeKind::File && (inode.is_file() || inode.is_device()) {
                Ok(inode)
            } else {
                Err(())
            }
        }
        None => {
            let inode_number = unsafe { allocate_inode(&SUPERBLOCK, dir.device, kind, log)? };
            let inode_ref = get(dir.device, inode_number, log).ok_or(())?;
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

            Ok(inode)
        }
    }
}

pub fn link(new: &str, old: &str) -> Result<(), ()> {
    let log = log::start();
    let inode_ref = search_inode(old, &log).ok_or(())?;
    let mut inode = inode_ref.lock();

    if inode.is_directory() {
        return Err(());
    }

    let dev = inode.device;
    let inum = inode.inode_number;
    inode.increment_link();
    inode.update(&log);
    drop(inode);

    let bad = |inode_ref: InodeReference, log| {
        let mut inode = inode_ref.lock();
        inode.decrement_link();
        inode.update(log);
        Err(())
    };

    let Some((dir_ref, name)) = search_parent_inode(new, &log) else {
        return bad(inode_ref, &log);
    };
    let mut dir = dir_ref.lock();

    if dir.device != dev {
        return bad(inode_ref, &log);
    }

    if dir.link(name, inum, &log).is_err() {
        return bad(inode_ref, &log);
    }

    Ok(())
}

pub fn unlink(path: &str) -> Result<(), ()> {
    let log = log::start();

    let (dir_ref, name) = search_parent_inode(path, &log).ok_or(())?;
    let mut dir = dir_ref.lock();

    if name == "." || name == ".." {
        return Err(());
    }

    let (ip_ref, offset) = dir.lookup(name, &log).ok_or(())?;
    let mut ip = ip_ref.lock();
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
    drop(dir);

    ip.decrement_link();
    ip.update(&log);
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

pub fn get<'log>(device: usize, inode: usize, log: &'log Logger) -> Option<InodeReference<'log>> {
    INODE_ALLOC.get(device, inode, log)
}

pub fn search_inode<'log>(path: &str, log: &'log Logger) -> Option<InodeReference<'log>> {
    let mut inode_ref = if path.starts_with('/') {
        get(ROOTDEV, ROOTINO, log).unwrap()
    } else {
        let context = process::context()?;
        let cwd = context.cwd.clone()?;
        cwd.into_ref(log)
    };

    for element in path.split('/') {
        if element.is_empty() {
            continue;
        }

        let mut inode = inode_ref.lock();
        if !inode.is_directory() {
            return None;
        }

        match inode.lookup(element, log) {
            Some((next, _)) => {
                drop(inode);
                inode_ref = next;
            }
            _ => return None,
        }
    }

    Some(inode_ref)
}

pub fn search_parent_inode<'p, 'log>(
    path: &'p str,
    log: &'log Logger,
) -> Option<(InodeReference<'log>, &'p str)> {
    let mut inode_ref = if path.starts_with('/') {
        get(ROOTDEV, ROOTINO, log).unwrap()
    } else {
        let context = process::context()?;
        let cwd = context.cwd.clone()?;
        cwd.into_ref(log)
    };

    let mut iter = path.split('/').peekable();
    while let Some(element) = iter.next() {
        if element.is_empty() {
            continue;
        }

        let mut inode = inode_ref.lock();
        if !inode.is_directory() {
            return None;
        }

        if iter.peek().is_none() {
            return Some((inode_ref, element));
        }

        match inode.lookup(element, log) {
            Some((next, _)) => {
                drop(inode);
                inode_ref = next;
            }
            _ => return None,
        }
    }

    None
}
