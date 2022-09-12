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
    config::{LOGSIZE, MAXOPBLOCKS},
    fs::SuperBlock,
    lock::{spin::SpinLock, Lock, LockGuard},
    process,
};

const _: () = {
    assert!(core::mem::size_of::<LogHeader>() <= BSIZE);
};

#[repr(C)]
#[derive(Debug)]
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

    fn write(&mut self, buf: &BufferGuard) {
        assert!((self.header.n as usize) < LOGSIZE);
        assert!((self.header.n as usize) < self.size - 1);
        assert!(self.outstanding > 0);

        let len = self.header.n as usize;
        let block = buf.block_number() as u32;
        if !self.header.block[0..len].contains(&block) {
            buffer::pin(buf);

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

    pub fn write(&self, buf: &BufferGuard) {
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

#[deprecated]
pub unsafe fn get_guard_without_start() -> LogGuard<'static> {
    LogGuard::new(&LOG)
}

fn read_header(log: &mut LockGuard<SpinLock<Log>>) -> Option<()> {
    let buf = buffer::get_with_unlock(log.device, log.start, log)?;
    unsafe { core::ptr::copy(buf.as_ptr(), &mut log.header, 1) };
    buffer::release_with_unlock(buf, log);
    Some(())
}

fn write_header(log: &mut LockGuard<SpinLock<Log>>) -> Option<()> {
    let mut buf = buffer::get_with_unlock(log.device, log.start, log)?;
    unsafe { core::ptr::copy(&log.header, buf.as_mut_ptr(), 1) };
    buffer::write_with_unlock(&mut buf, log);
    buffer::release_with_unlock(buf, log);
    Some(())
}

fn install_blocks(log: &mut LockGuard<SpinLock<Log>>, recovering: bool) {
    for tail in 0..(log.header.n as usize) {
        let from = buffer::get_with_unlock(log.device, log.start + tail + 1, log).unwrap();
        let mut to =
            buffer::get_with_unlock(log.device, log.header.block[tail] as usize, log).unwrap();

        unsafe {
            let src = from.as_ptr::<u8>();
            let dst = to.as_mut_ptr::<u8>();
            core::ptr::copy(src, dst, BSIZE);
        }
        buffer::write_with_unlock(&mut to, log);

        if !recovering {
            buffer::unpin(&to);
        }

        buffer::release_with_unlock(from, log);
        buffer::release_with_unlock(to, log);
    }

    log.header.n = 0;
}

fn write_log(log: &mut LockGuard<SpinLock<Log>>) {
    for tail in 0..(log.header.n as usize) {
        let mut to = buffer::get_with_unlock(log.device, log.start + tail + 1, log).unwrap();
        let from =
            buffer::get_with_unlock(log.device, log.header.block[tail] as usize, log).unwrap();

        unsafe {
            let src = from.as_ptr::<u8>();
            let dst = to.as_mut_ptr::<u8>();
            core::ptr::copy(src, dst, BSIZE);
        }
        buffer::write_with_unlock(&mut to, log);
        buffer::release_with_unlock(from, log);
        buffer::release_with_unlock(to, log);
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

#[no_mangle]
extern "C" fn begin_op() {
    let guard = LOG.lock().start();
    core::mem::forget(guard);
}

#[no_mangle]
extern "C" fn end_op() {
    LOG.lock().end();
}
