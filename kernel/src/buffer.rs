use core::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};

use crate::{
    cache::CacheRc,
    config::NBUF,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    undrop::Undroppable,
    virtio,
};

pub const BSIZE: usize = 1024;

pub const fn check_buffer_size<T, const SIZE: usize>() -> Option<usize> {
    if core::mem::size_of::<T>() > SIZE {
        None
    } else {
        Some(0)
    }
}

#[derive(PartialEq, Eq)]
struct BufferKey {
    device: usize,
    block: usize,
}

#[repr(C)]
pub struct Buffer<const SIZE: usize> {
    data: [u8; SIZE],
}

impl<const SIZE: usize> Buffer<SIZE> {
    pub const fn zeroed() -> Self {
        Self { data: [0; _] }
    }

    pub const fn size(&self) -> usize {
        SIZE
    }

    pub const fn as_ptr<T>(&self) -> *const T
    where
        [(); check_buffer_size::<T, SIZE>().unwrap()]:,
    {
        self.data.as_ptr().cast()
    }

    pub const fn as_mut_ptr<T>(&mut self) -> *mut T
    where
        [(); check_buffer_size::<T, SIZE>().unwrap()]:,
    {
        self.data.as_mut_ptr().cast()
    }

    pub const fn as_uninit<T>(&self) -> &MaybeUninit<T>
    where
        [(); check_buffer_size::<T, SIZE>().unwrap()]:,
    {
        let ptr = self.data.as_ptr();
        let ptr = ptr.cast::<MaybeUninit<T>>();
        unsafe { &*ptr }
    }

    pub const fn as_uninit_mut<T>(&mut self) -> &mut MaybeUninit<T>
    where
        [(); check_buffer_size::<T, SIZE>().unwrap()]:,
    {
        let ptr = self.data.as_mut_ptr();
        let ptr = ptr.cast::<MaybeUninit<T>>();
        unsafe { &mut *ptr }
    }

    pub fn write_zeros(&mut self) {
        self.data.fill(0);
    }
}

pub struct BufferGuard<'a> {
    buffer: LockGuard<'a, SleepLock<Buffer<BSIZE>>>,
    in_use: bool,
    block_number: usize,
    cache_index: usize,
}

impl<'a> BufferGuard<'a> {
    pub const fn block_number(&self) -> usize {
        self.block_number
    }
}

impl<'a> Deref for BufferGuard<'a> {
    type Target = Buffer<BSIZE>;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl<'a> DerefMut for BufferGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

pub struct BufferCache<const SIZE: usize> {
    cache: SpinLock<CacheRc<BufferKey, SIZE>>,
    buffers: [SleepLock<Buffer<BSIZE>>; SIZE],
}

impl<const SIZE: usize> BufferCache<SIZE> {
    pub const fn new() -> Self {
        Self {
            cache: SpinLock::new(CacheRc::new()),
            buffers: [const { SleepLock::new(Buffer::zeroed()) }; _],
        }
    }

    pub fn get(&self, device: usize, block: usize) -> Option<BufferGuard> {
        let (index, is_new) = self.cache.lock().get(BufferKey { device, block })?;

        let mut guard: BufferGuard = BufferGuard {
            buffer: self.buffers[index].lock(),
            in_use: false,
            block_number: block,
            cache_index: index,
        };

        if is_new {
            unsafe {
                virtio::disk::read(
                    guard.buffer.as_mut_ptr::<u8>().addr(),
                    guard.block_number(),
                    guard.buffer.size(),
                )
            };
        }

        Some(guard)
    }

    pub fn release(&self, buffer: BufferGuard) {
        self.cache.lock().release(buffer.cache_index).unwrap();
    }

    pub fn pin(&self, guard: &BufferGuard) {
        self.cache.lock().pin(guard.cache_index).unwrap();
    }

    pub fn unpin(&self, guard: &BufferGuard) {
        self.cache.lock().unpin(guard.cache_index).unwrap();
    }
}

static CACHE: BufferCache<NBUF> = BufferCache::new();

pub fn get(device: usize, block: usize) -> Option<Undroppable<BufferGuard<'static>>> {
    CACHE.get(device, block).map(Undroppable::new)
}

pub fn get_with_unlock<L: Lock>(
    device: usize,
    block: usize,
    lock: &mut LockGuard<L>,
) -> Option<Undroppable<BufferGuard<'static>>> {
    Lock::unlock_temporarily(lock, || get(device, block))
}

pub fn write(guard: &mut Undroppable<BufferGuard>) {
    unsafe {
        virtio::disk::write(
            guard.buffer.as_mut_ptr::<u8>().addr(),
            guard.block_number(),
            guard.buffer.size(),
        )
    };
}

pub fn write_with_unlock<L: Lock>(
    buffer: &mut Undroppable<BufferGuard<'static>>,
    lock: &mut LockGuard<L>,
) {
    Lock::unlock_temporarily(lock, || write(buffer));
}

pub fn release(buffer: Undroppable<BufferGuard>) {
    CACHE.release(Undroppable::into_inner(buffer));
}

pub fn release_with_unlock<L: Lock>(buffer: Undroppable<BufferGuard>, lock: &mut LockGuard<L>) {
    Lock::unlock_temporarily(lock, || release(buffer));
}

pub fn pin(guard: &BufferGuard) {
    CACHE.pin(guard);
}

pub fn unpin(guard: &BufferGuard) {
    CACHE.unpin(guard);
}

mod bindings {
    use super::*;

    #[no_mangle]
    unsafe extern "C" fn log_write(buf: *mut BufferC) {
        let log = crate::log::LogGuard;
        let guard = Undroppable::new(BufferGuard {
            buffer: LockGuard::from_ptr((*buf).original),
            in_use: false,
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        });
        log.write(&guard);
        Undroppable::forget(guard);
        core::mem::forget(log);
    }

    #[repr(C)]
    pub struct BufferC {
        data: *mut u8,
        block_index: usize,
        cache_index: usize,
        original: *const SleepLock<Buffer<BSIZE>>,
    }

    #[no_mangle]
    extern "C" fn binit() {}

    #[no_mangle]
    unsafe extern "C" fn bread(device: u32, block: u32) -> BufferC {
        let buf = get(device as _, block as _).unwrap();

        let ret = BufferC {
            data: buf.data.as_ptr().cast_mut(),
            block_index: block as _,
            cache_index: buf.cache_index,
            original: LockGuard::as_ptr(&buf.buffer),
        };

        core::mem::forget(buf);

        ret
    }

    #[no_mangle]
    unsafe extern "C" fn brelse(buf: BufferC) {
        let guard = Undroppable::new(BufferGuard {
            buffer: LockGuard::from_ptr(buf.original),
            in_use: false,
            block_number: buf.block_index,
            cache_index: buf.cache_index,
        });
        release(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bpin(buf: *mut BufferC) {
        let guard = Undroppable::new(BufferGuard {
            buffer: LockGuard::from_ptr((*buf).original),
            in_use: false,
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        });
        pin(&guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bunpin(buf: *mut BufferC) {
        let guard = Undroppable::new(BufferGuard {
            buffer: LockGuard::from_ptr((*buf).original),
            in_use: false,
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        });
        unpin(&guard);
        core::mem::forget(guard);
    }
}
