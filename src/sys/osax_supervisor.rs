//! Keeps the scripting addition alive across Dock restarts.
//!
//! The payload belongs to Dock, not to rift: it is code injected into a
//! process rift does not own, and every Dock restart drops it. Dock restarts
//! for ordinary reasons — a `killall Dock`, a settings change, a crash — and
//! the last of those is silent, because Dock relaunches in a blink and the
//! only visible consequence is that the addition's effects stop. The space
//! switch animation simply goes back to Dock's own timing, with nothing on
//! screen to say why.
//!
//! So rift watches. A handshake every couple of seconds says whether the
//! payload is still answering; when it stops, this asks for it back (the same
//! `sudo rift sa load` the user put in `run_on_start`, so rift never elevates
//! anything the user did not already ask it to) and replays the settings that
//! lived inside it. The same loop covers startup, where the payload does not
//! exist yet either: the first tick that finds one applies the settings, which
//! is why nothing here has to assume `run_on_start` finished first.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::common::config::SpaceSwitchAnimationSettings;
use crate::sys::{app, osax, scripting_addition};

/// How often the payload is asked whether it is still there. A connect and
/// fifteen bytes over a unix socket, so this is cheap enough to run steadily
/// and quick enough that a Dock crash costs seconds, not a session.
const POLL: Duration = Duration::from_secs(2);

/// How long the loop leaves `run_on_start`'s own `sa load` alone before
/// loading anything itself, so the two do not inject at once.
const STARTUP_GRACE: Duration = Duration::from_secs(8);

/// Waits between load attempts. A Dock that has just restarted refuses for a
/// second or two, and a load that fails for a lasting reason — a stale sudoers
/// rule — should not be retried in a tight loop.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

/// Starts the supervisor, and records the settings it is responsible for
/// replaying. Returns without waiting for the addition.
pub fn spawn(settings: &SpaceSwitchAnimationSettings, run_on_start: &[String]) {
    scripting_addition::desire_space_switch_animation(settings);
    let load = osax::sa_load_command(run_on_start);

    let spawned = thread::Builder::new()
        .name("osax-supervisor".to_string())
        .spawn(move || run(load));
    if let Err(error) = spawned {
        warn!(%error, "could not start the scripting addition supervisor; it will not be put back after a Dock restart");
    }
}

fn run(load: Option<Vec<String>>) {
    // The Dock pid and payload version already provisioned. A change in either
    // is a new payload holding none of rift's settings.
    let mut provisioned: Option<(i32, String)> = None;
    let mut attempt = 0usize;
    let mut next_attempt = Instant::now() + STARTUP_GRACE;
    let mut warned_unmanaged = false;

    loop {
        thread::sleep(POLL);

        // No Dock at all is a moment during login or a restart in flight;
        // there is nothing to inject into until it is back.
        let Some(dock) = app::dock_pid() else {
            provisioned = None;
            continue;
        };

        let Some(live) = scripting_addition::live_payload() else {
            if provisioned.take().is_some() {
                warn!("the scripting addition stopped answering (Dock restarted or crashed)");
                attempt = 0;
                next_attempt = Instant::now();
            }
            if Instant::now() < next_attempt {
                continue;
            }
            let Some(load) = load.as_deref() else {
                if !warned_unmanaged {
                    warned_unmanaged = true;
                    warn!(
                        "the scripting addition is not loaded and run_on_start has no \
                         'sudo rift sa load', so rift will not load it; add that line or run it \
                         by hand"
                    );
                }
                continue;
            };
            // An addition that is not on disk was uninstalled deliberately.
            if !osax::is_bundle_installed() {
                continue;
            }
            next_attempt = Instant::now() + BACKOFF[attempt.min(BACKOFF.len() - 1)];
            attempt += 1;
            match reload(load) {
                Ok(()) => info!("asked for the scripting addition back after a Dock restart"),
                // The reasons this fails last (a stale sudoers rule above all),
                // so it is said once a minute at worst, not every tick.
                Err(error) => warn!(
                    %error,
                    attempt,
                    "could not reload the scripting addition; if the rule is stale, \
                     'sudo rift sa install-sudoers' re-pins it"
                ),
            }
            continue;
        };

        let current = (dock, live.version);
        if provisioned.as_ref() == Some(&current) {
            continue;
        }
        match scripting_addition::reassert_space_switch_animation() {
            Ok(true) => info!(
                dock,
                version = %current.1,
                "scripting addition is live; re-applied the space switch animation"
            ),
            Ok(false) => {}
            Err(()) => warn!(
                "the scripting addition is live but would not take the space switch animation"
            ),
        }
        provisioned = Some(current);
        attempt = 0;
    }
}

/// The same command, with sudo told never to prompt.
///
/// `-n` is the whole reason this is safe to run unattended: with a pinned
/// sudoers rule it is passwordless, and without one sudo fails immediately
/// instead of waiting at a prompt nobody will ever see.
fn non_interactive(tokens: &[String]) -> Vec<String> {
    let mut args = tokens.to_vec();
    if args.first().is_some_and(|first| first == "sudo") && !args.iter().any(|arg| arg == "-n") {
        args.insert(1, "-n".to_string());
    }
    args
}

/// Runs the user's own `sa load` line, without a terminal to prompt at.
fn reload(tokens: &[String]) -> Result<(), String> {
    let args = non_interactive(tokens);
    let (program, rest) = args.split_first().expect("sa_load_command never returns empty");

    let output = Command::new(program)
        .args(rest)
        .output()
        .map_err(|error| format!("could not run '{}': {error}", args.join(" ")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "'{}' failed ({}): {}",
        args.join(" "),
        output.status,
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> { line.split_whitespace().map(str::to_string).collect() }

    #[test]
    fn sudo_is_made_non_interactive_so_an_unattended_load_cannot_hang() {
        assert_eq!(
            non_interactive(&words("sudo rift sa load")),
            words("sudo -n rift sa load")
        );
    }

    #[test]
    fn an_existing_n_is_left_alone_and_other_programs_are_untouched() {
        assert_eq!(
            non_interactive(&words("sudo -n rift sa load")),
            words("sudo -n rift sa load")
        );
        assert_eq!(
            non_interactive(&words("/usr/local/bin/load-sa")),
            words("/usr/local/bin/load-sa")
        );
    }

    #[test]
    fn backoff_is_clamped_to_its_last_step() {
        assert_eq!(BACKOFF[99usize.min(BACKOFF.len() - 1)], Duration::from_secs(60));
    }
}
