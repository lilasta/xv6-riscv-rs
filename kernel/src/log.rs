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
    buffer::{self, BSIZE},
    config::LOGSIZE,
};

const _: () = {
    assert!(core::mem::size_of::<LogHeader>() <= BSIZE);
};

#[repr(C)]
pub struct LogHeader {
    n: u32,
    block: [u32; LOGSIZE],
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
    committing: usize,
    device: usize,
    header: LogHeader,
}

pub struct LogGuard;

impl LogGuard {
    pub fn new() -> Self {
        extern "C" {
            fn begin_op();
        }
        unsafe {
            begin_op();
        }
        Self
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        extern "C" {
            fn end_op();
        }
        unsafe {
            end_op();
        }
    }
}
