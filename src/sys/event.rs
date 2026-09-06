use std::convert::TryFrom;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGDisplayHideCursor, CGDisplayShowCursor, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventSourceStateID, kCGNullDirectDisplay,
};
use serde::{Deserialize, Serialize};

pub use super::window_server::current_cursor_location;
use crate::sys::cg_ok;
pub use crate::sys::hotkey::{Hotkey, HotkeySpec, KeyCode, Modifiers};
use crate::sys::skylight::{
    CFRelease, CGEventSourceCreate, CGEventSourceSetLocalEventsSuppressionInterval,
    CGWarpMouseCursorPosition,
};

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
#[repr(u8)]
pub enum MouseState {
    Up = 1,
    Down = 2,
}

const MOUSE_STATE_UNKNOWN: u8 = 0;

static MOUSE_STATE: AtomicU8 = AtomicU8::new(MOUSE_STATE_UNKNOWN);

const RIFT_SYNTHETIC_EVENT_MARKER: i64 = 0x5249_4654;
const KEYCODE_W: u16 = 0x0d;

impl From<MouseState> for u8 {
    fn from(state: MouseState) -> u8 { state as u8 }
}

impl TryFrom<u8> for MouseState {
    type Error = ();

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            x if x == MouseState::Up as u8 => Ok(MouseState::Up),
            x if x == MouseState::Down as u8 => Ok(MouseState::Down),
            _ => Err(()),
        }
    }
}

pub fn set_mouse_state(state: MouseState) { MOUSE_STATE.store(state.into(), Ordering::Relaxed); }

static LAST_MOUSE_UP_WAS_LEFT: AtomicBool = AtomicBool::new(false);

/// Remembers which button a release was of. The left button is the one that
/// asks to go somewhere; the right one opens a menu, and a menu is a place the
/// pointer has to stay.
pub fn set_last_mouse_up_was_left(left: bool) {
    LAST_MOUSE_UP_WAS_LEFT.store(left, Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    static TEST_LAST_MOUSE_UP_WAS_LEFT: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Makes `last_mouse_up_was_left` answer `left` on this thread, for tests.
#[cfg(test)]
pub fn set_last_mouse_up_was_left_override(left: Option<bool>) {
    TEST_LAST_MOUSE_UP_WAS_LEFT.with(|cell| cell.set(left));
}

pub fn last_mouse_up_was_left() -> bool {
    #[cfg(test)]
    if let Some(left) = TEST_LAST_MOUSE_UP_WAS_LEFT.with(|cell| cell.get()) {
        return left;
    }
    LAST_MOUSE_UP_WAS_LEFT.load(Ordering::Relaxed)
}

static KEY_PRESSED_SINCE_MOUSE_UP: AtomicBool = AtomicBool::new(false);

/// Whether the keyboard has spoken since the button last came up. A focus
/// change that lands shortly after a click is normally the click's own doing,
/// but not when a key was pressed in between: cmd-tab, cmd-`, a hotkey. The
/// tap flips this on at every real key press and off at every release of a
/// button, so it reflects the physical order of events however far behind
/// the reactor is in reading them.
pub fn note_key_pressed() { KEY_PRESSED_SINCE_MOUSE_UP.store(true, Ordering::Relaxed); }

pub fn note_mouse_up() { KEY_PRESSED_SINCE_MOUSE_UP.store(false, Ordering::Relaxed); }

#[cfg(test)]
thread_local! {
    static TEST_KEY_PRESSED_SINCE_MOUSE_UP: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Makes `key_pressed_since_mouse_up` answer `pressed` on this thread, for tests.
#[cfg(test)]
pub fn set_key_pressed_since_mouse_up_override(pressed: Option<bool>) {
    TEST_KEY_PRESSED_SINCE_MOUSE_UP.with(|cell| cell.set(pressed));
}

pub fn key_pressed_since_mouse_up() -> bool {
    #[cfg(test)]
    if let Some(pressed) = TEST_KEY_PRESSED_SINCE_MOUSE_UP.with(|cell| cell.get()) {
        return pressed;
    }
    crate::sys::trace::observe("key_since_mouse_up", (), || {
        KEY_PRESSED_SINCE_MOUSE_UP.load(Ordering::Relaxed)
    })
}

#[cfg(test)]
thread_local! {
    static TEST_MOUSE_STATE_OVERRIDE: std::cell::Cell<Option<MouseState>> =
        const { std::cell::Cell::new(None) };
}

/// Makes `get_mouse_state` answer `state` on this thread, for tests.
#[cfg(test)]
pub fn set_mouse_state_override(state: Option<MouseState>) {
    TEST_MOUSE_STATE_OVERRIDE.with(|cell| cell.set(state));
}

pub fn get_mouse_state() -> Option<MouseState> {
    #[cfg(test)]
    if let Some(state) = TEST_MOUSE_STATE_OVERRIDE.with(|cell| cell.get()) {
        return Some(state);
    }
    crate::sys::trace::observe("mouse_state", (), || MOUSE_STATE.load(Ordering::Relaxed))
        .try_into()
        .ok()
}

pub fn warp_mouse(point: CGPoint) -> Result<(), CGError> {
    let src = unsafe { CGEventSourceCreate(CGEventSourceStateID::CombinedSessionState) };
    unsafe { CGEventSourceSetLocalEventsSuppressionInterval(src, 0.0) };

    let res = cg_ok(unsafe { CGWarpMouseCursorPosition(point) });
    unsafe { CFRelease(src) };
    res
}

pub fn hide_mouse() -> Result<(), CGError> { cg_ok(CGDisplayHideCursor(kCGNullDirectDisplay)) }

pub fn show_mouse() -> Result<(), CGError> { cg_ok(CGDisplayShowCursor(kCGNullDirectDisplay)) }

/// Ask an application to handle its standard Command-W action.
///
/// Posting to the owning process preserves application-specific close behavior (for example,
/// closing a tab or prompting to save) instead of pressing the window's AX close button.
pub fn post_command_w(pid: crate::sys::app::pid_t) -> bool {
    let Some(key_down) = CGEvent::new_keyboard_event(None, KEYCODE_W, true) else {
        return false;
    };
    let Some(key_up) = CGEvent::new_keyboard_event(None, KEYCODE_W, false) else {
        return false;
    };

    for event in [&key_down, &key_up] {
        CGEvent::set_flags(Some(event), CGEventFlags::MaskCommand);
        CGEvent::set_integer_value_field(
            Some(event),
            CGEventField::EventSourceUserData,
            RIFT_SYNTHETIC_EVENT_MARKER,
        );
        CGEvent::post_to_pid(pid, Some(event));
    }
    true
}

pub fn is_rift_synthetic_event(event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData)
        == RIFT_SYNTHETIC_EVENT_MARKER
}

/// Closes a click for an app whose drag rift is taking over: the app saw the
/// press on its title/tab strip; this synthetic release turns it into a
/// completed click before the drag events are consumed, so the app is never
/// left holding half a gesture (see the swallowed-release bug). Tagged as
/// rift-synthetic so the event tap passes it through untouched.
pub fn post_synthetic_left_mouse_up(pid: crate::sys::app::pid_t, at: CGPoint) -> bool {
    use objc2_core_graphics::{CGEventType, CGMouseButton};
    let Some(up) =
        CGEvent::new_mouse_event(None, CGEventType::LeftMouseUp, at, CGMouseButton::Left)
    else {
        return false;
    };
    CGEvent::set_integer_value_field(
        Some(&up),
        CGEventField::EventSourceUserData,
        RIFT_SYNTHETIC_EVENT_MARKER,
    );
    CGEvent::post_to_pid(pid, Some(&up));
    true
}
