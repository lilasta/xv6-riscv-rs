use core::ffi::c_void;

use crate::config::NDEV;

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
    pub pipe: *mut c_void,
    pub ip: *mut c_void,
    pub off: u32,
    pub major: u32,
}

extern "C" {
    pub fn filedup(f: *mut FileC) -> *mut FileC;
    pub fn fileread(f: *mut FileC, addr: usize, size: i32) -> i32;
    pub fn filewrite(f: *mut FileC, addr: usize, size: i32) -> i32;
    pub fn fileclose(f: *mut FileC);
    pub fn filestat(f: *mut FileC, addr: usize) -> i32;
    pub fn filealloc() -> *mut FileC;
    pub fn pipealloc(f1: *mut *mut FileC, f2: *mut *mut FileC) -> i32;
}

pub const FD_NONE: u32 = 0;
pub const FD_PIPE: u32 = 1;
pub const FD_INODE: u32 = 2;
pub const FD_DEVICE: u32 = 3;

extern "C" {
    pub static mut devsw: [DeviceFile; NDEV];
}
