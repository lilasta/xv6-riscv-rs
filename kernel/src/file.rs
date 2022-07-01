use crate::config::NDEV;

#[repr(C)]
pub struct DeviceFile {
    pub read: extern "C" fn(i32, usize, i32) -> i32,
    pub write: extern "C" fn(i32, usize, i32) -> i32,
}

extern "C" {
    pub static mut devsw: [DeviceFile; NDEV];
}
