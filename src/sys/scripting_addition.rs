//! Client for rift's scripting addition.
//!
//! Some things the window server used to allow have no unprivileged API left on
//! macOS 26 — moving a window to a specific space is the one that matters here.
//! Every route that does not need elevated privileges was probed and is gone:
//! `SLSPerformAsynchronousBridgedWindowManagementOperation` no longer exports,
//! `SLSSetWindowListWorkspace` answers `kCGErrorNotImplemented`,
//! `SLSMoveWindowsToManagedSpace` accepts the call and does nothing, and
//! `CGSAddWindowsToSpaces` is no longer in SkyLight at all.
//!
//! What remains is code running *inside* Dock, which is what a scripting
//! addition is: a payload injected into Dock that listens on a unix socket.
//! [`crate::sys::osax`] builds, installs and injects that payload; this module
//! is its client, and a command is fifteen bytes. The important detail is that
//! the payload belongs to Dock, not to rift — it keeps answering while rift is
//! not running, and a Dock restart drops it until `sudo rift sa load` runs
//! again.
//!
//! This is strictly opt-in and strictly a fallback: rift's own paths are used
//! wherever one exists, and every function here reports failure rather than
//! pretending, so a machine without the addition simply does not get these
//! commands.
//!
//! Wire format, from yabai's `src/sa.m`, which the vendored payload keeps:
//!
//! ```text
//! [i16 length][u8 opcode][args, native endian]
//! ```
//!
//! where `length` counts the opcode and args, i.e. the frame size minus its own
//! two bytes. The server closes the connection once it has acted; each command
//! is its own short-lived connection, so there is no session to keep. Two
//! commands answer first: [`handshake`], with a NUL-terminated version string
//! followed by a `u32` of attribute flags, and [`focus_space`], with one byte
//! saying whether the switch was issued.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tracing::{debug, warn};

/// The payload's opcodes. Only the ones rift has no other way to perform are
/// listed; the numbering belongs to `enum sa_opcode` in `src/osax/common.h`.
mod opcode {
    pub const HANDSHAKE: u8 = 0x01;
    pub const SPACE_FOCUS: u8 = 0x02;
    pub const SPACE_CREATE: u8 = 0x03;
    pub const SPACE_MOVE: u8 = 0x05;
    pub const SPACE_DESTROY: u8 = 0x04;
    pub const WINDOW_TO_SPACE: u8 = 0x13;
    pub const SPACE_SWITCH_ANIMATION: u8 = 0x14;
}

/// Which Dock internals the payload managed to find, one flag each, matching
/// the `OSAX_ATTRIB_*` defines in `src/osax/common.h`.
pub mod attrib {
    /// `dock_spaces`, the object every space command goes through.
    pub const DOCK_SPACES: u32 = 0x01;
    /// The desktop picture manager, which a space move has to be told about.
    pub const DPPM: u32 = 0x02;
    pub const ADD_SPACE: u32 = 0x04;
    pub const REM_SPACE: u32 = 0x08;
    pub const MOV_SPACE: u32 = 0x10;
    /// Dock's "set front window", which rift does its own way.
    pub const SET_WINDOW: u32 = 0x20;
    /// The instruction the payload patches to zero out Dock's space-change
    /// animation. rift never asks for this and does not need it.
    pub const ANIM_TIME: u32 = 0x40;
    /// The routine Dock steps the trackpad space switch with, which
    /// [`super::set_space_switch_animation`] hooks. Optional: only the
    /// `space_switch_animation` setting needs it.
    pub const SPACE_STEP: u32 = 0x80;

    /// The flags whose absence would actually cost rift a command, and the
    /// command each one is for:
    ///
    /// | flag | needed by |
    /// |---|---|
    /// | `DOCK_SPACES` | every space command |
    /// | `ADD_SPACE` | [`super::create_space`] |
    /// | `REM_SPACE` | [`super::destroy_space`] |
    /// | `MOV_SPACE`, `DPPM` | [`super::move_space_after_space`] |
    ///
    /// `SET_WINDOW` and `ANIM_TIME` are deliberately absent: they belong to
    /// yabai commands rift does not send. [`super::move_window_to_space`],
    /// the one that matters most, needs no flag at all — inside Dock it is a
    /// plain `SLSMoveWindowsToManagedSpace` that works from there and nowhere
    /// else.
    pub const REQUIRED: u32 = DOCK_SPACES | DPPM | ADD_SPACE | REM_SPACE | MOV_SPACE;

    /// Names the flags in `wanted` that `reported` does not have, for messages.
    pub fn missing(reported: u32, wanted: u32) -> Vec<&'static str> {
        [
            (DOCK_SPACES, "dock.spaces"),
            (DPPM, "desktop picture manager"),
            (ADD_SPACE, "add space"),
            (REM_SPACE, "remove space"),
            (MOV_SPACE, "move space"),
            (SET_WINDOW, "set front window"),
            (ANIM_TIME, "animation time"),
            (SPACE_STEP, "space switch step"),
        ]
        .into_iter()
        .filter(|(flag, _)| wanted & flag != 0 && reported & flag == 0)
        .map(|(_, name)| name)
        .collect()
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// The socket a payload running inside `user`'s Dock serves, matching
/// `SA_SOCKET_PATH_FMT` in `src/osax/common.h`.
pub fn socket_path_for_user(user: &str) -> String { format!("/tmp/rift-sa_{user}.socket") }

fn socket_path() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    Some(socket_path_for_user(&user))
}

/// What a payload answers a handshake with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// The payload's compiled-in `OSAX_VERSION`, which is the build inside Dock
    /// right now — not whatever is installed on disk.
    pub version: String,
    /// Which Dock internals the payload found, as [`attrib`] flags. What
    /// matters is [`Self::missing`], not whether every flag is set.
    pub attributes: u32,
}

impl Handshake {
    /// The names of the flags rift needs and this payload does not have.
    ///
    /// Empty is the answer to look for. A payload can report less than every
    /// flag and still serve rift completely: a second payload in the same Dock
    /// will not find `ANIM_TIME`, because finding it means matching an
    /// instruction the first payload has already overwritten.
    pub fn missing(&self) -> Vec<&'static str> {
        attrib::missing(self.attributes, attrib::REQUIRED)
    }
}

/// Asks the payload at `path` what it is, without acting on anything.
///
/// This is the only honest test of whether the addition is live: the bundle can
/// be installed and current while the Dock holding it has been restarted out
/// from under it.
pub fn handshake(path: &str) -> Option<Handshake> {
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));

    stream.write_all(&[0x01, 0x00, opcode::HANDSHAKE]).ok()?;

    // The reply is `version\0`, four bytes of attributes, then a newline, and
    // the payload closes the connection behind it.
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).ok()?;

    let nul = reply.iter().position(|byte| *byte == 0)?;
    let version = String::from_utf8(reply[..nul].to_vec()).ok()?;
    let attributes = reply.get(nul + 1..nul + 5)?.try_into().ok()?;

    Some(Handshake {
        version,
        attributes: u32::from_ne_bytes(attributes),
    })
}

/// The payload answering right now, if there is one.
///
/// The honest liveness test, and the one the supervisor runs on: it reaches
/// the payload inside the Dock that is running, not the bundle on disk.
pub fn live_payload() -> Option<Handshake> { socket_path().and_then(|path| handshake(&path)) }

/// Whether the scripting addition is loaded and accepting connections.
pub fn is_available() -> bool {
    #[cfg(test)]
    {
        return test_hooks::available();
    }
    #[allow(unreachable_code)]
    socket_path().is_some_and(|path| UnixStream::connect(path).is_ok())
}

/// The test suite runs on developer machines that may well have the addition
/// loaded, and a command sent to it there acts on the real Dock. Under test
/// the socket is never touched: commands are recorded instead, and whether
/// the addition "is available" is whatever the test says.
#[cfg(test)]
pub mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static AVAILABLE: Cell<bool> = const { Cell::new(false) };
        static SENT: RefCell<Vec<(u8, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
        static NEXT_CREATED: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub fn available() -> bool { AVAILABLE.with(|available| available.get()) }

    pub fn set_available(available: bool) {
        AVAILABLE.with(|cell| cell.set(available));
        SENT.with(|sent| sent.borrow_mut().clear());
        NEXT_CREATED.with(|cell| cell.set(None));
    }

    /// The id `create_space_after` reports for the next desktop it creates.
    pub fn set_next_created_space(space: Option<u64>) { NEXT_CREATED.with(|cell| cell.set(space)); }

    pub(super) fn next_created_space() -> Option<u64> { NEXT_CREATED.with(|cell| cell.get()) }

    /// `(space, after)` of every desktop-reorder command sent, in order.
    pub fn space_moves() -> Vec<(u64, u64)> {
        SENT.with(|sent| {
            sent.borrow()
                .iter()
                .filter(|(op, _)| *op == super::opcode::SPACE_MOVE)
                .map(|(_, args)| {
                    (
                        u64::from_ne_bytes(args[0..8].try_into().unwrap()),
                        u64::from_ne_bytes(args[8..16].try_into().unwrap()),
                    )
                })
                .collect()
        })
    }

    /// Every desktop-destruction command sent, in order.
    pub fn space_destroys() -> Vec<u64> {
        SENT.with(|sent| {
            sent.borrow()
                .iter()
                .filter(|(op, _)| *op == super::opcode::SPACE_DESTROY)
                .map(|(_, args)| u64::from_ne_bytes(args[0..8].try_into().unwrap()))
                .collect()
        })
    }

    /// The anchor of every desktop-creation command sent, in order.
    pub fn space_creations() -> Vec<u64> {
        SENT.with(|sent| {
            sent.borrow()
                .iter()
                .filter(|(op, _)| *op == super::opcode::SPACE_CREATE)
                .map(|(_, args)| u64::from_ne_bytes(args[0..8].try_into().unwrap()))
                .collect()
        })
    }

    pub(super) fn record(op: u8, args: &[u8]) -> bool {
        if !available() {
            return false;
        }
        SENT.with(|sent| sent.borrow_mut().push((op, args.to_vec())));
        true
    }

    /// `(window server id, space)` of every window-to-space command sent, in order.
    pub fn window_moves() -> Vec<(u32, u64)> {
        SENT.with(|sent| {
            sent.borrow()
                .iter()
                .filter(|(op, _)| *op == super::opcode::WINDOW_TO_SPACE)
                .map(|(_, args)| {
                    let space = u64::from_ne_bytes(args[0..8].try_into().unwrap());
                    let window = u32::from_ne_bytes(args[8..12].try_into().unwrap());
                    (window, space)
                })
                .collect()
        })
    }

    /// Every space-focus command sent, in order.
    pub fn space_focuses() -> Vec<u64> {
        SENT.with(|sent| {
            sent.borrow()
                .iter()
                .filter(|(op, _)| *op == super::opcode::SPACE_FOCUS)
                .map(|(_, args)| u64::from_ne_bytes(args[0..8].try_into().unwrap()))
                .collect()
        })
    }
}

/// Sends one command. Returns whether the addition accepted it.
///
/// A missing socket is the normal case on a machine without the addition, and
/// is logged at debug rather than warn so it does not read as a fault.
fn send(op: u8, args: &[u8]) -> bool {
    #[cfg(test)]
    {
        return test_hooks::record(op, args);
    }
    #[allow(unreachable_code)]
    send_frame(op, args).is_some()
}

/// Delivers one command and returns whatever the payload wrote back before
/// closing, which for most commands is nothing. `None` means the command
/// never reached the payload.
///
/// The read waits for the close, and the payload closes only after acting,
/// so a returned reply means the command has been carried out, not merely
/// queued.
fn send_frame(op: u8, args: &[u8]) -> Option<Vec<u8>> {
    let path = socket_path()?;

    let body_len = 1 + args.len();
    let Ok(header) = i16::try_from(body_len) else {
        warn!("scripting addition command too large");
        return None;
    };

    let mut frame = Vec::with_capacity(2 + body_len);
    frame.extend_from_slice(&header.to_ne_bytes());
    frame.push(op);
    frame.extend_from_slice(args);

    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            debug!(%error, %path, "scripting addition is not loaded");
            return None;
        }
    };
    let _ = stream.set_write_timeout(Some(CONNECT_TIMEOUT));
    let _ = stream.set_read_timeout(Some(CONNECT_TIMEOUT));

    if let Err(error) = stream.write_all(&frame) {
        warn!(%error, op, "failed to send scripting addition command");
        return None;
    }

    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    Some(reply)
}

/// Moves a window to a space, both by their window-server ids.
pub fn move_window_to_space(window_server_id: u32, space: u64) -> bool {
    crate::sys::trace::observe("sa_move_window_to_space", (window_server_id, space), || {
        let mut args = Vec::with_capacity(12);
        args.extend_from_slice(&space.to_ne_bytes());
        args.extend_from_slice(&window_server_id.to_ne_bytes());
        send(opcode::WINDOW_TO_SPACE, &args)
    })
}

/// Focuses a space by id, with no animation whatsoever.
///
/// Inside Dock this is `SLSShowSpaces` / `SLSHideSpaces` /
/// `SLSManagedDisplaySetCurrentSpace` plus a patch of Dock's own
/// `_currentSpace` ivar — a teleport, not a gesture, so nothing slides. The
/// synthetic-swipe path in `sys::space_switch` still shows a single frame of
/// movement because it drives the Dock's real swipe machinery; this does not.
///
/// True means the payload issued the switch (or the space was already
/// current), false that it refused — the addition is not loaded, or Dock did
/// not know the space. That verdict is the whole basis for falling back to a
/// gesture, so it comes from the payload itself rather than from watching
/// the window server afterwards: the payload serves commands one at a time
/// and answers after the switch is issued, whereas a readback from here can
/// stall long enough under load to look like a miss. A swipe posted on top
/// of a switch that did happen lands one space too far.
pub fn focus_space(space: u64) -> bool {
    crate::sys::trace::observe("sa_focus_space", space, || {
        send_for_verdict(opcode::SPACE_FOCUS, &space.to_ne_bytes())
    })
}

/// Like [`send`], for the commands the payload answers with a verdict byte.
///
/// No byte at all is read as accepted, not refused: it is what a payload from
/// before the verdict existed answers, and one of those has already acted by
/// the time it closes the connection.
fn send_for_verdict(op: u8, args: &[u8]) -> bool {
    #[cfg(test)]
    {
        return test_hooks::record(op, args);
    }
    #[allow(unreachable_code)]
    match send_frame(op, args) {
        Some(reply) => verdict(&reply),
        None => false,
    }
}

/// Reads a verdict reply: `[0]` is a refusal, anything else is acceptance.
fn verdict(reply: &[u8]) -> bool { reply.first().is_none_or(|byte| *byte != 0) }

/// Creates a space on the display holding `space`.
pub fn create_space(space: u64) -> bool { send(opcode::SPACE_CREATE, &space.to_ne_bytes()) }

/// Creates a desktop after `anchor`, on that desktop's display, and says
/// which one it is: the window server lists it as soon as the addition has
/// made it, so the space that was not there before is the new one. `None`
/// when the addition refused or the new desktop cannot be told apart.
pub fn create_space_after(
    anchor: crate::sys::screen::SpaceId,
) -> Option<crate::sys::screen::SpaceId> {
    #[cfg(test)]
    {
        if !create_space(anchor.get()) {
            return None;
        }
        return test_hooks::next_created_space().map(crate::sys::screen::SpaceId::new);
    }
    #[allow(unreachable_code)]
    {
        let before = crate::sys::space_switch::all_spaces_in_display_order();
        if !create_space(anchor.get()) {
            return None;
        }
        crate::sys::space_switch::all_spaces_in_display_order()
            .into_iter()
            .find(|space| !before.contains(space))
    }
}

/// Reorders `space` to sit immediately after `after` on the same display,
/// optionally focusing it.
///
/// The third argument is a slot yabai uses when moving a space between
/// displays and leaves zeroed otherwise.
pub fn move_space_after_space(space: u64, after: u64, focus: bool) -> bool {
    let mut args = Vec::with_capacity(25);
    args.extend_from_slice(&space.to_ne_bytes());
    args.extend_from_slice(&after.to_ne_bytes());
    args.extend_from_slice(&0u64.to_ne_bytes());
    args.push(u8::from(focus));
    send(opcode::SPACE_MOVE, &args)
}

/// Destroys a space by id.
pub fn destroy_space(space: u64) -> bool { send(opcode::SPACE_DESTROY, &space.to_ne_bytes()) }

/// Sets the timing of the trackpad space switch, or gives Dock its own back.
///
/// After a swipe between spaces is released, Dock finishes the slide with a
/// velocity spring it steps from a timer. The payload hooks that step and,
/// while a duration is set, moves the spaces along `bezier` — a CSS-style
/// cubic bezier through (0,0), (x1,y1), (x2,y2), (1,1) — over `duration`
/// instead. `None` restores the original instructions. The finger tracking
/// before the release is Dock's either way.
///
/// The payload keeps the setting until it is told otherwise or Dock restarts,
/// so this is sent at startup and on every config reload rather than per
/// switch. Needs [`attrib::SPACE_STEP`], which the payload only reports on a
/// Dock whose routine it recognised.
pub fn set_space_switch_animation(animation: Option<(Duration, [f64; 4])>) -> bool {
    let (duration, bezier) = match animation {
        Some((duration, bezier)) => (duration.as_secs_f64(), bezier),
        None => (0.0, [0.0; 4]),
    };
    let mut args = Vec::with_capacity(5 * size_of::<f64>());
    args.extend_from_slice(&duration.to_ne_bytes());
    for point in bezier {
        args.extend_from_slice(&point.to_ne_bytes());
    }
    send(opcode::SPACE_SWITCH_ANIMATION, &args)
}

/// Applies the `space_switch_animation` setting to the payload.
///
/// Disabled means asking for Dock's own animation back, which matters after a
/// reload that turned the setting off. When the setting is on and nothing
/// answers, that is worth a warning: the user asked for a timing they are not
/// getting, and `rift sa status` says why.
pub fn apply_space_switch_animation(
    settings: &crate::common::config::SpaceSwitchAnimationSettings,
) {
    let animation = desire_space_switch_animation(settings);
    if !set_space_switch_animation(animation) && settings.enabled {
        warn!(
            "space_switch_animation is enabled but the scripting addition did not take it; \
             it needs the addition loaded in a Dock it recognises (see 'rift sa status')"
        );
    }
}

/// What the payload is meant to be holding, so that a payload appearing later
/// can be given it.
///
/// The setting lives inside Dock, so every Dock restart — a crash included —
/// silently drops it, and the only signal is that the swipe goes back to
/// Dock's own timing. [`crate::sys::osax_supervisor`] watches for that and
/// replays this.
static DESIRED_ANIMATION: Lazy<Mutex<Option<Option<(Duration, [f64; 4])>>>> =
    Lazy::new(|| Mutex::new(None));

/// Records the setting without sending it, and returns it in wire form.
pub fn desire_space_switch_animation(
    settings: &crate::common::config::SpaceSwitchAnimationSettings,
) -> Option<(Duration, [f64; 4])> {
    let animation = settings.enabled.then(|| {
        (
            Duration::from_millis(settings.duration_ms),
            settings.easing.bezier(),
        )
    });
    *DESIRED_ANIMATION.lock() = Some(animation);
    animation
}

/// Sends the recorded setting to a payload that has just appeared.
///
/// `Ok(false)` means nothing has been asked for yet, which is not a failure.
pub fn reassert_space_switch_animation() -> Result<bool, ()> {
    let Some(animation) = *DESIRED_ANIMATION.lock() else {
        return Ok(false);
    };
    if set_space_switch_animation(animation) {
        Ok(true)
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_reads_the_payload_byte_and_tolerates_none() {
        assert!(verdict(&[1]));
        assert!(!verdict(&[0]));
        assert!(
            verdict(&[]),
            "a pre-verdict payload has acted by the time it closes"
        );
    }

    #[test]
    fn required_attributes_exclude_what_rift_never_sends() {
        // Observed on macOS 26.6.2: a payload loaded into a Dock that already
        // holds another one reports 0x3f, never 0x40 -- finding ANIM_TIME means
        // matching an instruction the first payload has already overwritten.
        // rift sends no opcode that needs it, so 0x3f is healthy.
        let handshake = Handshake {
            version: "1.0.0".into(),
            attributes: 0x3f,
        };
        assert!(handshake.missing().is_empty());

        assert_eq!(attrib::REQUIRED & attrib::ANIM_TIME, 0);
        assert_eq!(attrib::REQUIRED & attrib::SET_WINDOW, 0);
    }

    #[test]
    fn missing_required_attributes_are_named() {
        let handshake = Handshake {
            version: "1.0.0".into(),
            attributes: attrib::REQUIRED & !attrib::ADD_SPACE & !attrib::MOV_SPACE,
        };
        assert_eq!(handshake.missing(), vec!["add space", "move space"]);
    }

    #[test]
    fn a_payload_that_found_nothing_is_missing_every_requirement() {
        let handshake = Handshake {
            version: "1.0.0".into(),
            attributes: 0,
        };
        assert_eq!(handshake.missing().len(), 5);
    }

    #[test]
    fn frame_layout_matches_the_payload() {
        // move_window_to_space packs sid then wid after the opcode, so the
        // frame is 2 header + 1 opcode + 8 + 4 = 15 bytes, and the length
        // header counts everything but itself.
        let args_len = size_of::<u64>() + size_of::<u32>();
        let body_len = 1 + args_len;
        assert_eq!(body_len, 13);
        assert_eq!(2 + body_len, 15);
    }
}
