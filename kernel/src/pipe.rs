use core::{mem::MaybeUninit, ptr::NonNull};

use crate::{
    allocator::KernelAllocator,
    lock::{spin::SpinLock, Lock, LockGuard},
    process,
    vm::PageTableExtension,
};

struct PipeInner<const SIZE: usize> {
    data: [u8; SIZE],
    read: usize,
    write: usize,
    read_open: bool,
    write_open: bool,
}

impl<const SIZE: usize> PipeInner<SIZE> {
    pub const fn new() -> Self {
        Self {
            data: [0; _],
            read: 0,
            write: 0,
            read_open: true,
            write_open: true,
        }
    }

    pub const fn is_used(&self) -> bool {
        !self.write_open && !self.read_open
    }

    pub fn close_read(&mut self) {
        self.read_open = false;
        process::wakeup(core::ptr::addr_of!(self.write).addr());
    }

    pub fn close_write(&mut self) {
        self.write_open = false;
        process::wakeup(core::ptr::addr_of!(self.read).addr());
    }

    pub fn write(self: &mut LockGuard<SpinLock<Self>>, addr: usize, n: usize) -> Result<usize, ()> {
        let mut i = 0;
        while i < n {
            if !self.read_open || process::is_killed() == Some(true) {
                return Err(());
            }

            if self.write == self.read + SIZE {
                process::wakeup(core::ptr::addr_of!(self.read).addr());
                process::sleep(core::ptr::addr_of!(self.write).addr(), self);
            } else {
                match process::read_memory(addr + i) {
                    Some(ch) => {
                        let index = self.write % SIZE;
                        self.data[index] = ch;
                        self.write += 1;
                        i += 1;
                    }
                    None => break,
                }
            }
        }
        process::wakeup(core::ptr::addr_of!(self.read).addr());

        Ok(i)
    }

    pub fn read(self: &mut LockGuard<SpinLock<Self>>, addr: usize, n: usize) -> Result<usize, ()> {
        while self.read == self.write && self.write_open {
            if process::is_killed() == Some(true) {
                return Err(());
            }
            process::sleep(core::ptr::addr_of!(self.read).addr(), self);
        }

        let mut total_read = 0;
        for i in 0..n {
            if self.read == self.write {
                break;
            }

            let ch = self.data[self.read % SIZE];
            self.read += 1;

            // TODO: process::write_memorY
            if unsafe {
                process::context()
                    .unwrap()
                    .pagetable
                    .write(addr + i, &ch)
                    .is_err()
            } {
                break;
            }

            total_read += 1;
        }

        process::wakeup(core::ptr::addr_of!(self.write).addr());
        Ok(total_read)
    }
}

// TODO: Remove repr
#[repr(C)]
pub struct Pipe<const SIZE: usize> {
    inner: NonNull<SpinLock<PipeInner<SIZE>>>,
    write: bool,
    dropped: bool, // TODO: delete this HACK
}

impl<const SIZE: usize> Pipe<SIZE> {
    pub fn allocate() -> Option<(Self, Self)> {
        let mut inner: NonNull<MaybeUninit<_>> = KernelAllocator::get().allocate()?;
        let inner = unsafe {
            inner
                .as_mut()
                .write(SpinLock::new(PipeInner::<SIZE>::new()));
            inner.cast()
        };
        let read = Self {
            inner,
            write: false,
            dropped: false,
        };
        let write = Self {
            inner,
            write: true,
            dropped: false,
        };
        Some((read, write))
    }

    pub fn write(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self.write {
            true => unsafe { self.inner.as_ref().lock().write(addr, n) },
            false => Err(()),
        }
    }

    pub fn read(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self.write {
            true => Err(()),
            false => unsafe { self.inner.as_ref().lock().read(addr, n) },
        }
    }
}

impl<const SIZE: usize> Drop for Pipe<SIZE> {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }

        let mut inner = unsafe { self.inner.as_ref().lock() };
        match self.write {
            true => inner.close_write(),
            false => inner.close_read(),
        }

        if inner.is_used() {
            drop(inner);
            KernelAllocator::get().deallocate(self.inner);
        }

        self.dropped = true;
    }
}

mod binding {
    use crate::file::PIPESIZE;

    use super::*;

    #[no_mangle]
    unsafe extern "C" fn pipeclose(pipe: *mut Pipe<PIPESIZE>, _: i32) {
        core::ptr::drop_in_place(pipe);
    }

    #[no_mangle]
    unsafe extern "C" fn pipewrite(pipe: *mut Pipe<PIPESIZE>, addr: usize, n: u32) -> i32 {
        match (*pipe).write(addr, n as usize) {
            Ok(i) => i as i32,
            Err(_) => -1,
        }
    }

    #[no_mangle]
    unsafe extern "C" fn piperead(pipe: *mut Pipe<PIPESIZE>, addr: usize, n: u32) -> i32 {
        match (*pipe).read(addr, n as usize) {
            Ok(i) => i as i32,
            Err(_) => -1,
        }
    }
}
