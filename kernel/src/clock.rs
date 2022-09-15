use crate::{process, spinlock::SpinLock};

static TICKS: SpinLock<u64> = SpinLock::new(0);

pub fn get() -> u64 {
    *TICKS.lock()
}

pub fn tick() {
    let mut ticks = TICKS.lock();
    *ticks = (*ticks).wrapping_add(1);
    process::wakeup(core::ptr::addr_of!(TICKS).addr());
}

#[must_use]
pub fn sleep(time: u64) -> bool {
    let mut current = TICKS.lock();

    let start = *current;
    while (*current - start) < time {
        if process::is_killed() == Some(true) {
            return false;
        }
        process::sleep(core::ptr::addr_of!(TICKS).addr(), &mut current);
    }

    true
}
