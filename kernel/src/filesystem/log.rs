// Simple logging that allows concurrent FS system calls.
//
// A log transaction contains the updates of multiple FS system
// calls. The logging system only commits when there are
// no FS system calls active. Thus there is never
// any reasoning required about whether a commit might
// write an uncommitted system call's updates to disk.
//
// A system call should call begin_op()/end_op() to mark
// its start and end. Usually begin_op() just increments
// the count of in-progress FS system calls and returns.
// But if it thinks the log is close to running out, it
// sleeps until the last outstanding end_op() commits.
//
// The log is a physical re-do log containing disk blocks.
// The on-disk log format:
//   header block, containing block #s for block A, B, C, ...
//   block A
//   block B
//   block C
//   ...
// Log appends are synchronous.

use crate::filesystem::buffer::{self, Buffer, BSIZE};
use crate::filesystem::superblock::SuperBlock;
use crate::{
    config::{LOGSIZE, MAXOPBLOCKS, NBUF},
    process,
    spinlock::{SpinLock, SpinLockGuard},
};

const _: () = {
    assert!(core::mem::size_of::<LogHeader>() <= BSIZE);
};

#[repr(C)]
#[derive(Debug, Clone)]
struct LogHeader {
    n: u32,
    block: [u32; LOGSIZE],
}

impl LogHeader {
    const fn empty() -> Self {
        Self {
            n: 0,
            block: [0; _],
        }
    }
}

#[derive(Debug)]
pub struct Log {
    start: usize,
    size: usize,
    outstanding: usize,
    committing: bool,
    device: usize,
    header: LogHeader,
}

impl Log {
    const fn new() -> Self {
        Self {
            start: 0,
            size: 0,
            outstanding: 0,
            committing: false,
            device: 0,
            header: LogHeader::empty(),
        }
    }

    fn start(mut self: SpinLockGuard<'static, Self>) -> Logger {
        loop {
            let is_full = self.header.n as usize + (self.outstanding + 1) * MAXOPBLOCKS > LOGSIZE;
            if self.committing || is_full {
                process::sleep(core::ptr::addr_of!(*self).addr(), &mut self);
            } else {
                self.outstanding += 1;
                return Logger::new(SpinLock::unlock(self));
            }
        }
    }

    fn commit(self: &mut SpinLockGuard<Self>) {
        self.committing = true;
        if self.header.n > 0 {
            write_log(self);
            write_header(self);
            install_blocks(self, false);
            write_header(self);
        }
        self.committing = false;
    }

    fn end(self: &mut SpinLockGuard<Self>) {
        assert!(!self.committing);

        self.outstanding -= 1;

        if self.outstanding == 0 {
            self.commit();
        }

        process::wakeup(core::ptr::addr_of!(**self).addr());
    }

    fn write<T>(&mut self, buf: &Buffer<T, BSIZE, NBUF>) {
        assert!((self.header.n as usize) < LOGSIZE);
        assert!((self.header.n as usize) < self.size - 1);
        assert!(self.outstanding > 0);

        let len = self.header.n as usize;
        let block = Buffer::block_number(buf) as u32;
        if !self.header.block[0..len].contains(&block) {
            buffer::pin(buf);
            self.header.block[len] = block;
            self.header.n += 1;
        }
    }
}

#[derive(Debug)]
pub struct Logger {
    log: &'static SpinLock<Log>,
}

impl Logger {
    fn new(log: &'static SpinLock<Log>) -> Self {
        Self { log }
    }

    pub fn write<T>(&self, buf: &Buffer<T, BSIZE, NBUF>) {
        self.log.lock().write(buf);
    }
}

impl Drop for Logger {
    fn drop(&mut self) {
        self.log.lock().end();
    }
}

static LOG: SpinLock<Log> = SpinLock::new(Log::new());

pub fn start() -> Logger {
    LOG.lock().start()
}

fn read_header(log: &mut SpinLockGuard<Log>) -> Option<()> {
    let device = log.device;
    let inode = log.start;

    log.header = SpinLock::unlock_temporarily(log, move || unsafe {
        let header = buffer::with_read::<LogHeader>(device, inode).unwrap();
        (*header).clone()
    });

    Some(())
}

fn write_header(log: &mut SpinLockGuard<Log>) -> Option<()> {
    let header = log.header.clone();
    let device = log.device;
    let inode = log.start;

    SpinLock::unlock_temporarily(log, move || unsafe {
        let buf = buffer::with_write(device, inode, &header).unwrap();
        buffer::flush(buf);
    });

    Some(())
}

fn install_blocks(log: &mut SpinLockGuard<Log>, recovering: bool) {
    for tail in 0..(log.header.n as usize) {
        let device = log.device;
        let inode_in = log.start + tail + 1;
        let inode_out = log.header.block[tail] as usize;

        SpinLock::unlock_temporarily(log, move || unsafe {
            let from = buffer::with_read::<[u8; BSIZE]>(device, inode_in).unwrap();
            let to = buffer::with_write(device, inode_out, &*from).unwrap();

            if !recovering {
                buffer::unpin(&to);
            }

            buffer::flush(to);
        });
    }

    log.header.n = 0;
}

fn write_log(log: &mut SpinLockGuard<Log>) {
    for tail in 0..(log.header.n as usize) {
        let device = log.device;
        let inode_in = log.header.block[tail] as usize;
        let inode_out = log.start + tail + 1;

        SpinLock::unlock_temporarily(log, move || unsafe {
            let from = buffer::with_read::<[u8; BSIZE]>(device, inode_in).unwrap();
            let to = buffer::with_write(device, inode_out, &*from).unwrap();
            buffer::flush(to);
        });
    }
}

pub fn initialize(device: usize, sb: &SuperBlock) {
    let mut log = LOG.lock();
    log.start = sb.logstart as usize;
    log.size = sb.nlog as usize;
    log.device = device;

    read_header(&mut log).unwrap();
    install_blocks(&mut log, true);
    write_header(&mut log).unwrap();
}
