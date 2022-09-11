use core::sync::atomic::{AtomicUsize, Ordering::*};

use crate::{
    buffer::BSIZE,
    config::{MAXOPBLOCKS, NDEV},
    fs::{self, InodeC, InodeLockGuard, Stat},
    log,
    pipe::Pipe,
};

pub const PIPESIZE: usize = 512;

#[derive(Debug)]
pub enum File {
    Pipe {
        pipe: Pipe<PIPESIZE>,
    },
    Inode {
        inode: *mut InodeC,
        offset: AtomicUsize,
        readable: bool,
        writable: bool,
    },
    Device {
        inode: *mut InodeC,
        major: usize,
        readable: bool,
        writable: bool,
    },
}

impl File {
    pub const fn new_pipe(pipe: Pipe<PIPESIZE>) -> Self {
        Self::Pipe { pipe }
    }

    pub const fn new_inode(inode: *mut InodeC, readable: bool, writable: bool) -> Self {
        Self::Inode {
            inode,
            offset: AtomicUsize::new(0),
            readable,
            writable,
        }
    }

    pub const fn new_device(
        inode: *mut InodeC,
        major: usize,
        readable: bool,
        writable: bool,
    ) -> Self {
        Self::Device {
            inode,
            major,
            readable,
            writable,
        }
    }

    pub fn stat(&self) -> Result<Stat, ()> {
        match self {
            Self::Pipe { .. } => Err(()),
            Self::Inode { inode, .. } | Self::Device { inode, .. } => {
                Ok(InodeLockGuard::new(*inode).stat())
            }
        }
    }

    pub fn read(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self {
            Self::Pipe { pipe } => pipe.read(addr, n),
            Self::Inode {
                inode,
                offset,
                readable,
                ..
            } => {
                if !*readable {
                    return Err(());
                }

                let inode = InodeLockGuard::new(*inode);
                let result =
                    inode.copy_to(true, <*mut u8>::from_bits(addr), offset.load(Acquire), n);
                inode.unlock_without_put();

                let read = match result {
                    Ok(read) => read,
                    Err(read) => read,
                };

                offset.fetch_add(read, Release);
                Ok(read)
            }
            Self::Device {
                major, readable, ..
            } => {
                if !*readable {
                    return Err(());
                }

                let device = unsafe { devsw.get(*major).ok_or(())? };
                let result = (device.as_ref().unwrap().read)(1, addr, n);
                if result < 0 {
                    Err(())
                } else {
                    Ok(result as usize)
                }
            }
        }
    }

    pub fn write(&self, addr: usize, n: usize) -> Result<usize, ()> {
        match self {
            Self::Pipe { pipe } => pipe.write(addr, n),
            Self::Inode {
                inode,
                offset,
                writable,
                ..
            } => {
                if !*writable {
                    return Err(());
                }

                // write a few blocks at a time to avoid exceeding
                // the maximum log transaction size, including
                // i-node, indirect block, allocation blocks,
                // and 2 blocks of slop for non-aligned writes.
                // this really belongs lower down, since writei()
                // might be writing a device like the console.
                let max = ((MAXOPBLOCKS - 1 - 1 - 2) / 2) * BSIZE;
                let mut i = 0;
                while i < n {
                    let n = (n - i).min(max);

                    let log = log::start();
                    let inode = InodeLockGuard::new(*inode);
                    let result = inode.copy_from(
                        true,
                        <*const u8>::from_bits(addr + i),
                        offset.load(Acquire),
                        n,
                    );
                    let wrote = match result {
                        Ok(wrote) => wrote,
                        Err(wrote) => wrote,
                    };
                    offset.fetch_add(wrote, Release);
                    inode.unlock_without_put();
                    drop(log);

                    if result.is_err() {
                        break;
                    }
                    i += n;
                }

                if i == n {
                    Ok(n)
                } else {
                    Err(())
                }
            }
            Self::Device {
                major, writable, ..
            } => {
                if !*writable {
                    return Err(());
                }

                let device = unsafe { devsw.get(*major).ok_or(())? };
                let result = (device.as_ref().unwrap().write)(1, addr, n);
                if result < 0 {
                    Err(())
                } else {
                    Ok(result as usize)
                }
            }
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        match self {
            Self::Pipe { .. } => {}
            Self::Inode { inode, .. } | Self::Device { inode, .. } => {
                let log = log::start();
                fs::put(&log, *inode);
            }
        }
    }
}

#[repr(C)]
pub struct DeviceFile {
    pub read: extern "C" fn(i32, usize, usize) -> i32,
    pub write: extern "C" fn(i32, usize, usize) -> i32,
}

pub static mut devsw: [Option<DeviceFile>; NDEV] = [const { None }; _];
