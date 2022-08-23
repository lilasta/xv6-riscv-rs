use core::{
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    cache::CacheRc,
    config::NBUF,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    virtio,
};

pub const BSIZE: usize = 1024;

#[derive(PartialEq, Eq)]
struct BufferKey {
    device: usize,
    block: usize,
}

pub struct Buffer<const SIZE: usize> {
    in_use: bool,
    modified: bool,
    data: [u8; SIZE],
}

impl<const SIZE: usize> Buffer<SIZE> {
    pub const fn empty() -> Self {
        Self {
            in_use: false,
            modified: false,
            data: [0; _],
        }
    }

    pub const fn size(&self) -> usize {
        SIZE
    }

    pub const fn as_ptr<T>(&self) -> Option<*const T> {
        if core::mem::size_of::<T>() > self.data.len() {
            return None;
        }

        Some(self.data.as_ptr().cast())
    }

    pub const fn as_mut_ptr<T>(&mut self) -> Option<*mut T> {
        if core::mem::size_of::<T>() > self.data.len() {
            return None;
        }

        Some(self.data.as_mut_ptr().cast())
    }

    pub const fn as_uninit<T>(&self) -> Option<&MaybeUninit<T>> {
        if core::mem::size_of::<T>() > self.data.len() {
            return None;
        }

        let ptr = self.data.as_ptr();
        let ptr = ptr.cast::<MaybeUninit<T>>();
        Some(unsafe { &*ptr })
    }

    pub const fn as_uninit_mut<T>(&mut self) -> Option<&mut MaybeUninit<T>> {
        if core::mem::size_of::<T>() > self.data.len() {
            return None;
        }

        let ptr = self.data.as_mut_ptr();
        let ptr = ptr.cast::<MaybeUninit<T>>();
        Some(unsafe { &mut *ptr })
    }
}

pub struct BufferGuard {
    buffer: ManuallyDrop<LockGuard<'static, SleepLock<Buffer<BSIZE>>>>,
    block_number: usize,
    cache_index: usize,
}

impl virtio::disk::Buffer for BufferGuard {
    fn block_number(&self) -> usize {
        self.block_number
    }

    fn size(&self) -> usize {
        self.buffer.data.len()
    }

    fn addr(&self) -> usize {
        self.buffer.data.as_ptr().addr()
    }

    fn start(&mut self) {
        self.buffer.in_use = true;
    }

    fn finish(&mut self) {
        self.buffer.in_use = false;
    }

    fn is_finished(&self) -> bool {
        !self.buffer.in_use
    }
}

impl Deref for BufferGuard {
    type Target = Buffer<BSIZE>;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for BufferGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer.modified = true;
        &mut self.buffer
    }
}

impl Drop for BufferGuard {
    fn drop(&mut self) {
        if self.buffer.modified {
            self.buffer.modified = false;
            unsafe {
                virtio::disk::write(NonNull::new_unchecked(self));
            }
        }

        unsafe { ManuallyDrop::drop(&mut self.buffer) };

        cache().release(self.cache_index).unwrap();
    }
}

fn cache() -> LockGuard<'static, SpinLock<CacheRc<BufferKey, NBUF>>> {
    static CACHE: SpinLock<CacheRc<BufferKey, NBUF>> = SpinLock::new(CacheRc::new());
    CACHE.lock()
}

pub fn get(device: usize, block: usize) -> Option<BufferGuard> {
    static BUFFERS: [SleepLock<Buffer<BSIZE>>; NBUF] =
        [const { SleepLock::new(Buffer::empty()) }; _];

    let (index, is_new) = cache().get(BufferKey { device, block })?;

    let mut guard = BufferGuard {
        buffer: ManuallyDrop::new(BUFFERS[index].lock()),
        block_number: block,
        cache_index: index,
    };

    if is_new {
        unsafe {
            virtio::disk::read(NonNull::new_unchecked(&mut guard));
        }
        guard.buffer.modified = false;
    }

    Some(guard)
}

pub fn pin(guard: &BufferGuard) {
    cache().pin(guard.cache_index);
}

pub fn unpin(guard: &BufferGuard) {
    cache().unpin(guard.cache_index);
}

mod bindings {
    use super::*;

    #[repr(C)]
    struct BufferC {
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
    unsafe extern "C" fn bwrite(buf: *mut BufferC) {
        (*(*buf).original).get_mut().modified = true;
    }

    #[no_mangle]
    unsafe extern "C" fn brelse(buf: BufferC) {
        let guard = BufferGuard {
            buffer: ManuallyDrop::new(LockGuard::from_ptr(buf.original)),
            block_number: buf.block_index,
            cache_index: buf.cache_index,
        };
        drop(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bpin(buf: *mut BufferC) {
        let guard = BufferGuard {
            buffer: ManuallyDrop::new(LockGuard::from_ptr((*buf).original)),
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        };
        pin(&guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bunpin(buf: *mut BufferC) {
        let guard = BufferGuard {
            buffer: ManuallyDrop::new(LockGuard::from_ptr((*buf).original)),
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        };
        unpin(&guard);
        core::mem::forget(guard);
    }
}
