use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use crate::{
    cache::RcCache,
    config::NBUF,
    sleeplock::{SleepLock, SleepLockGuard},
    spinlock::SpinLock,
    virtio,
};

pub const BSIZE: usize = 1024;

const fn check_convertibility<T, const SIZE: usize>() {
    assert!(core::mem::size_of::<T>() <= SIZE);
    assert!(!core::mem::needs_drop::<T>());
}

#[derive(PartialEq, Eq)]
struct BufferKey {
    device: usize,
    block: usize,
}

pub struct Buffer<'a, T, const BSIZE: usize, const CSIZE: usize> {
    cache: &'a BufferCache<BSIZE, CSIZE>,
    buffer: SleepLockGuard<[u8; BSIZE]>,
    block_number: usize,
    cache_index: usize,
    phantom: PhantomData<T>,
}

impl<'a, T, const BSIZE: usize, const CSIZE: usize> Buffer<'a, T, BSIZE, CSIZE> {
    pub const fn block_number(this: &Self) -> usize {
        this.block_number
    }
}

impl<'a, T, const BSIZE: usize, const CSIZE: usize> Deref for Buffer<'a, T, BSIZE, CSIZE> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.buffer.as_ptr().cast::<T>().as_ref().unwrap_unchecked() }
    }
}

impl<'a, T, const BSIZE: usize, const CSIZE: usize> DerefMut for Buffer<'a, T, BSIZE, CSIZE> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            self.buffer
                .as_mut_ptr()
                .cast::<T>()
                .as_mut()
                .unwrap_unchecked()
        }
    }
}

impl<'a, T, const BSIZE: usize, const CSIZE: usize> Drop for Buffer<'a, T, BSIZE, CSIZE> {
    fn drop(&mut self) {
        self.cache.release(self.cache_index);
    }
}

pub struct BufferCache<const BSIZE: usize, const CSIZE: usize> {
    buffers: [SleepLock<[u8; BSIZE]>; CSIZE],
    cache: SpinLock<RcCache<BufferKey, CSIZE>>,
}

impl<const BSIZE: usize, const CSIZE: usize> BufferCache<BSIZE, CSIZE> {
    pub const fn new() -> Self {
        Self {
            // TODO:
            //buffers: [const { SleepLock::new([0; _]) }; _],
            buffers: [const { SleepLock::new(unsafe { core::mem::MaybeUninit::zeroed().assume_init() }) };
                _],
            cache: SpinLock::new(RcCache::new()),
        }
    }

    /// バッファを取得します。
    /// もし目的のバッファが使用中であればスリープして待機するため、
    /// スピンロックを保持している場合はこの関数を使用する前に解除する必要があります。
    fn get(
        &'static self,
        device: usize,
        block: usize,
    ) -> Option<(usize, SleepLockGuard<[u8; BSIZE]>, bool)> {
        // キャッシュのロックを保持しておきます
        let mut cache = self.cache.lock();

        // index: 目的のバッファのインデックス
        // is_new: それが新規のバッファであるかどうか
        let (index, is_new) = cache.get(BufferKey { device, block })?;

        // ロックを外せるようになったので外します
        // ここで外し忘れると（値の破棄順序によっては）、キャッシュをロックしたまま
        // バッファのロックをスリープで待機し、デッドロックが起こる危険があります
        // （🔒の箇所）
        SpinLock::unlock(cache);

        Some((
            index,
            self.buffers[index].lock(), // 🔒
            is_new,
        ))
    }

    unsafe fn with_read<T>(
        &'static self,
        device: usize,
        block: usize,
    ) -> Option<Buffer<'static, T, BSIZE, CSIZE>> {
        const { check_convertibility::<T, BSIZE>() };

        let (index, mut buffer, is_uninit) = self.get(device, block)?;

        if is_uninit {
            unsafe { virtio::disk::read(buffer.as_mut_ptr().addr(), block, BSIZE) };
        }

        Some(Buffer {
            cache: self,
            buffer,
            block_number: block,
            cache_index: index,
            phantom: PhantomData,
        })
    }

    fn with_write<T>(
        &'static self,
        device: usize,
        block: usize,
        src: &T,
    ) -> Option<Buffer<'static, T, BSIZE, CSIZE>> {
        const { check_convertibility::<T, BSIZE>() };

        let (index, mut buffer, _) = self.get(device, block)?;

        unsafe {
            buffer.as_mut_ptr().cast::<T>().copy_from(src, 1);
        }

        Some(Buffer {
            cache: self,
            buffer,
            block_number: block,
            cache_index: index,
            phantom: PhantomData,
        })
    }

    fn release(&self, index: usize) {
        self.cache.lock().release(index).unwrap();
    }

    fn pin(&self, index: usize) {
        self.cache.lock().duplicate(index).unwrap();
    }

    fn unpin(&self, index: usize) {
        let is_released = self.cache.lock().release(index).unwrap();
        assert!(!is_released)
    }
}

static CACHE: BufferCache<BSIZE, NBUF> = BufferCache::new();

pub unsafe fn with_read<T>(device: usize, block: usize) -> Option<Buffer<'static, T, BSIZE, NBUF>> {
    CACHE.with_read(device, block)
}

pub fn with_write<T>(
    device: usize,
    block: usize,
    src: &T,
) -> Option<Buffer<'static, T, BSIZE, NBUF>> {
    CACHE.with_write(device, block, src)
}

pub unsafe fn flush<T: 'static>(mut buffer: Buffer<'static, T, BSIZE, NBUF>) {
    virtio::disk::write(
        buffer.buffer.as_mut_ptr().addr(),
        buffer.block_number,
        BSIZE,
    );
}

pub fn pin<T>(buffer: &Buffer<T, BSIZE, NBUF>) {
    CACHE.pin(buffer.cache_index);
}

pub fn unpin<T>(buffer: &Buffer<T, BSIZE, NBUF>) {
    CACHE.unpin(buffer.cache_index);
}
