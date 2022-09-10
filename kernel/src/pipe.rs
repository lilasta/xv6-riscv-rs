use crate::{
    lock::{spin::SpinLock, LockGuard},
    process,
    vm::PageTableExtension,
};

pub struct Pipe<const SIZE: usize> {
    data: [u8; SIZE],
    read: usize,
    write: usize,
    is_reading: bool,
    is_writing: bool,
}

impl<const SIZE: usize> Pipe<SIZE> {
    pub const fn new() -> Self {
        Self {
            data: [0; _],
            read: 0,
            write: 0,
            is_reading: false,
            is_writing: false,
        }
    }

    pub fn write(self: &mut LockGuard<SpinLock<Self>>, addr: usize, n: usize) -> Result<usize, ()> {
        let mut i = 0;
        while i < n {
            if self.is_reading || process::is_killed() == Some(true) {
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
        while self.read == self.write && !self.is_writing {
            if process::is_killed() == Some(true) {
                return Err(());
            }
            process::sleep(core::ptr::addr_of!(self.read).addr(), self);
        }

        let read_previous = self.read;
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
        }

        process::wakeup(core::ptr::addr_of!(self.write).addr());
        Ok(self.read - read_previous)
    }
}
