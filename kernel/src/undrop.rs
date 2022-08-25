#[repr(transparent)]
pub struct Undroppable<T>(T);

impl<T> Undroppable<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub const fn forget(self) {
        core::mem::forget(self);
    }
}

impl<T> core::ops::Deref for Undroppable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> core::ops::DerefMut for Undroppable<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> Drop for Undroppable<T> {
    fn drop(&mut self) {
        const { panic!() };
    }
}
