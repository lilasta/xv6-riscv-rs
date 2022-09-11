use core::ffi::c_void;

use crate::{
    config::NDEV,
    fs::{self, InodeC, InodeLockGuard, Stat},
    pipe::Pipe,
};

pub const PIPESIZE: usize = 512;

pub enum File {
    Pipe {
        pipe: Pipe<PIPESIZE>,
    },
    Inode {
        inode: *mut InodeC,
        offset: usize,
        readable: bool,
        writable: bool,
    },
    Device {
        inode: *mut InodeC,
        readable: bool,
        writable: bool,
    },
}

impl File {
    pub const fn new_pipe(pipe: Pipe<PIPESIZE>) -> Self {
        Self::Pipe { pipe }
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
        todo!()
    }

    pub fn write(&mut self, addr: usize, n: usize) -> Result<usize, ()> {
        todo!()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        match self {
            Self::Pipe { .. } => {}
            Self::Inode { inode, .. } | Self::Device { inode, .. } => {
                fs::put(*inode);
            }
        }
    }
}

#[repr(C)]
pub struct DeviceFile {
    pub read: extern "C" fn(i32, usize, i32) -> i32,
    pub write: extern "C" fn(i32, usize, i32) -> i32,
}

#[repr(C)]
pub struct FileC {
    pub kind: u32,
    pub refcnt: u32,
    pub readable: bool,
    pub writable: bool,
    pub ip: *mut c_void,
    pub off: u32,
    pub major: u32,
    pub pipe: Pipe<PIPESIZE>,
}

extern "C" {
    pub fn filedup(f: *mut FileC) -> *mut FileC;
    pub fn fileread(f: *mut FileC, addr: usize, size: i32) -> i32;
    pub fn filewrite(f: *mut FileC, addr: usize, size: i32) -> i32;
    pub fn fileclose(f: *mut FileC);
    pub fn filestat(f: *mut FileC, addr: usize) -> i32;
    pub fn filealloc() -> *mut FileC;
}

pub const FD_NONE: u32 = 0;
pub const FD_PIPE: u32 = 1;
pub const FD_INODE: u32 = 2;
pub const FD_DEVICE: u32 = 3;

extern "C" {
    pub static mut devsw: [DeviceFile; NDEV];
}
