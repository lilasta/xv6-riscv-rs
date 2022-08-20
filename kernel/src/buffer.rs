use core::{mem::MaybeUninit, ptr::NonNull};

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

    pub fn get(&mut self, dev: usize, block: usize) -> Option<(usize, &SleepLock<Buffer>)> {
        for mut link in self.iter() {
            unsafe {
                let buf = &self.buffers[link.as_ref().index];
                if buf.get().device_number == dev && buf.get().block_number == block {
                    link.as_mut().ref_count += 1;
                    return Some((link.as_ref().index, buf));
                }
            }
        }

        for mut link in self.iter_rev() {
            unsafe {
                if link.as_ref().ref_count == 0 {
                    link.as_mut().ref_count = 1;

                    let buf = &self.buffers[link.as_ref().index];
                    buf.get_mut().device_number = dev;
                    buf.get_mut().block_number = block;
                    buf.get_mut().is_valid = false;
                    return Some((link.as_ref().index, buf));
                }
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

struct Buffer {
    device_number: usize,
    block_number: usize,
    on_rw: bool,
    is_valid: bool,
    data: [u8; BSIZE],
}

impl Buffer {
    const fn none() -> Self {
        Self {
            device_number: 0,
            block_number: 0,
            on_rw: false,
            is_valid: false,
            data: [0; _],
        }
    }

    pub fn data<T>(&mut self) -> &mut MaybeUninit<T> {
        assert!(BSIZE >= core::mem::size_of::<T>());
        let ptr = self.data.as_mut_ptr();
        let ptr = ptr.cast::<MaybeUninit<T>>();
        unsafe { &mut *ptr }
    }

    pub fn read(this: &mut Self) {
        if !this.is_valid {
            unsafe {
                let ptr = NonNull::new_unchecked(this);
                virtio::disk::read(ptr);
            }
            this.is_valid = true;
        }
    }

    pub fn write(this: &mut Self) {
        unsafe {
            let ptr = NonNull::new_unchecked(this);
            virtio::disk::write(ptr);
        }
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

struct BufferGuard<'a> {
    index: usize,
    buffer: LockGuard<'a, SleepLock<Buffer>>,
}

impl<'a> Drop for BufferGuard<'a> {
    fn drop(&mut self) {
        let mut cache = cache().lock();

        let link = unsafe { &mut *(&mut cache.links[self.index] as *mut Link) };

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
        *is_initialized = true;
        CACHE.lock().init();
    }

    &CACHE
}

fn pin(guard: &BufferGuard) {
    cache().lock().links[guard.index].ref_count += 1;
}

fn unpin(guard: &BufferGuard) {
    cache().lock().links[guard.index].ref_count -= 1;
}

mod bindings {
    use super::*;

    #[repr(C)]
    struct BufferC {
        data: *mut u8,
        index: u64,
        blockno: u32,
        original: *const SleepLock<Buffer>,
    }

    #[no_mangle]
    extern "C" fn binit() {}

    #[no_mangle]
    unsafe extern "C" fn bread(dev: u32, block: u32) -> BufferC {
        let mut cache = cache().lock();
        let (index, buf) = cache.get(dev as _, block as _).unwrap();
        let buf = &*(buf as *const SleepLock<_>);
        drop(cache);

        let mut buf = BufferGuard {
            index,
            buffer: buf.lock(),
        };

        Buffer::read(&mut buf.buffer);

        let ret = BufferC {
            data: buf.buffer.data.as_mut_ptr(),
            index: buf.index as _,
            blockno: buf.buffer.block_number as _,
            original: LockGuard::as_ptr(&buf.buffer).cast(),
        };

        core::mem::forget(buf);

        ret
    }

    #[no_mangle]
    unsafe extern "C" fn bwrite(buf: *mut BufferC) {
        Buffer::write((*(*buf).original).get_mut())
    }

    #[no_mangle]
    unsafe extern "C" fn brelse(buf: BufferC) {
        let guard = BufferGuard {
            index: buf.index as _,
            buffer: LockGuard::from_ptr(buf.original),
        };
        drop(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bpin(buf: *mut BufferC) {
        let mut guard = BufferGuard {
            index: (*buf).index as _,
            buffer: LockGuard::from_ptr((*buf).original),
        };
        pin(&mut guard);
        core::mem::forget(guard);
    }

    #[no_mangle]
    unsafe extern "C" fn bunpin(buf: *mut BufferC) {
        let mut guard = BufferGuard {
            index: (*buf).index as _,
            buffer: LockGuard::from_ptr((*buf).original),
        };
        unpin(&mut guard);
        core::mem::forget(guard);
    }
}
