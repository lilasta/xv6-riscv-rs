use crate::{
    cache::CacheRc,
    config::NBUF,
    sleeplock::{SleepLock, SleepLockGuard},
    spinlock::{SpinLock, SpinLockGuard},
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

struct Buffer<const SIZE: usize> {
    data: [u8; SIZE],
    is_initialized: bool,
}

impl<const SIZE: usize> Buffer<SIZE> {
    const fn zeroed() -> Self {
        Self {
            data: [0; _],
            is_initialized: false,
        }
    }
}

pub struct BufferGuard<'a, const BSIZE: usize, const CSIZE: usize> {
    cache: &'a BufferCache<BSIZE, CSIZE>,
    buffer: SleepLockGuard<'a, Buffer<BSIZE>>,
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
        self.cache.pin(self.cache_index);
    }

    pub fn unpin(&self) {
        self.cache.unpin(self.cache_index);
    }

    pub fn clear(&mut self) {
        self.buffer.data.fill(0);
        self.buffer.is_initialized = true;
    }

    pub unsafe fn read_array<T>(&mut self) -> &mut [T] {
        if !self.buffer.is_initialized {
            virtio::disk::read(
                self.buffer.data.as_mut_ptr().addr(),
                self.block_number(),
                self.size(),
            );
            self.buffer.is_initialized = true;
        }

        core::slice::from_raw_parts_mut(
            self.buffer.data.as_mut_ptr().cast::<T>(),
            BSIZE / core::mem::size_of::<T>(),
        )
    }

    pub unsafe fn read<T>(&mut self) -> &mut T {
        const { check_convertibility::<T, BSIZE>() };

        if !self.buffer.is_initialized {
            virtio::disk::read(
                self.buffer.data.as_mut_ptr().addr(),
                self.block_number(),
                self.size(),
            );
            self.buffer.is_initialized = true;
        }

        &mut *self.buffer.data.as_mut_ptr().cast::<T>()
    }

    pub unsafe fn write<T>(&mut self, src: &T) {
        const { check_convertibility::<T, BSIZE>() };

        self.buffer.data.as_mut_ptr().cast::<T>().copy_from(src, 1);
        virtio::disk::write(
            self.buffer.data.as_mut_ptr().addr(),
            self.block_number(),
            self.size(),
        );
        self.buffer.is_initialized = true;
    }

    pub unsafe fn read_with_unlock<T, LT>(&mut self, lock: &mut SpinLockGuard<LT>) -> &mut T {
        const { check_convertibility::<T, BSIZE>() };

        SpinLock::unlock_temporarily(lock, || self.read::<T>())
    }

    pub unsafe fn write_with_unlock<T, LT>(&mut self, src: &T, lock: &mut SpinLockGuard<LT>) {
        const { check_convertibility::<T, BSIZE>() };

        SpinLock::unlock_temporarily(lock, || self.write(src));
    }
}

impl<'a, const BSIZE: usize, const CSIZE: usize> Drop for BufferGuard<'a, BSIZE, CSIZE> {
    fn drop(&mut self) {
        self.cache.release(self.cache_index);
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

    /// バッファを取得します。
    /// もし目的のバッファが使用中であればスリープして待機するため、
    /// スピンロックを保持している場合はこの関数を使用する前に解除する必要があります。
    pub fn get(&self, device: usize, block: usize) -> Option<BufferGuard<BSIZE, CSIZE>> {
        // キャッシュのロックを保持しておきます
        let mut cache = self.cache.lock();

        // index: 目的のバッファのインデックス
        // is_new: それが新規のバッファであるかどうか
        let (index, is_new) = cache.get(BufferKey { device, block })?;

        // この条件判定はキャッシュがロックされているうちに行います
        if is_new {
            // まだディスクからの読み込みがされていないため、初期化済を示すフラグを偽にします
            // 新しいバッファであり、誰もロックしていないので待機は起こりません
            // （そのため、デッドロックの心配はありません）
            //
            // この処理はバッファを解放する際に行ってもおそらく構いません（解放したバッファが最後の参照であればフラグを偽に）
            self.buffers[index].lock().is_initialized = false;
        }

        // ロックを外せるようになったので外します
        // ここで外し忘れると（値の破棄順序によっては）、キャッシュをロックしたまま
        // バッファのロックをスリープで待機し、デッドロックが起こる危険があります
        // （🔒の箇所）
        SpinLock::unlock(cache);

        Some(BufferGuard {
            cache: self,
            buffer: self.buffers[index].lock(), // 🔒
            block_number: block,
            cache_index: index,
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

pub fn get(device: usize, block: usize) -> Option<BufferGuard<'static, BSIZE, NBUF>> {
    CACHE.get(device, block)
}
