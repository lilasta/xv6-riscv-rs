use core::{
    ffi::{c_char, CStr},
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

use alloc::ffi::CString;

use crate::{
    bitmap::Bitmap,
    buffer::{self, BSIZE},
    cache::CacheRc,
    config::{NINODE, ROOTDEV},
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    log::{self, initlog, LogGuard},
    process,
};

// Directory is a file containing a sequence of dirent structures.
pub const DIRSIZE: usize = 14;

const ROOTINO: usize = 1; // root i-number
const NDIRECT: usize = 12;
const NINDIRECT: usize = BSIZE / core::mem::size_of::<u32>();

const MAXFILE: usize = NDIRECT + NINDIRECT;
const INODES_PER_BLOCK: usize = BSIZE / core::mem::size_of::<Inode>();
const FSMAGIC: u32 = 0x10203040;

static mut FS: FileSystem = FileSystem::uninit();

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
    inode_start: usize,
    inode_count: usize,
    cache: SpinLock<CacheRc<InodeKey, N>>,
    inodes: [SleepLock<Inode>; N],
}

impl<const N: usize> InodeAllocator<N> {
    const fn inode_block_at(&self, index: usize) -> usize {
        self.inode_start + index / INODES_PER_BLOCK
    }

    pub const fn new(inode_start: usize, inode_count: usize) -> Self {
        Self {
            inode_start,
            inode_count,
            cache: SpinLock::new(CacheRc::new()),
            inodes: [const { SleepLock::new(Inode::zeroed()) }; _],
        }
    }

    fn read_inode(&self, device: usize, index: usize) -> Option<Inode> {
        let block = buffer::get(device, self.inode_block_at(index))?;
        let ptr = unsafe { block.as_ptr::<Inode>().add(index % INODES_PER_BLOCK) };
        let inode = unsafe { ptr.read() };
        buffer::release(block);
        Some(inode)
    }

    fn write_inode(&self, device: usize, index: usize, inode: &Inode, log: &LogGuard) {
        let mut block = buffer::get(device, self.inode_block_at(index)).unwrap();
        let ptr = unsafe { block.as_mut_ptr::<Inode>().add(index % INODES_PER_BLOCK) };
        unsafe { ptr.copy_from(inode, 1) };
        log.write(&block);
        buffer::release(block);
    }

    pub fn get(&self, device: usize, inode_index: usize) -> Option<InodeGuard> {
        let (cache_index, is_new) = self.cache.lock().get(InodeKey {
            device,
            index: inode_index,
        })?;

        if is_new {
            *self.inodes[cache_index].lock() = self.read_inode(device, inode_index).unwrap();
        }

        Some(InodeGuard {
            device,
            inode_index,
            cache_index,
            inode: self.inodes[cache_index].lock(),
        })
    }

    pub fn pin(&self, cache_index: usize) {
        self.cache.lock().pin(cache_index).unwrap();
    }

    pub fn release(&self, mut guard: InodeGuard, log: &LogGuard) {
        let was_last = self.cache.lock().release(guard.cache_index).unwrap();
        if was_last {
            assert!(guard.inode.nlink == 0); // TODO: ?

            truncate(&mut guard, log);
            self.deallocate(guard.device, guard.inode_index, log);
        }
    }

    pub fn allocate(&self, device: usize, kind: u16, log: &LogGuard) -> Option<usize> {
        for index in 1..(self.inode_count as usize) {
            let mut block = buffer::get(device, self.inode_block_at(index)).unwrap();
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
                buffer::release(block);
                return Some(index);
            }

            buffer::release(block);
        }

        None
    }

    pub fn deallocate(&self, device: usize, index: usize, log: &LogGuard) {
        let mut block = buffer::get(device, self.inode_block_at(index)).unwrap();
        let ptr = unsafe { block.as_mut_ptr::<Inode>().add(index % INODES_PER_BLOCK) };
        unsafe { (*ptr).kind = 0 };
        log.write(&block);
        buffer::release(block);
    }
}

pub struct FileSystem {
    superblock: SuperBlock,
    inode_alloc: InodeAllocator<NINODE>,
}

impl FileSystem {
    const BITMAP_BITS: usize = BSIZE * (u8::BITS as usize);

    const fn bitmap_at(&self, index: usize) -> usize {
        self.superblock.bmapstart as usize + index / Self::BITMAP_BITS
    }

    /*
    pub const fn new(superblock: SuperBlock) -> Self {
        Self {
            inode_alloc: InodeAllocator::new(
                superblock.inodestart as usize,
                superblock.ninodes as usize,
            ),
            superblock,
        }
    }
    */

    pub const fn uninit() -> Self {
        Self {
            inode_alloc: InodeAllocator::new(0, 0),
            superblock: SuperBlock::zeroed(),
        }
    }

    pub fn init(&mut self, superblock: SuperBlock) {
        self.inode_alloc.inode_count = superblock.ninodes as usize;
        self.inode_alloc.inode_start = superblock.inodestart as usize;
        self.superblock = superblock;
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

    unsafe { FS.init(superblock) };
}

#[no_mangle]
extern "C" fn sb() -> *mut SuperBlock {
    unsafe { &mut FS.superblock }
}

#[no_mangle]
unsafe extern "C" fn balloc(dev: u32) -> u32 {
    let guard = log::get_guard_without_start();
    let ret = FS.allocate_block(dev as _, &guard).unwrap_or(0);
    core::mem::forget(guard);
    ret as u32
}

#[no_mangle]
unsafe extern "C" fn bfree(dev: u32, block: u32) {
    let guard = log::get_guard_without_start();
    FS.deallocate_block(dev as _, block as _, &guard);
    core::mem::forget(guard);
}

pub trait InodeOps {
    fn get(&self, device: usize, inode: usize) -> Option<InodeLockGuard>;
    fn create(&self, path: &str, kind: u16, major: u16, minor: u16) -> Result<InodeLockGuard, ()>;
    fn search(&self, path: &str) -> Option<InodeLockGuard>;
    fn search_parent(&self, path: &str, name: &mut [u8; DIRSIZE + 1]) -> Option<InodeLockGuard>;
}

impl<'a> InodeOps for LogGuard<'a> {
    fn get(&self, device: usize, inode: usize) -> Option<InodeLockGuard> {
        extern "C" {
            fn iget(device: u32, inode: u32) -> *mut InodeC;
        }

        unsafe {
            let inode = iget(device as _, inode as _);
            if inode.is_null() {
                None
            } else {
                Some(InodeLockGuard::new(inode))
            }
        }
    }

    fn create(&self, path: &str, kind: u16, major: u16, minor: u16) -> Result<InodeLockGuard, ()> {
        let mut name = [0u8; DIRSIZE + 1];
        let mut dir = self.search_parent(path, &mut name).ok_or(())?;

        let name = unsafe { CStr::from_ptr(name.as_ptr().cast()).to_str().or(Err(()))? };

        match dir.lookup(name) {
            // TODO: T_FILE
            Some((inode, _)) if kind == 2 && (inode.is_file() || inode.is_device()) => Ok(inode),
            Some(_) => Err(()),
            None => {
                extern "C" {
                    fn ialloc(dev: u32, kind: u16) -> u32;
                }

                let inode_number = unsafe { ialloc(dir.device_number() as u32, kind) as usize };
                assert!(inode_number != 0);

                let mut inode = self.get(dir.device_number(), inode_number).unwrap();
                inode.major = major;
                inode.minor = minor;
                inode.nlink = 1;
                inode.update();

                // TODO: T_DIR
                // TODO: avoid unwrap (https://github.com/mit-pdos/xv6-riscv/blob/riscv/kernel/sysfile.c#L295)
                if kind == 1 {
                    dir.increment_link();
                    dir.update();

                    inode.link(".", inode.inode_number()).unwrap();
                    inode.link("..", dir.inode_number()).unwrap();
                }

                dir.link(name, inode.inode_number()).unwrap();

                Ok(inode)
            }
        }
    }

    fn search(&self, path: &str) -> Option<InodeLockGuard> {
        /*
        let mut inode = if path.starts_with("/") {
            self.get(ROOTDEV, ROOTINO).unwrap()
        } else {
            InodeLockGuard::new(idup(process::context().unwrap().cwd))
        };

        for element in path.split_terminator("/") {
            if element.is_empty() {
                continue;
            }

            if element == "." {
                continue;
            }

            match inode.lookup(element) {
                Some((next, _)) => inode = next,
                _ => return None,
            }
        }

        Some(inode)
        */
        let inode = namei(path.as_ptr().cast());
        if inode.is_null() {
            None
        } else {
            Some(InodeLockGuard::new(inode))
        }
    }

    fn search_parent(&self, path: &str, name: &mut [u8; DIRSIZE + 1]) -> Option<InodeLockGuard> {
        extern "C" {
            fn nameiparent(path: *const c_char, name: *mut c_char) -> *mut InodeC;
        }

        unsafe {
            let inode = nameiparent(path.as_ptr().cast(), name.as_mut_ptr().cast());
            if inode.is_null() {
                None
            } else {
                Some(InodeLockGuard::new(inode))
            }
        }
    }
}

#[repr(C)]
pub struct InodeC {
    dev: u32,
    inum: u32,
    refcnt: u32,
    valid: u32,
    kind: u16,
    major: u16,
    minor: u16,
    nlink: u16,
    size: u32,
    addrs: [u32; NDIRECT + 1],
}

#[repr(C)]
pub struct DirectoryEntry {
    inode_number: u16,
    name: [u8; DIRSIZE],
}

impl DirectoryEntry {
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

pub struct InodeLockGuard<'a> {
    inode: *mut InodeC,
    lifetime: PhantomData<&'a ()>,
}

impl<'a> InodeLockGuard<'a> {
    pub fn new(inode: *mut InodeC) -> Self {
        extern "C" {
            fn ilock(ip: *mut InodeC);
        }
        unsafe { ilock(inode) };
        Self {
            inode,
            lifetime: PhantomData,
        }
    }

    pub const fn is_directory(&self) -> bool {
        unsafe { (*self.inode).kind == 1 }
    }

    pub const fn is_file(&self) -> bool {
        unsafe { (*self.inode).kind == 2 }
    }

    pub const fn is_device(&self) -> bool {
        unsafe { (*self.inode).kind == 3 }
    }

    pub const fn counf_of_link(&self) -> usize {
        unsafe { (*self.inode).nlink as usize }
    }

    pub const fn increment_link(&mut self) {
        unsafe { (*self.inode).nlink += 1 };
    }

    pub const fn decrement_link(&mut self) {
        unsafe { (*self.inode).nlink -= 1 };
    }

    pub const fn size(&self) -> usize {
        unsafe { (*self.inode).size as usize }
    }

    pub const fn device_number(&self) -> usize {
        unsafe { (*self.inode).dev as usize }
    }

    pub const fn inode_number(&self) -> usize {
        unsafe { (*self.inode).inum as usize }
    }

    pub const fn device_major(&self) -> Option<usize> {
        if self.is_device() {
            Some(unsafe { (*self.inode).major as usize })
        } else {
            None
        }
    }

    pub const fn device_minor(&self) -> Option<usize> {
        if self.is_device() {
            Some(unsafe { (*self.inode).minor as usize })
        } else {
            None
        }
    }

    pub fn stat(&self) -> Stat {
        Stat {
            device: self.dev,
            inode: self.inum,
            kind: self.kind,
            nlink: self.nlink,
            size: self.size as usize,
        }
    }

    // TODO: -> InodeKey
    pub fn unlock_without_put(self) -> *mut InodeC {
        extern "C" {
            fn iunlock(ip: *mut InodeC);
        }

        unsafe {
            let inode = self.inode;
            core::mem::forget(self);
            iunlock(inode);
            inode
        }
    }

    pub fn copy_to<T>(
        &self,
        is_dst_user: bool,
        dst: *mut T,
        offset: usize,
        count: usize,
    ) -> Result<usize, usize> {
        extern "C" {
            fn readi(ip: *mut InodeC, user_dst: i32, dst: usize, off: u32, n: u32) -> i32;
        }

        unsafe {
            let type_size = core::mem::size_of::<T>();
            let must_read = type_size * count;

            let read = readi(
                self.inode,
                is_dst_user as i32,
                dst.addr(),
                offset as u32,
                must_read as u32,
            ) as usize;

            if read == must_read {
                Ok(must_read)
            } else {
                Err(read)
            }
        }
    }

    pub fn copy_from<T>(
        &self,
        is_src_user: bool,
        src: *const T,
        offset: usize,
        count: usize,
    ) -> Result<usize, usize> {
        let type_size = core::mem::size_of::<T>();
        let must_write = type_size * count;
        /*

        if offset > self.size as usize || offset.checked_add(must_write).is_none() {
            return Err(0);
        }

        if offset + must_write > MAXFILE * BSIZE {
            return Err(0);
        }

        let mut wrote = 0;
        while wrote < must_write {
            todo!()
        } */

        extern "C" {
            fn writei(ip: *mut InodeC, user_src: i32, src: usize, off: u32, n: u32) -> i32;
        }

        unsafe {
            let wrote = writei(
                self.inode,
                is_src_user as i32,
                src.addr(),
                offset as u32,
                must_write as u32,
            );

            let wrote = if wrote < 0 {
                return Err(0);
            } else {
                wrote as usize
            };

            if wrote == must_write {
                Ok(must_write)
            } else {
                Err(wrote)
            }
        }
    }

    pub fn read<T>(&self, offset: usize) -> Result<T, usize> {
        let mut value = MaybeUninit::<T>::uninit();
        self.copy_to(false, value.as_mut_ptr(), offset, 1)?;
        Ok(unsafe { value.assume_init() })
    }

    pub fn write<T>(&mut self, value: T, offset: usize) -> Result<(), usize> {
        self.copy_from(false, &value, offset, 1).and(Ok(()))
    }

    pub fn update(&mut self) {
        extern "C" {
            fn iupdate(ip: *mut InodeC);
        }

        unsafe { iupdate(self.inode) };
    }

    pub fn truncate(&mut self) {
        extern "C" {
            fn itrunc(ip: *mut InodeC);
        }

        unsafe { itrunc(self.inode) };
    }

    pub fn link(&mut self, name: &str, inode_number: usize) -> Result<(), ()> {
        if !self.is_directory() {
            return Err(());
        }

        if self.lookup(name).is_some() {
            return Err(());
        }

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for offset in (0..self.size()).step_by(entry_size) {
            let mut entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                entry.name.fill(0);
                entry.name[..name.len()].copy_from_slice(name.as_bytes());
                entry.inode_number = inode_number as u16;
                self.write(entry, offset).unwrap();
                return Ok(());
            }
        }

        // TODO: WHAT IS THIS
        let mut entry = DirectoryEntry {
            inode_number: inode_number as u16,
            name: [0; _],
        };
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        self.write(entry, self.size()).unwrap();
        Ok(())
    }

    pub fn lookup(&self, name: &str) -> Option<(InodeLockGuard<'a>, usize)> {
        if !self.is_directory() {
            return None;
        }

        let name = CString::new(name).unwrap();

        for offset in (0..self.size()).step_by(core::mem::size_of::<DirectoryEntry>()) {
            let entry = self.read::<DirectoryEntry>(offset).unwrap();
            if entry.inode_number == 0 {
                continue;
            }

            let mut cmp = [0; DIRSIZE];
            cmp[..name.as_bytes().len()].copy_from_slice(name.as_bytes());

            if cmp == entry.name {
                extern "C" {
                    fn iget(device: u32, inode: u32) -> *mut InodeC;
                }

                unsafe {
                    let inode = iget(self.device_number() as u32, entry.inode_number as u32);
                    return if inode.is_null() {
                        None
                    } else {
                        Some((InodeLockGuard::new(inode), offset))
                    };
                }
            }
        }

        None
    }

    pub fn is_empty(&self) -> Option<bool> {
        if !self.is_directory() {
            return None;
        }

        let entry_size = core::mem::size_of::<DirectoryEntry>();
        for off in ((2 * entry_size)..(self.size as usize)).step_by(entry_size) {
            let entry = self.read::<DirectoryEntry>(off).unwrap();
            if entry.inode_number != 0 {
                return Some(false);
            }
        }
        Some(true)
    }
}

impl<'a> Deref for InodeLockGuard<'a> {
    type Target = InodeC;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.inode }
    }
}

impl<'a> DerefMut for InodeLockGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.inode }
    }
}

impl<'a> Drop for InodeLockGuard<'a> {
    fn drop(&mut self) {
        extern "C" {
            fn iunlockput(ip: *mut InodeC);
        }
        unsafe { iunlockput(self.inode) };
    }
}

pub fn link(new: &str, old: &str) -> Result<(), ()> {
    let log = log::start();
    let mut ip = log.search(old).ok_or(())?;

    if ip.is_directory() {
        return Err(());
    }

    let dev = ip.device_number();
    let inum = ip.inode_number();
    ip.increment_link();
    ip.update();
    drop(ip);

    let bad = || {
        let mut inode = log.get(dev, inum).unwrap();
        inode.decrement_link();
        Err(())
    };

    let mut name = [0u8; DIRSIZE + 1];
    let Some(mut dir) = log.search_parent(new, &mut name) else {
        return bad();
    };

    if dir.device_number() != dev {
        return bad();
    }

    let name = CStr::from_bytes_until_nul(&name).unwrap().to_str().unwrap();
    if dir.link(name, inum).is_err() {
        return bad();
    }

    Ok(())
}

#[deprecated]
pub fn namei(path: *const c_char) -> *mut InodeC {
    extern "C" {
        fn namei(path: *const c_char) -> *mut InodeC;
    }

    unsafe { namei(path) }
}

#[deprecated]
pub fn idup(ip: *mut InodeC) -> *mut InodeC {
    extern "C" {
        fn idup(ip: *mut InodeC) -> *mut InodeC;
    }

    unsafe { idup(ip) }
}

#[deprecated]
pub fn put(_log: &crate::log::LogGuard, ip: *mut InodeC) {
    extern "C" {
        fn iput(ip: *mut InodeC);
    }

    unsafe { iput(ip) };
}

pub fn unlink(path: &str) -> Result<(), ()> {
    let log = log::start();

    let mut name = [0u8; DIRSIZE + 1];
    let mut dir = log.search_parent(path, &mut name).ok_or(())?;

    let name = CStr::from_bytes_until_nul(&name).unwrap();
    let name = name.to_str().map_err(|_| ()).unwrap();

    if name == "." || name == ".." {
        return Err(());
    }

    let (mut ip, offset) = dir.lookup(name).ok_or(())?;
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

    ip.decrement_link();
    ip.update();
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
