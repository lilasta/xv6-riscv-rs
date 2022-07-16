use core::{
    ffi::c_void,
    ops::{Deref, DerefMut},
};

extern "C" {
    fn ilock(ip: *mut c_void);
    fn iunlockput(ip: *mut c_void);
}

pub struct InodeLockGuard {
    inode: *mut c_void,
}

impl InodeLockGuard {
    pub fn new(inode: *mut c_void) -> Self {
        unsafe { ilock(inode) };
        Self { inode }
    }
}

impl Deref for InodeLockGuard {
    type Target = *mut c_void;

    fn deref(&self) -> &Self::Target {
        &self.inode
    }
}

impl DerefMut for InodeLockGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inode
    }
}

impl Drop for InodeLockGuard {
    fn drop(&mut self) {
        unsafe { iunlockput(self.inode) };
    }
}
