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

use crate::{buffer, config::LOGSIZE};

#[repr(C)]
#[derive(Clone)]
pub struct LogHeader {
    n: u32,
    block: [u32; LOGSIZE],
}

fn read_header(device: usize, block: usize) -> Option<LogHeader> {
    let buffer = buffer::get(device, block)?;
    let uninit = buffer.as_uninit::<LogHeader>()?;
    let header = unsafe { uninit.assume_init_ref() };
    Some(header.clone())
}

fn write_header(device: usize, block: usize, header: &LogHeader) -> Result<(), ()> {
    let mut buffer = buffer::get(device, block).ok_or(())?;
    let uninit = buffer.as_uninit_mut::<LogHeader>().ok_or(())?;
    uninit.write(header.clone());
    Ok(())
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
