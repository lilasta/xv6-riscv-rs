use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};

use crate::bitmap::Bitmap;
use crate::cache::RcCache;
use crate::config::{NINODE, ROOTDEV};
use crate::filesystem::buffer::{self, BSIZE};
use crate::filesystem::directory_entry::DirectoryEntry;
use crate::filesystem::inode::{Inode, InodeKind};
use crate::filesystem::log::{self};
use crate::filesystem::superblock::SuperBlock;
use crate::filesystem::{
    BITMAP_BITS, FSMAGIC, INODES_PER_BLOCK, MAXFILE, NDIRECT, NINDIRECT, ROOTINO,
};
use crate::process::{self, copyin_either, copyout_either};
use crate::sleeplock::{SleepLock, SleepLockGuard};
use crate::spinlock::SpinLock;

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

#[derive(Debug)]
pub struct CachedInode {
    inode: Inode,
    device: usize,
    inode_number: usize,
    is_initialized: bool,
}

impl CachedInode {
    pub const fn zeroed() -> Self {
        Self {
            inode: Inode::zeroed(),
            device: 0,
            inode_number: 0,
            is_initialized: false,
        }
    }

    pub const fn is_directory(&self) -> bool {
        matches!(self.inode.kind, InodeKind::Directory)
    }

    pub const fn is_file(&self) -> bool {
        matches!(self.inode.kind, InodeKind::File)
    }

    pub const fn is_device(&self) -> bool {
        matches!(self.inode.kind, InodeKind::Device)
    }

    pub const fn counf_of_link(&self) -> usize {
        self.inode.nlink as usize
    }

    pub const fn increment_link(&mut self) {
        self.inode.nlink += 1;
    }

    pub const fn decrement_link(&mut self) {
        self.inode.nlink -= 1;
    }

    pub const fn size(&self) -> usize {
        self.inode.size as usize
    }

    pub const fn device_major(&self) -> Option<usize> {
        if self.is_device() {
            Some(self.inode.major as usize)
        } else {
            None
        }
    }

    pub const fn device_minor(&self) -> Option<usize> {
        if self.is_device() {
            Some(self.inode.minor as usize)
        } else {
            None
        }
    }

    pub const fn stat(&self) -> Stat {
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

        let block = unsafe {
            buffer::with_read::<[Inode; INODES_PER_BLOCK]>(self.device, block_index).unwrap()
        };

        self.inode = block[in_block_index].clone();

        assert!(!matches!(self.inode.kind, InodeKind::Unused));
    }

    fn offset_to_block(&mut self, offset: usize) -> Option<usize> {
        let index = offset / BSIZE;

        if let Some(addr) = self.inode.addrs.get_mut(index) {
            if *addr == 0 {
                *addr = unsafe { allocate_block(&SUPERBLOCK, self.device)? as u32 };
            }
            return Some(*addr as usize);
        }

        if (NDIRECT..NDIRECT + NINDIRECT).contains(&index) {
            let index = index - NDIRECT;

            if self.inode.chain == 0 {
                self.inode.chain = unsafe { allocate_block(&SUPERBLOCK, self.device)? as u32 };
            }

            let mut addrs = unsafe {
                buffer::with_read::<[u32; NINDIRECT]>(self.device, self.inode.chain as usize)?
            };
            let addr = if addrs[index] == 0 {
                let allocated = unsafe { allocate_block(&SUPERBLOCK, self.device)? };
                addrs[index] = allocated as u32;
                log::write(&addrs);
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

            let block = self.offset_to_block(offset).ok_or(())?;

            let offset_in_block = offset % BSIZE;
            let len = (n - read).min(BSIZE - offset_in_block);

            let src = unsafe { buffer::with_read::<[u8; BSIZE]>(self.device, block).unwrap() };
            let src = &src[offset_in_block..][..len];
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

    pub fn lookup(&mut self, name: &str) -> Option<(InodeReference, usize)> {
        if !self.is_directory() {
            return None;
        }

        for offset in (0..self.size()).step_by(core::mem::size_of::<DirectoryEntry>()) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number() == 0 {
                continue;
            }

            if entry.is(name) {
                return INODE_ALLOC
                    .get(self.device, entry.inode_number())
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
            if entry.inode_number() != 0 {
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

            let block = self.offset_to_block(offset).ok_or(())?;

            let mut dst = unsafe { buffer::with_read::<[u8; BSIZE]>(self.device, block).unwrap() };
            let offset_in_block = offset % BSIZE;
            let len = (write_size - wrote).min(BSIZE - offset_in_block);
            let is_copied = unsafe {
                let dst = &mut dst[offset_in_block..][..len];
                copyin_either(dst, is_src_user, src + wrote)
            };
            if is_copied {
                log::write(&dst);
            } else {
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

    pub fn link(&mut self, name: &str, inode_number: usize) -> Result<(), ()> {
        if !self.is_directory() {
            return Err(());
        }

        if self.lookup(name).is_some() {
            return Err(());
        }

        let new_entry = DirectoryEntry::new(inode_number, name);

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for offset in (0..self.size()).step_by(entry_size) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number() == 0 {
                self.write(new_entry, offset).unwrap();
                return Ok(());
            }
        }

        let insert_offset = self.size() - self.size() % entry_size;
        self.write(new_entry, insert_offset).or(Err(()))
    }

    pub fn update(&mut self) {
        let inode_start = unsafe { SUPERBLOCK.inodestart as usize };

        let block_index = inode_start + self.inode_number / INODES_PER_BLOCK;
        let in_block_index = self.inode_number % INODES_PER_BLOCK;

        let mut inodes = unsafe {
            buffer::with_read::<[Inode; INODES_PER_BLOCK]>(self.device, block_index).unwrap()
        };
        inodes[in_block_index] = self.inode.clone();
        log::write(&inodes);
    }

    pub fn truncate(&mut self) {
        for addr in self.inode.addrs {
            if addr != 0 {
                unsafe { deallocate_block(&SUPERBLOCK, self.device, addr as usize) };
            }
        }
        self.inode.addrs.fill(0);

        if self.inode.chain != 0 {
            let addrs = unsafe {
                buffer::with_read::<[u32; NINDIRECT]>(self.device, self.inode.chain as usize)
                    .unwrap()
            };
            for addr in addrs.iter() {
                if *addr != 0 {
                    unsafe { deallocate_block(&SUPERBLOCK, self.device, *addr as usize) };
                }
            }
            unsafe { deallocate_block(&SUPERBLOCK, self.device, self.inode.chain as usize) };
            self.inode.chain = 0;
        }

        self.inode.size = 0;
        self.update();
    }
}

#[derive(Debug)]
pub struct InodeReference {
    cache_index: usize,
    entry: &'static SleepLock<CachedInode>,
}

impl InodeReference {
    pub fn lock(&self) -> InodeGuard {
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

impl Clone for InodeReference {
    fn clone(&self) -> Self {
        INODE_ALLOC.duplicate(self.cache_index);
        Self { ..*self }
    }
}

impl Drop for InodeReference {
    fn drop(&mut self) {
        INODE_ALLOC.release(self.cache_index);
    }
}

#[derive(Debug)]
pub struct InodeGuard {
    cache_index: usize,
    entry: ManuallyDrop<SleepLockGuard<CachedInode>>,
}

impl InodeGuard {
    pub fn as_ref(this: &Self) -> InodeReference {
        INODE_ALLOC
            .get(this.entry.device, this.entry.inode_number)
            .unwrap()
    }
}

impl Deref for InodeGuard {
    type Target = CachedInode;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl DerefMut for InodeGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

impl Drop for InodeGuard {
    fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.entry) };
        INODE_ALLOC.release(self.cache_index);
    }
}

unsafe fn allocate_block(superblock: &SuperBlock, device: usize) -> Option<usize> {
    for bi in (0..(superblock.size as usize)).step_by(BITMAP_BITS) {
        let mut bitmap =
            buffer::with_read::<Bitmap<{ BITMAP_BITS }>>(device, superblock.bitmap_at(bi)).unwrap();

        match bitmap.allocate() {
            Some(index) if (bi + index) < superblock.size as usize => {
                log::write(&bitmap);

                let block = bi + index;
                write_zeros_to_block(device, block);
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

unsafe fn deallocate_block(superblock: &SuperBlock, device: usize, block: usize) {
    let mut bitmap =
        buffer::with_read::<Bitmap<{ BITMAP_BITS }>>(device, superblock.bitmap_at(block)).unwrap();

    bitmap.deallocate(block % BITMAP_BITS).unwrap();
    assert!(bitmap.get(block % BITMAP_BITS) == Some(false));

    log::write(&bitmap);
}

fn allocate_inode(superblock: &SuperBlock, device: usize, kind: InodeKind) -> Result<usize, ()> {
    for inum in 1..(superblock.ninodes as usize) {
        let block_index = superblock.inode_block_at(inum);
        let in_block_index = inum % INODES_PER_BLOCK;

        let mut inodes =
            unsafe { buffer::with_read::<[Inode; INODES_PER_BLOCK]>(device, block_index).unwrap() };

        let inode = &mut inodes[in_block_index];
        if inode.kind == InodeKind::Unused {
            inode.kind = kind;
            log::write(&inodes);

            return Ok(inum);
        }
    }

    Err(())
}

pub struct InodeCache<const N: usize> {
    cache: SpinLock<RcCache<InodeKey, N>>,
    inodes: [SleepLock<CachedInode>; N],
}

impl<const N: usize> InodeCache<N> {
    pub const fn new() -> Self {
        Self {
            cache: SpinLock::new(RcCache::new()),
            inodes: [const { SleepLock::new(CachedInode::zeroed()) }; _],
        }
    }

    pub fn get(&'static self, device: usize, inode_number: usize) -> Option<InodeReference> {
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

    pub fn release(&'static self, index: usize) {
        let mut cache = self.cache.lock();

        let reference = cache.reference_count(index).unwrap();
        if reference == 1 {
            let mut entry = self.inodes[index].lock();
            if entry.inode.nlink == 0 && entry.is_initialized {
                SpinLock::unlock_temporarily(&mut cache, move || {
                    assert!(entry.inode.nlink == 0);
                    entry.truncate();
                    entry.inode.kind = InodeKind::Unused;
                    entry.update();
                    entry.is_initialized = false;
                });
            }
        }

        cache.release(index).unwrap();
    }
}

static mut SUPERBLOCK: SuperBlock = SuperBlock::zeroed();

static INODE_ALLOC: InodeCache<NINODE> = InodeCache::new();

fn write_zeros_to_block(device: usize, block: usize) {
    let buf = buffer::with_write::<[u8; BSIZE]>(device, block, &[0; _]).unwrap();
    log::write(&buf);
}

pub fn initialize(device: usize) {
    let superblock = unsafe { buffer::with_read::<SuperBlock>(device, 1).unwrap() };
    assert!(superblock.magic == FSMAGIC);
    log::initialize(device, &superblock);
    unsafe { SUPERBLOCK = (*superblock).clone() };
}

pub fn create(path: &str, kind: InodeKind, major: u16, minor: u16) -> Result<InodeGuard, ()> {
    log::with(|| {
        let (dir_ref, name) = search_parent_inode(path).ok_or(())?;
        let mut dir = dir_ref.lock();

        match dir.lookup(name) {
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
                let inode_number = unsafe { allocate_inode(&SUPERBLOCK, dir.device, kind)? };
                let inode_ref = get(dir.device, inode_number).ok_or(())?;
                let mut inode = inode_ref.lock();
                inode.inode.major = major;
                inode.inode.minor = minor;
                inode.inode.nlink = 1;
                inode.update();

                let bad = |mut inode: InodeGuard| {
                    inode.inode.nlink = 0;
                    inode.update();
                    Err(())
                };

                if kind == InodeKind::Directory {
                    let this = inode.inode_number;
                    let parent = dir.inode_number;
                    if inode.link(".", this).is_err() || inode.link("..", parent).is_err() {
                        return bad(inode);
                    }
                }

                if dir.link(name, inode.inode_number).is_err() {
                    return bad(inode);
                }

                if kind == InodeKind::Directory {
                    dir.increment_link();
                    dir.update();
                }

                Ok(inode)
            }
        }
    })
}

pub fn link(new: &str, old: &str) -> Result<(), ()> {
    log::with(|| {
        let inode_ref = search_inode(old).ok_or(())?;
        let mut inode = inode_ref.lock();

        if inode.is_directory() {
            return Err(());
        }

        let dev = inode.device;
        let inum = inode.inode_number;
        inode.increment_link();
        inode.update();
        drop(inode);

        let bad = |inode_ref: InodeReference| {
            let mut inode = inode_ref.lock();
            inode.decrement_link();
            inode.update();
            Err(())
        };

        let Some((dir_ref, name)) = search_parent_inode(new) else {
            return bad(inode_ref);
        };
        let mut dir = dir_ref.lock();

        if dir.device != dev {
            return bad(inode_ref);
        }

        if dir.link(name, inum).is_err() {
            return bad(inode_ref);
        }

        Ok(())
    })
}

pub fn unlink(path: &str) -> Result<(), ()> {
    log::with(|| {
        let (dir_ref, name) = search_parent_inode(path).ok_or(())?;
        let mut dir = dir_ref.lock();

        if name == "." || name == ".." {
            return Err(());
        }

        let (ip_ref, offset) = dir.lookup(name).ok_or(())?;
        let mut ip = ip_ref.lock();
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
        Ok(())
    })
}

pub fn make_directory(path: &str) -> Result<(), ()> {
    log::with(|| {
        let _ = create(path, InodeKind::Directory, 0, 0)?;
        Ok(())
    })
}

pub fn make_special_file(path: &str, major: u16, minor: u16) -> Result<(), ()> {
    log::with(|| {
        let _ = create(path, InodeKind::Device, major, minor)?;
        Ok(())
    })
}

pub fn get(device: usize, inode: usize) -> Option<InodeReference> {
    INODE_ALLOC.get(device, inode)
}

pub fn search_inode(path: &str) -> Option<InodeReference> {
    let mut inode_ref = if path.starts_with('/') {
        get(ROOTDEV, ROOTINO).unwrap()
    } else {
        let context = process::context()?;
        let mut cwd = context.cwd.clone()?;
        unsafe { ManuallyDrop::take(&mut cwd) }
    };

    for element in path.split('/') {
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

fn search_parent_inode(path: &str) -> Option<(InodeReference, &str)> {
    let mut inode_ref = if path.starts_with('/') {
        get(ROOTDEV, ROOTINO).unwrap()
    } else {
        let context = process::context()?;
        let mut cwd = context.cwd.clone()?;
        unsafe { ManuallyDrop::take(&mut cwd) }
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
