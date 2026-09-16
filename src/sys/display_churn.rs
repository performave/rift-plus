use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::sys::skylight::DisplayReconfigFlags;

static DISPLAY_CHURN_ACTIVE: AtomicBool = AtomicBool::new(false);
static DISPLAY_CHURN_EPOCH: AtomicU64 = AtomicU64::new(0);
static DISPLAY_CHURN_FLAGS: AtomicU64 = AtomicU64::new(0);

pub fn begin(flags: DisplayReconfigFlags) -> u64 {
    let was_active = DISPLAY_CHURN_ACTIVE.swap(true, Ordering::SeqCst);
    if !was_active {
        let epoch = DISPLAY_CHURN_EPOCH.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        DISPLAY_CHURN_FLAGS.store(flags.bits() as u64, Ordering::SeqCst);
        epoch
    } else {
        DISPLAY_CHURN_FLAGS.fetch_or(flags.bits() as u64, Ordering::SeqCst);
        DISPLAY_CHURN_EPOCH.load(Ordering::SeqCst)
    }
}

pub fn end() -> u64 {
    DISPLAY_CHURN_ACTIVE.store(false, Ordering::SeqCst);
    DISPLAY_CHURN_FLAGS.store(0, Ordering::SeqCst);
    DISPLAY_CHURN_EPOCH.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

pub fn is_active() -> bool { DISPLAY_CHURN_ACTIVE.load(Ordering::SeqCst) }

pub fn epoch() -> u64 { DISPLAY_CHURN_EPOCH.load(Ordering::SeqCst) }

pub fn flags() -> DisplayReconfigFlags {
    DisplayReconfigFlags::from_bits_truncate(DISPLAY_CHURN_FLAGS.load(Ordering::SeqCst) as u32)
}

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> std::ffi::c_int;
}

#[cfg(test)]
thread_local! {
    static TEST_SINCE_WINDOWS_LAST_MOVED: std::cell::RefCell<Option<Duration>> =
        const { std::cell::RefCell::new(None) };
}

/// What `since_windows_last_moved` answers under test, instead of asking the
/// window server about the machine the tests run on.
#[cfg(test)]
pub fn set_since_windows_last_moved(since: Option<Duration>) {
    TEST_SINCE_WINDOWS_LAST_MOVED.with(|cell| *cell.borrow_mut() = since);
}

/// How long ago the window server last moved windows between desktops for a
/// display reconfiguration, by its own clock. `None` if it never has.
///
/// The flags and epoch above track the reconfigurations rift is told about;
/// this is the window server's own account of when it last acted on one. That
/// distinction matters because the window server keeps reshuffling after the
/// event that announced the change has been handled and the churn flag
/// cleared — it reaps desktops seconds later — so this is the only honest
/// answer to whether something that just happened to the desktops was its
/// doing or the user's.
pub fn since_windows_last_moved() -> Option<Duration> {
    #[cfg(test)]
    return TEST_SINCE_WINDOWS_LAST_MOVED.with(|cell| *cell.borrow());
    #[allow(unreachable_code)]
    unsafe {
        let at = crate::sys::skylight::SLSGetDisplayReconfigureTimeWhenWindowsLastMoved();
        if at == 0 {
            return None;
        }
        let mut timebase = MachTimebase { numer: 0, denom: 0 };
        if mach_timebase_info(&mut timebase) != 0 || timebase.denom == 0 {
            return None;
        }
        let ticks = mach_absolute_time().saturating_sub(at);
        let nanos = ticks as u128 * timebase.numer as u128 / timebase.denom as u128;
        Some(Duration::from_nanos(nanos.min(u64::MAX as u128) as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_end_toggles_global_state() {
        let _ = begin(DisplayReconfigFlags::ADD);
        assert!(is_active());
        assert!(flags().contains(DisplayReconfigFlags::ADD));
        let _ = end();
        assert!(!is_active());
        assert!(flags().is_empty());
    }
}
