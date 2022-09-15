use alloc::sync::Arc;

use crate::{
    process,
    spinlock::{SpinLock, SpinLockGuard},
};

#[derive(Debug)]
pub enum Pipe<const SIZE: usize> {
    Read(Arc<SpinLock<PipeInner<SIZE>>>),
    Write(Arc<SpinLock<PipeInner<SIZE>>>),
}

impl<const SIZE: usize> Pipe<SIZE> {
    pub fn allocate() -> Option<(Self, Self)> {
        let inner = Arc::new(SpinLock::new(PipeInner::<SIZE>::new()));
        let read = Self::Read(inner.clone());
        let write = Self::Write(inner);
        Some((read, write))
    }

    pub fn write(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self {
            Self::Read(_) => Err(()),
            Self::Write(inner) => inner.lock().write(addr, n),
        }
    }

    pub fn read(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self {
            Self::Read(inner) => inner.lock().read(addr, n),
            Self::Write(_) => Err(()),
        }
    }
}

impl<const SIZE: usize> Drop for Pipe<SIZE> {
    fn drop(&mut self) {
        match self {
            Self::Read(inner) => inner.lock().close_read(),
            Self::Write(inner) => inner.lock().close_write(),
        }
    }
}

#[derive(Debug)]
pub struct PipeInner<const SIZE: usize> {
    data: [u8; SIZE],
    read: usize,
    write: usize,
    read_open: bool,
    write_open: bool,
}

impl<const SIZE: usize> PipeInner<SIZE> {
    const fn new() -> Self {
        Self {
            data: [0; _],
            read: 0,
            write: 0,
            read_open: true,
            write_open: true,
        }
    }

    fn close_read(&mut self) {
        assert!(self.read_open);
        self.read_open = false;
        process::wakeup(core::ptr::addr_of!(self.write).addr());
    }

    fn close_write(&mut self) {
        assert!(self.write_open);
        self.write_open = false;
        process::wakeup(core::ptr::addr_of!(self.read).addr());
    }

    fn write(self: &mut SpinLockGuard<Self>, addr: usize, n: usize) -> Result<usize, ()> {
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

    fn read(self: &mut SpinLockGuard<Self>, addr: usize, n: usize) -> Result<usize, ()> {
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

            if !process::write_memory(addr + i, ch) {
                break;
            }

            total_read += 1;
        }

        process::wakeup(core::ptr::addr_of!(self.write).addr());
        Ok(total_read)
    }
}
