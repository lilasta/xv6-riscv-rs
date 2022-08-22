use core::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::NonNull,
};

use crate::{
    config::NBUF,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    virtio,
};

const BSIZE: usize = 1024;

struct Cache<const N: usize> {
    buffers: [SleepLock<Buffer>; N],
    links: [Link; N],
    head: NonNull<Link>,
    tail: NonNull<Link>,
}

impl<const N: usize> Cache<N> {
    const fn uninit() -> Self {
        Self {
            buffers: [const { SleepLock::new(Buffer::none()) }; _],
            links: [const { Link::dangling() }; _],
            head: NonNull::dangling(),
            tail: NonNull::dangling(),
        }
    }

    fn init(mut self: Pin<&mut Self>) {
        for (i, link) in self.links.iter_mut().enumerate() {
            link.index = i;
        }

        unsafe {
            let first = NonNull::new_unchecked(&mut self.links[0]);
            self.head = first;
            self.tail = first;
            self.links[0].next = first;
            self.links[0].prev = first;

            for i in 1..N {
                let ptr = NonNull::new_unchecked(&mut self.links[i]);
                self.links[i].prev = self.tail;
                self.links[i].next = self.head;
                self.head.as_mut().prev = ptr;
                self.tail.as_mut().next = ptr;
                self.tail = ptr;
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = &mut Link> {
        let mut next = Some(self.head);
        let end = self.tail;

        core::iter::from_fn(move || {
            let current = unsafe { next?.as_mut() };
            if next == Some(end) {
                next = None
            } else {
                next = Some(current.next);
            }
            Some(current)
        })
    }

    fn iter_rev(&self) -> impl Iterator<Item = &mut Link> {
        let mut next = Some(self.tail);
        let end = self.head;

        core::iter::from_fn(move || {
            let current = unsafe { next?.as_mut() };
            if next == Some(end) {
                next = None
            } else {
                next = Some(current.prev);
            }
            Some(current)
        })
    }

    pub fn get_or_allocate(
        &mut self,
        device: usize,
        block: usize,
    ) -> Option<(usize, &SleepLock<Buffer>, bool)> {
        for link in self.iter() {
            if link.ref_count > 0
                && link.device_number == Some(device)
                && link.block_number == Some(block)
            {
                link.ref_count += 1;
                return Some((link.index, &self.buffers[link.index], false));
            }
        }

        for link in self.iter_rev() {
            if link.ref_count == 0 {
                assert!(link.device_number.is_none());
                assert!(link.block_number.is_none());

                link.ref_count = 1;
                link.device_number = Some(device);
                link.block_number = Some(block);

                let buffer = &self.buffers[link.index];

                // TODO: 初期化忘れそうだから駄目
                unsafe {
                    let buffer = buffer.get_mut();
                    buffer.modified = false;
                }

                return Some((link.index, buffer, true));
            }
        }
        None
    }

    pub const fn pin(&mut self, index: usize) -> Option<()> {
        self.links.get_mut(index)?.ref_count += 1;
        Some(())
    }

    pub const fn unpin(&mut self, index: usize) -> Option<()> {
        self.links.get_mut(index)?.ref_count -= 1;
        Some(())
    }

    pub fn release(&mut self, index: usize) -> Option<bool> {
        let link = self.links.get_mut(index)?;

        link.ref_count -= 1;

        if link.ref_count == 0 {
            link.block_number = None;
            link.device_number = None;

            let me = unsafe { NonNull::new_unchecked(link) };

            if self.head.as_ptr() == link {
                self.head = link.next;
                self.tail = me;
            } else if self.tail.as_ptr() == link {
                // do nothing
            } else {
                unsafe {
                    link.next.as_mut().prev = link.prev;
                    link.prev.as_mut().next = link.next;
                    link.next = self.head;
                    link.prev = self.tail;

                    self.head.as_mut().prev = me;
                    self.tail.as_mut().next = me;
                    self.tail = me;
                }
            }
            Some(true)
        } else {
            Some(false)
        }
    }
}

struct Link {
    index: usize,
    ref_count: usize,
    device_number: Option<usize>,
    block_number: Option<usize>,
    next: NonNull<Self>,
    prev: NonNull<Self>,
}

impl Link {
    const fn dangling() -> Self {
        Self {
            index: 0,
            ref_count: 0,
            device_number: None,
            block_number: None,
            next: NonNull::dangling(),
            prev: NonNull::dangling(),
        }
    }
}

pub struct Buffer {
    on_rw: bool,
    modified: bool,
    data: [u8; BSIZE],
}

impl Buffer {
    const fn none() -> Self {
        Self {
            on_rw: false,
            modified: false,
            data: [0; _],
        }
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
    buffer: LockGuard<'static, SleepLock<Buffer>>,
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
        self.buffer.on_rw = true;
    }

    fn finish(&mut self) {
        self.buffer.on_rw = false;
    }

    fn is_finished(&self) -> bool {
        !self.buffer.on_rw
    }
}

impl Deref for BufferGuard {
    type Target = Buffer;

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
        if self.modified {
            self.modified = false;
            unsafe {
                virtio::disk::write(NonNull::new_unchecked(self));
            }
        }

        cache().lock().release(self.cache_index).unwrap();
    }
}

fn cache() -> &'static SpinLock<Cache<NBUF>> {
    static CACHE: SpinLock<Cache<NBUF>> = SpinLock::new(Cache::uninit());
    static INIT: SpinLock<bool> = SpinLock::new(false);

    let mut is_initialized = INIT.lock();
    if !*is_initialized {
        let mut cache = CACHE.lock();
        let cache = Pin::new(&mut *cache);
        cache.init();
        *is_initialized = true;
    }

    &CACHE
}

pub fn get(device: usize, block: usize) -> Option<BufferGuard> {
    let mut cache = cache().lock();
    let (index, buffer, is_allocated) = cache.get_or_allocate(device, block)?;
    let buffer = unsafe { &*(buffer as *const SleepLock<_>) };
    drop(cache);

    let mut buffer = BufferGuard {
        buffer: buffer.lock(),
        block_number: block,
        cache_index: index,
    };

    if is_allocated {
        unsafe {
            virtio::disk::read(NonNull::new_unchecked(&mut buffer));
        }
    }

    Some(buffer)
}

pub fn pin(guard: &BufferGuard) {
    cache().lock().pin(guard.cache_index);
}

pub fn unpin(guard: &BufferGuard) {
    cache().lock().unpin(guard.cache_index);
}

mod bindings {
    use super::*;

    #[repr(C)]
    struct BufferC {
        data: *mut u8,
        block_index: usize,
        cache_index: usize,
        original: *const SleepLock<Buffer>,
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
            buffer: LockGuard::from_ptr(buf.original),
            block_number: buf.block_index,
            cache_index: buf.cache_index,
        };
        drop(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bpin(buf: *mut BufferC) {
        let guard = BufferGuard {
            buffer: LockGuard::from_ptr((*buf).original),
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        };
        pin(&guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bunpin(buf: *mut BufferC) {
        let guard = BufferGuard {
            buffer: LockGuard::from_ptr((*buf).original),
            block_number: (*buf).block_index,
            cache_index: (*buf).cache_index,
        };
        unpin(&guard);
        core::mem::forget(guard);
    }
}
