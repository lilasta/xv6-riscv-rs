use core::ops::{Deref, DerefMut};

use crate::{config::NCPU, riscv};

static mut STATES: [InterruptState; NCPU] = [const { InterruptState::new() }; NCPU];

pub struct InterruptState {
    // Depth of push_off() nesting.
    depth_of_disable: usize,

    // Were interrupts enabled before push_off()?
    is_enabled_before: bool,
}

impl InterruptState {
    pub const fn new() -> Self {
        Self {
            depth_of_disable: 0,
            is_enabled_before: false,
        }
    }
}

pub struct InterruptGuard<T> {
    value: T,
}

impl<T> InterruptGuard<T> {
    pub fn with(init: impl FnOnce() -> T) -> Self {
        push_off();
        Self { value: init() }
    }
}

impl<T> Drop for InterruptGuard<T> {
    fn drop(&mut self) {
        pop_off();
    }
}

impl<T> Deref for InterruptGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for InterruptGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T> !Sync for InterruptGuard<T> {}
impl<T> !Send for InterruptGuard<T> {}

fn hartid() -> usize {
    assert!(!is_enabled());
    // Machineでないと取れない
    //unsafe { riscv::read_csr!(mhartid) as usize }
    unsafe { riscv::read_reg!(tp) as usize }
}

fn get_state() -> &'static mut InterruptState {
    assert!(!is_enabled());
    unsafe { &mut STATES[hartid()] }
}

pub fn is_enabled() -> bool {
    unsafe { riscv::read_csr!(sstatus) & riscv::sstatus::SIE != 0 }
}

pub fn get_depth() -> usize {
    get_state().depth_of_disable
}

pub fn is_enabled_before() -> bool {
    get_state().is_enabled_before
}

pub fn set_enabled_before(value: bool) {
    get_state().is_enabled_before = value;
}

pub fn push_off() {
    unsafe {
        // TODO: おそらく順序が大事?
        let is_enabled = is_enabled();

        riscv::disable_interrupt();

        let mut state = get_state();

        if state.depth_of_disable == 0 {
            state.is_enabled_before = is_enabled;
        }

        state.depth_of_disable += 1;
    }
}

pub fn pop_off() {
    unsafe {
        let mut state = get_state();
        assert!(state.depth_of_disable > 0);

        state.depth_of_disable -= 1;

        if state.depth_of_disable == 0 && state.is_enabled_before {
            riscv::enable_interrupt();
        }
    }
}

pub fn off<R>(f: impl FnOnce() -> R) -> R {
    push_off();
    let ret = f();
    pop_off();
    ret
}

mod binding {
    #[no_mangle]
    extern "C" fn push_off() {
        super::push_off();
    }

    #[no_mangle]
    extern "C" fn pop_off() {
        super::pop_off();
    }
}
