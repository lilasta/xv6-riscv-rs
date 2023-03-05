use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicUsize, Ordering::*};

use crate::fs::InodeReference;
use crate::{
    config::{MAXOPBLOCKS, NDEV},
    filesystem::buffer::BSIZE,
    filesystem::log,
    fs::Stat,
    pipe::Pipe,
};

pub const PIPESIZE: usize = 512;

#[derive(Debug)]
pub enum File {
    Pipe {
        pipe: Pipe<PIPESIZE>,
    },
    Inode {
        inode: ManuallyDrop<InodeReference>,
        offset: AtomicUsize,
        readable: bool,
        writable: bool,
    },
    Device {
        inode: ManuallyDrop<InodeReference>,
        major: usize,
        readable: bool,
        writable: bool,
    },
}

impl File {
    pub const fn new_pipe(pipe: Pipe<PIPESIZE>) -> Self {
        Self::Pipe { pipe }
    }

    pub const fn new_inode(inode: InodeReference, readable: bool, writable: bool) -> Self {
        Self::Inode {
            inode: ManuallyDrop::new(inode),
            offset: AtomicUsize::new(0),
            readable,
            writable,
        }
    }

    pub const fn new_device(
        inode: InodeReference,
        major: usize,
        readable: bool,
        writable: bool,
    ) -> Self {
        Self::Device {
            inode: ManuallyDrop::new(inode),
            major,
            readable,
            writable,
        }
    }

    pub fn stat(&self) -> Result<Stat, ()> {
        match self {
            Self::Pipe { .. } => Err(()),
            Self::Inode { inode, .. } | Self::Device { inode, .. } => {
                log::with(|| Ok(inode.lock().stat()))
            }
        }
    }

    pub fn read(&'static self, addr: usize, n: usize) -> Result<usize, ()> {
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

                log::with(|| {
                    let read = inode
                        .lock()
                        .copy_to::<u8>(true, addr, offset.load(Acquire), n)?;
                    offset.fetch_add(read, Release);
                    Ok(read)
                })
            }
            Self::Device {
                major, readable, ..
            } => {
                if !*readable {
                    return Err(());
                }

                let device = unsafe { DEVICEFILES.get(*major).ok_or(())? };
                let result = (device.as_ref().unwrap().read)(addr, n);
                if result < 0 {
                    Err(())
                } else {
                    Ok(result as usize)
                }
            }
        }
    }

    pub fn write(&'static self, addr: usize, n: usize) -> Result<usize, ()> {
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

                    let wrote = log::with(|| {
                        let wrote = inode
                            .lock()
                            .copy_from::<u8>(true, addr + i, offset.load(Acquire), n)
                            .unwrap_or(0);
                        offset.fetch_add(wrote, Release);
                        wrote
                    });

                    if wrote != n {
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
                let device = unsafe { DEVICEFILES.get(*major).ok_or(())? };
                let result = (device.as_ref().unwrap().write)(addr, n);
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
            Self::Inode { inode, .. } | Self::Device { inode, .. } => {
                log::with(|| unsafe { ManuallyDrop::drop(inode) })
            }
            _ => {}
        }
    }
}

#[repr(C)]
pub struct DeviceFile {
    pub read: fn(usize, usize) -> i32,
    pub write: fn(usize, usize) -> i32,
}

pub static mut DEVICEFILES: [Option<DeviceFile>; NDEV] = [const { None }; _];
