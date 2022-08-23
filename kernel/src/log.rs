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
    lock::{spin::SpinLock, Lock},
    process,
    virtio::disk::Buffer,
};

const _: () = {
    assert!(core::mem::size_of::<LogHeader>() <= BSIZE);
};

#[repr(C)]
pub struct SuperBlock {
    magic: u32,      // Must be FSMAGIC
    size: u32,       // Size of file system image (blocks)
    nblocks: u32,    // Number of data blocks
    ninodes: u32,    // Number of inodes.
    nlog: u32,       // Number of log blocks
    logstart: u32,   // Block number of first log block
    inodestart: u32, // Block number of first inode block
    bmapstart: u32,  // Block number of first free map block
}

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

fn read_header(device: usize, block: usize, header: &mut LogHeader) -> Option<()> {
    let buffer = buffer::get(device, block)?;
    let uninit = buffer.as_uninit::<LogHeader>()?;
    unsafe { core::ptr::copy_nonoverlapping(uninit.as_ptr(), header, 1) };
    Some(())
}

fn write_header(device: usize, block: usize, header: &LogHeader) -> Option<()> {
    let mut buffer = buffer::get(device, block)?;
    let uninit = buffer.as_uninit_mut::<LogHeader>()?;
    unsafe { core::ptr::copy_nonoverlapping(header, uninit.as_mut_ptr(), 1) };
    Some(())
}

fn install_blocks(log: &mut Log, recovering: bool) {
    for tail in 0..(log.header.n as usize) {
        let logged = buffer::get(log.device, log.start + tail + 1).unwrap();
        let mut dst = buffer::get(log.device, log.header.block[tail] as usize).unwrap();

        unsafe {
            let src = logged.as_ptr::<u8>().unwrap();
            let dst = dst.as_mut_ptr::<u8>().unwrap();
            core::ptr::copy(src, dst, BSIZE);
        }

        if !recovering {
            buffer::unpin(&dst);
        }
    }

    log.header.n = 0;
}

fn recover(log: &mut Log) {
    read_header(log.device, log.start, &mut log.header).unwrap();
    install_blocks(log, true);
    write_header(log.device, log.start, &log.header).unwrap();
}

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

fn commit() {
    todo!()
}

static LOG: SpinLock<Log> = SpinLock::new(Log::uninit());

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
        assert!(log.outstanding == 0);

        let is_new = log
            .header
            .block
            .iter()
            .take(log.header.n as usize)
            .find(|b| **b as usize == buf.block_number())
            .is_none();

        if is_new {
            buffer::pin(buf);

            let i = log.header.n as usize;
            log.header.block[i] = buf.block_number() as u32;
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

        let mut do_commit = false;
        log.outstanding -= 1;
        if log.outstanding == 0 {
            do_commit = true;
            log.committing = true;
        } else {
            process::wakeup(core::ptr::addr_of!(LOG).addr());
        }

        if do_commit {
            // call commit w/o holding locks, since not allowed
            // to sleep with locks.
            Lock::unlock_temporarily(&mut log, commit);
            log.committing = false;
            process::wakeup(core::ptr::addr_of!(LOG).addr());
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        Self::end();
    }
}
