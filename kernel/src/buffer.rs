use crate::{
    cache::CacheRc,
    config::NBUF,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    virtio,
};

pub const BSIZE: usize = 1024;

pub const fn check_buffer_conversion<T, const SIZE: usize>() -> usize {
    assert!(core::mem::size_of::<T>() <= SIZE);
    assert!(core::mem::needs_drop::<T>() == false);
    0
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
}

pub struct BufferGuard<'a, const BSIZE: usize, const CSIZE: usize> {
    cache: &'a BufferCache<BSIZE, CSIZE>,
    buffer: LockGuard<'a, SleepLock<Buffer<BSIZE>>>,
    is_valid: bool,
    in_use: bool,
    block_number: usize,
    cache_index: usize,
}

impl<'a, const BSIZE: usize, const CSIZE: usize> BufferGuard<'a, BSIZE, CSIZE> {
    pub const fn size(&self) -> usize {
        BSIZE
    }

    pub const fn block_number(&self) -> usize {
        self.block_number
    }

    pub fn pin(&self) {
        self.cache.pin(self);
    }

    pub fn unpin(&self) {
        self.cache.unpin(self);
    }

    pub fn clear(&mut self) {
        self.buffer.data.fill(0);
        self.is_valid = true;
    }

    pub unsafe fn read_array<T>(&mut self) -> &mut [T] {
        if !self.is_valid {
            virtio::disk::read(
                self.buffer.data.as_mut_ptr().addr(),
                self.block_number(),
                self.size(),
            );
            self.is_valid = true;
        }

        core::slice::from_raw_parts_mut(
            self.buffer.data.as_mut_ptr().cast::<T>(),
            BSIZE / core::mem::size_of::<T>(),
        )
    }

    pub unsafe fn read<T>(&mut self) -> &mut T
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        if !self.is_valid {
            virtio::disk::read(
                self.buffer.data.as_mut_ptr().addr(),
                self.block_number(),
                self.size(),
            );
            self.is_valid = true;
        }
        self.as_mut().unwrap()
    }

    pub unsafe fn write<T>(&mut self, src: T)
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        self.buffer.data.as_mut_ptr().cast::<T>().write(src);
        virtio::disk::write(
            self.buffer.data.as_mut_ptr().addr(),
            self.block_number(),
            self.size(),
        );
        self.is_valid = true;
    }

    pub unsafe fn as_ref<T>(&self) -> Option<&T>
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        if self.is_valid {
            self.buffer.data.as_ptr().cast::<T>().as_ref()
        } else {
            None
        }
    }

    pub unsafe fn as_mut<T>(&mut self) -> Option<&mut T>
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        if self.is_valid {
            self.buffer.data.as_mut_ptr().cast::<T>().as_mut()
        } else {
            None
        }
    }

    pub unsafe fn read_array_with_unlock<T, L: Lock>(
        &mut self,
        lock: &mut LockGuard<L>,
    ) -> &mut [T] {
        Lock::unlock_temporarily(lock, || self.read_array::<T>())
    }

    pub unsafe fn read_with_unlock<T, L: Lock>(&mut self, lock: &mut LockGuard<L>) -> &mut T
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        Lock::unlock_temporarily(lock, || self.read::<T>())
    }

    pub unsafe fn write_with_unlock<T, L: Lock>(&mut self, src: T, lock: &mut LockGuard<L>)
    where
        [(); check_buffer_conversion::<T, BSIZE>()]:,
    {
        Lock::unlock_temporarily(lock, || self.write(src));
    }
}

impl<'a, const BSIZE: usize, const CSIZE: usize> Drop for BufferGuard<'a, BSIZE, CSIZE> {
    fn drop(&mut self) {
        self.cache.release(self);
    }
}

pub struct BufferCache<const BSIZE: usize, const CSIZE: usize> {
    buffers: [SleepLock<Buffer<BSIZE>>; CSIZE],
    cache: SpinLock<CacheRc<BufferKey, CSIZE>>,
}

impl<const BSIZE: usize, const CSIZE: usize> BufferCache<BSIZE, CSIZE> {
    pub const fn new() -> Self {
        Self {
            buffers: [const { SleepLock::new(Buffer::zeroed()) }; _],
            cache: SpinLock::new(CacheRc::new()),
        }
    }

    pub fn get(&self, device: usize, block: usize) -> Option<BufferGuard<BSIZE, CSIZE>> {
        let (index, is_new) = self.cache.lock().get(BufferKey { device, block })?;

        Some(BufferGuard {
            cache: self,
            buffer: self.buffers[index].lock(),
            is_valid: !is_new,
            in_use: false,
            block_number: block,
            cache_index: index,
        })
    }

    fn release(&self, buffer: &BufferGuard<BSIZE, CSIZE>) {
        self.cache.lock().release(buffer.cache_index).unwrap();
    }

    pub fn pin(&self, guard: &BufferGuard<BSIZE, CSIZE>) {
        self.cache.lock().pin(guard.cache_index).unwrap();
    }

    pub fn unpin(&self, guard: &BufferGuard<BSIZE, CSIZE>) {
        self.cache.lock().unpin(guard.cache_index).unwrap();
    }
}

static CACHE: BufferCache<BSIZE, NBUF> = BufferCache::new();

pub fn get(device: usize, block: usize) -> Option<BufferGuard<'static, BSIZE, NBUF>> {
    CACHE.get(device, block)
}
