use core::ops::{Deref, DerefMut};

#[repr(transparent)]
pub struct Undroppable<T>(T);

impl<T> Undroppable<T> {
    /// コンパイル時にパニックを発生させるための定数
    const PANIC: () = panic!("Undroppable!");

    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub const fn forget(self) {
        core::mem::forget(self);
    }
}

impl<T> const Deref for Undroppable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> const DerefMut for Undroppable<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> const Drop for Undroppable<T> {
    ///
    fn drop(&mut self) {
        // SAFETY: constコンテキストでは巻き戻しが行われないので、二重パニックの心配はありません。
        Self::PANIC
    }
}
