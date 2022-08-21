use core::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    config::NBUF,
    lock::{sleep::SleepLock, spin::SpinLock, Lock, LockGuard},
    virtio,
};

const BSIZE: usize = 1024;

struct Cache {
    buffers: [SleepLock<Buffer>; NBUF],
    links: [Link; NBUF],
    head: NonNull<Link>,
    tail: NonNull<Link>,
}

impl Cache {
    fn init(&mut self) {
        for (i, link) in self.links.iter_mut().enumerate() {
            link.index = i;
            link.ref_count = 0;
        }

        for (i, buf) in self.buffers.iter().enumerate() {
            unsafe { buf.get_mut().cache_index = i };
        }

        unsafe {
            let first = NonNull::new_unchecked(&mut self.links[0]);
            self.head = first;
            self.tail = first;
            self.links[0].next = first;
            self.links[0].prev = first;

            for link in self.links.iter_mut().skip(1) {
                let ptr = NonNull::new_unchecked(link);
                link.prev = self.tail;
                link.next = self.head;
                self.head.as_mut().prev = ptr;
                self.tail.as_mut().next = ptr;
                self.tail = ptr;
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = NonNull<Link>> {
        let mut next = Some(self.head);
        let end = self.tail;

        core::iter::from_fn(move || {
            let current = next?;
            if next == Some(end) {
                next = None
            } else {
                next = unsafe { Some(current.as_ref().next) };
            }
            Some(current)
        })
    }

    fn iter_rev(&self) -> impl Iterator<Item = NonNull<Link>> {
        let mut next = Some(self.tail);
        let end = self.head;

        core::iter::from_fn(move || {
            let current = next?;
            if next == Some(end) {
                next = None
            } else {
                next = unsafe { Some(current.as_ref().prev) };
            }
            Some(current)
        })
    }

    pub fn get_or_allocate(
        &mut self,
        device: usize,
        block: usize,
    ) -> Option<(&SleepLock<Buffer>, bool)> {
        for mut link in self.iter() {
            let link = unsafe { link.as_mut() };
            let buffer = self.buffers.get(link.index)?;

            if unsafe { buffer.get().device_number == device && buffer.get().block_number == block }
            {
                link.ref_count += 1;
                return Some((buffer, false));
            }
        }

        for mut link in self.iter_rev() {
            let link = unsafe { link.as_mut() };
            if link.ref_count == 0 {
                link.ref_count = 1;

                let buffer = self.buffers.get(link.index)?;

                // TODO: 初期化忘れそうだから駄目
                unsafe {
                    let buffer = buffer.get_mut();
                    buffer.device_number = device;
                    buffer.block_number = block;
                    buffer.modified = false;
                }

                return Some((buffer, true));
            }
        }
        None
    }
}

struct Link {
    index: usize,
    ref_count: usize,
    next: NonNull<Self>,
    prev: NonNull<Self>,
}

impl Link {
    const fn dangling() -> Self {
        Self {
            index: 0,
            ref_count: 0,
            next: NonNull::dangling(),
            prev: NonNull::dangling(),
        }
    }
}

pub struct Buffer {
    cache_index: usize,
    device_number: usize,
    block_number: usize,
    on_rw: bool,
    modified: bool,
    data: [u8; BSIZE],
}

impl Buffer {
    const fn none() -> Self {
        Self {
            cache_index: 0,
            device_number: 0,
            block_number: 0,
            on_rw: false,
            modified: false,
            data: [0; _],
        }
    }

    pub const fn mark_modified(&mut self) {
        self.modified = true;
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

impl virtio::disk::Buffer for Buffer {
    fn block_number(&self) -> usize {
        self.block_number
    }

    fn size(&self) -> usize {
        BSIZE
    }

    fn addr(&self) -> usize {
        self.data.as_ptr().addr()
    }

    fn start(&mut self) {
        self.on_rw = true;
    }

    fn finish(&mut self) {
        self.on_rw = false;
    }

    fn is_finished(&self) -> bool {
        !self.on_rw
    }
}

pub struct BufferGuard<'a>(LockGuard<'a, SleepLock<Buffer>>);

impl<'a> Deref for BufferGuard<'a> {
    type Target = Buffer;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for BufferGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'a> Drop for BufferGuard<'a> {
    fn drop(&mut self) {
        let mut cache = cache().lock();

        let link = unsafe { &mut *(&mut cache.links[self.cache_index] as *mut Link) };

        link.ref_count -= 1;

        if link.ref_count == 0 {
            unsafe {
                let me = NonNull::new_unchecked(link);

                if cache.head.as_ptr() == link {
                    cache.head = link.next;
                    cache.tail = me;
                } else if cache.tail.as_ptr() == link {
                    // do nothing
                } else {
                    link.next.as_mut().prev = link.prev;
                    link.prev.as_mut().next = link.next;
                    link.next = cache.head;
                    link.prev = cache.tail;

                    cache.head.as_mut().prev = me;
                    cache.tail.as_mut().next = me;
                    cache.tail = me;
                }
            }

            Lock::unlock(cache);

            // Lazy writing
            if self.modified {
                unsafe {
                    let ptr = NonNull::new_unchecked(self.deref_mut());
                    virtio::disk::write(ptr);
                }
            }
        }
    }
}

fn cache() -> &'static SpinLock<Cache> {
    static CACHE: SpinLock<Cache> = SpinLock::new(Cache {
        buffers: [const { SleepLock::new(Buffer::none()) }; _],
        links: [const { Link::dangling() }; _],
        head: NonNull::dangling(),
        tail: NonNull::dangling(),
    });
    static INIT: SpinLock<bool> = SpinLock::new(false);

    let mut is_initialized = INIT.lock();
    if !*is_initialized {
        CACHE.lock().init();
        *is_initialized = true;
    }

    &CACHE
}

pub fn get(device: usize, block: usize) -> Option<BufferGuard<'static>> {
    let mut cache = cache().lock();
    let (buffer, is_allocated) = cache.get_or_allocate(device, block)?;
    let buffer = unsafe { &*(buffer as *const SleepLock<_>) };
    drop(cache);

    let mut buffer = BufferGuard(buffer.lock());

    if is_allocated {
        unsafe {
            let ptr = NonNull::new_unchecked(&mut *buffer);
            virtio::disk::read(ptr);
        }
    }

    Some(buffer)
}

pub fn pin(buffer: &Buffer) {
    cache().lock().links[buffer.cache_index].ref_count += 1;
}

pub fn unpin(buffer: &Buffer) {
    cache().lock().links[buffer.cache_index].ref_count -= 1;
}

mod bindings {
    use super::*;

    #[repr(C)]
    struct BufferC {
        data: *mut u8,
        blockno: u32,
        original: *const SleepLock<Buffer>,
    }

    #[no_mangle]
    extern "C" fn binit() {}

    #[no_mangle]
    unsafe extern "C" fn bread(device: u32, block: u32) -> BufferC {
        let mut buf = get(device as _, block as _).unwrap();

        let ret = BufferC {
            data: buf.data.as_mut_ptr(),
            blockno: block,
            original: LockGuard::as_ptr(&buf.0),
        };

        core::mem::forget(buf);

        ret
    }

    #[no_mangle]
    unsafe extern "C" fn bwrite(buf: *mut BufferC) {
        (*(*buf).original).get_mut().mark_modified()
    }

    #[no_mangle]
    unsafe extern "C" fn brelse(buf: BufferC) {
        let guard = BufferGuard(LockGuard::from_ptr(buf.original));
        drop(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bpin(buf: *mut BufferC) {
        let guard = BufferGuard(LockGuard::from_ptr((*buf).original));
        pin(&guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bunpin(buf: *mut BufferC) {
        let guard = BufferGuard(LockGuard::from_ptr((*buf).original));
        unpin(&guard);
        core::mem::forget(guard);
    }
}
