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

use crate::{
    buffer::{self, BufferGuard, BSIZE},
    config::{LOGSIZE, MAXOPBLOCKS, NBUF},
    fs::SuperBlock,
    lock::{spin::SpinLock, Lock, LockGuard},
    process,
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
    pub const fn empty() -> Self {
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
    pub const fn uninit() -> Self {
        Self {
            start: 0,
            size: 0,
            outstanding: 0,
            committing: false,
            device: 0,
            header: LogHeader::empty(),
        }
    }

    fn start<'a>(mut self: LockGuard<'a, SpinLock<Self>>) -> LogGuard<'a> {
        loop {
            if self.committing {
                process::sleep(core::ptr::addr_of!(*self).addr(), &mut self);
            } else if self.header.n as usize + (self.outstanding + 1) * MAXOPBLOCKS > LOGSIZE {
                process::sleep(core::ptr::addr_of!(*self).addr(), &mut self);
            } else {
                self.outstanding += 1;
                return LogGuard::new(Lock::unlock(self));
            }
        }
    }

    fn commit(self: &mut LockGuard<SpinLock<Log>>) {
        self.committing = true;
        if self.header.n > 0 {
            write_log(self);
            write_header(self);
            install_blocks(self, false);
            write_header(self);
        }
        self.committing = false;
    }

    fn end(self: &mut LockGuard<SpinLock<Self>>) {
        assert!(!self.committing);

        self.outstanding -= 1;

        if self.outstanding == 0 {
            self.commit();
        }

        process::wakeup(core::ptr::addr_of!(**self).addr());
    }

    fn write(&mut self, buf: &BufferGuard<BSIZE, NBUF>) {
        assert!((self.header.n as usize) < LOGSIZE);
        assert!((self.header.n as usize) < self.size - 1);
        assert!(self.outstanding > 0);

        let len = self.header.n as usize;
        let block = buf.block_number() as u32;
        if !self.header.block[0..len].contains(&block) {
            buf.pin();
            self.header.block[len] = block;
            self.header.n += 1;
        }
    }
}

#[derive(Debug)]
pub struct LogGuard<'a> {
    log: &'a SpinLock<Log>,
}

impl<'a> LogGuard<'a> {
    fn new(log: &'a SpinLock<Log>) -> Self {
        Self { log }
    }

    pub fn write(&self, buf: &BufferGuard<BSIZE, NBUF>) {
        self.log.lock().write(buf);
    }
}

impl<'a> Drop for LogGuard<'a> {
    fn drop(&mut self) {
        self.log.lock().end();
    }
}

static LOG: SpinLock<Log> = SpinLock::new(Log::uninit());

pub fn start() -> LogGuard<'static> {
    LOG.lock().start()
}

fn read_header(log: &mut LockGuard<SpinLock<Log>>) -> Option<()> {
    let mut buf = buffer::get(log.device, log.start)?;
    unsafe {
        log.header = buf.read_with_unlock::<LogHeader, _>(log).clone();
    }
    Some(())
}

fn write_header(log: &mut LockGuard<SpinLock<Log>>) -> Option<()> {
    let mut buf = buffer::get(log.device, log.start)?;
    unsafe {
        buf.write_with_unlock(log.header.clone(), log);
    }
    Some(())
}

fn install_blocks(log: &mut LockGuard<SpinLock<Log>>, recovering: bool) {
    for tail in 0..(log.header.n as usize) {
        let mut from = buffer::get(log.device, log.start + tail + 1).unwrap();
        let mut to = buffer::get(log.device, log.header.block[tail] as usize).unwrap();

        unsafe {
            let from = from.read_with_unlock::<[u8; BSIZE], _>(log);
            to.write_with_unlock(from, log);
        }

        if !recovering {
            to.unpin();
        }
    }

    log.header.n = 0;
}

fn write_log(log: &mut LockGuard<SpinLock<Log>>) {
    for tail in 0..(log.header.n as usize) {
        let mut to = buffer::get(log.device, log.start + tail + 1).unwrap();
        let mut from = buffer::get(log.device, log.header.block[tail] as usize).unwrap();

        unsafe {
            let from = from.read_with_unlock::<[u8; BSIZE], _>(log);
            to.write_with_unlock(from, log);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn initlog(dev: u32, sb: *const SuperBlock) {
    let mut log = LOG.lock();
    log.start = (*sb).logstart as usize;
    log.size = (*sb).nlog as usize;
    log.device = dev as usize;

    read_header(&mut log).unwrap();
    install_blocks(&mut log, true);
    write_header(&mut log).unwrap();
}
