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
    virtio::disk::Buffer,
};

const _: () = {
    assert!(core::mem::size_of::<LogHeader>() <= BSIZE);
};

#[repr(C)]
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

static LOG: SpinLock<Log> = SpinLock::new(Log::uninit());

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
}

fn commit(log: &mut LockGuard<SpinLock<Log>>) {
    log.committing = true;
    if log.header.n > 0 {
        write_log(log);
        write_header(log);
        install_blocks(log, false);
        write_header(log);
    }
    log.committing = false;
}

pub struct LogGuard;

impl LogGuard {
    pub fn new() -> Self {
        Self::begin();
        Self
    }

    pub fn write(&self, buf: &BufferGuard) {
        let mut log = LOG.lock();

        assert!((log.header.n as usize) < LOGSIZE);
        assert!((log.header.n as usize) < log.size - 1);
        assert!(log.outstanding > 0);

        let len = log.header.n as usize;
        let block = buf.block_number() as u32;
        if !log.header.block[0..len].contains(&block) {
            buffer::pin(buf);

            log.header.block[len] = block;
            log.header.n += 1;
        }
    }

    fn begin() {
        let mut log = LOG.lock();
        loop {
            if log.committing {
                process::sleep(core::ptr::addr_of!(LOG).addr(), &mut log);
            } else if log.header.n as usize + (log.outstanding + 1) * MAXOPBLOCKS > LOGSIZE {
                process::sleep(core::ptr::addr_of!(LOG).addr(), &mut log);
            } else {
                log.outstanding += 1;
                return;
            }
        }
    }

    fn end() {
        let mut log = LOG.lock();
        assert!(!log.committing);

        log.outstanding -= 1;

        if log.outstanding == 0 {
            commit(&mut log);
        }

        process::wakeup(core::ptr::addr_of!(LOG).addr());
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        Self::end();
    }
}

pub fn get() -> LogGuard {
    LogGuard::new()
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
    LogGuard::begin();
}

#[no_mangle]
extern "C" fn end_op() {
    LogGuard::end();
}
