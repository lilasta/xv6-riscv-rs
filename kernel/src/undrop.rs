use core::ops::{Deref, DerefMut};

#[repr(transparent)]
pub struct Undroppable<T>(T);

impl<T> Undroppable<T> {
    /// コンパイル時にパニックを発生させるための定数
    const PANIC: () = panic!("Undroppable!");

    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub const fn forget(this: Self) {
        core::mem::forget(this);
    }

    pub const fn into_inner(mut this: Self) -> T {
        let inner = unsafe { Self::take(&mut this) };
        Self::forget(this);
        inner
    }

    pub const unsafe fn take(this: &mut Self) -> T {
        unsafe { core::ptr::read(&this.0) }
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
