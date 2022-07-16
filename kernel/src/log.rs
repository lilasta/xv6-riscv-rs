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
