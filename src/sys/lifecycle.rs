//! Saving the layout on the way out, and often enough that a crash cannot take
//! it with it.
//!
//! rift could always write its layout and start from a written one, but only
//! when told to by hand. These are the two things that make it automatic: a
//! handler for the signal launchd sends to stop the service, and a heartbeat
//! that keeps the saved file current. The heartbeat is what a crash or a
//! `kill -9` falls back on, and its side effect matters as much as the save
//! itself — the file's mtime tracks the last moment rift was alive, so the age
//! of the snapshot at the next startup is the length of the gap.

use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use nix::libc;
use tracing::warn;

/// How long the saver gets before the process leaves anyway. A wedged reactor
/// must not hold the restart open until launchd loses patience and sends
/// SIGKILL, which on a dev rebuild is the difference between a pause and a
/// twenty-second wait.
const SAVE_DEADLINE: Duration = Duration::from_secs(5);

/// The write end of the self-pipe the handler pokes. A signal handler may do
/// almost nothing safely; writing a byte to a pipe is one of the things it may.
static TERMINATION_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn note_termination(_signal: c_int) {
    let fd = TERMINATION_PIPE.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let byte = 1u8;
    // Nothing to do about a failed write from inside a handler, and the
    // deadline below covers it.
    unsafe { libc::write(fd, std::ptr::from_ref(&byte).cast::<c_void>(), 1) };
}

/// Calls `save` once, on the first SIGTERM or SIGINT, and gives it
/// `SAVE_DEADLINE` to end the process itself before leaving without it.
///
/// `save` runs on an ordinary thread, not in the handler, so it may do whatever
/// it likes — the handler only wakes it.
pub fn save_on_termination(save: impl FnOnce() + Send + 'static) {
    let mut fds = [0 as c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        warn!("Could not open the termination pipe; the layout will not be saved on shutdown");
        return;
    }
    let [read_fd, write_fd] = fds;
    TERMINATION_PIPE.store(write_fd, Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        unsafe { libc::signal(signal, note_termination as *const () as libc::sighandler_t) };
    }

    thread::spawn(move || {
        let mut byte = [0u8; 1];
        loop {
            let read = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast::<c_void>(), 1) };
            if read == 1 {
                break;
            }
            // A handler firing during the read interrupts it; anything else
            // means the pipe is gone and no signal will ever arrive.
            if read < 0 && nix::errno::Errno::last() == nix::errno::Errno::EINTR {
                continue;
            }
            return;
        }
        thread::spawn(|| {
            thread::sleep(SAVE_DEADLINE);
            warn!("Layout save did not finish in time; exiting anyway");
            std::process::exit(0);
        });
        save();
    });
}

/// Calls `save` every `interval` for as long as the process lives.
pub fn save_periodically(interval: Duration, save: impl Fn() + Send + 'static) {
    if interval.is_zero() {
        return;
    }
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            save();
        }
    });
}
