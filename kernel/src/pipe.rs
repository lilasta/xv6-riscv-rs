use crate::{
    lock::{spin::SpinLock, LockGuard},
    process,
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
                todo!();
                i += 1;
            }
        }
        process::wakeup(core::ptr::addr_of!(self.read).addr());

        Ok(i)
    }

    pub fn read(&mut self, addr: usize, n: usize) -> usize {
        todo!()
    }
}
