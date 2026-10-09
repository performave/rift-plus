//! Keeps space reserved for the docks of the apps named in
//! `settings.layout.external_docks`. [`crate::sys::external_dock`] finds and
//! measures a dock; this decides when to look again and when an answer counts.
//!
//! It runs on a thread of its own because a measurement is a few dozen
//! Accessibility round trips into the dock app, which must not hold up the
//! main thread when that app is slow to answer.
//!
//! The dock app says when it changes, but not always. DockDoor Pro reports a
//! layout change on every hover and might say nothing when a window is only
//! ordered out. So the window server is polled as well: one window-list read
//! every two seconds, only while a dock app is running, and a fresh
//! measurement whenever the dock's windows have moved.
//!
//! An answer has to hold for a moment before it moves any tiles. Otherwise a
//! drawer sliding out of the dock's edge, or an auto-hidden dock revealed under
//! the pointer, would rearrange every window on the display and put them back a
//! second later.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::CGRect;
use objc2_foundation::NSString;
use tracing::{debug, info, warn};

use crate::actor::{self, spaces};
use crate::sys::app::{NSRunningApplicationExt, pid_t};
use crate::sys::axuielement::AXUIElement;
use crate::sys::external_dock::{self, Edge, Reservation};
use crate::sys::observer::Observer;
use crate::sys::timer::Timer;

#[derive(Debug)]
pub enum Event {
    /// The bundle ids in `settings.layout.external_docks`.
    ConfigUpdated(Vec<String>),
    AppLaunched(pid_t, Option<String>),
    AppTerminated(pid_t),
    /// A watched dock app reported a change through Accessibility.
    Changed,
}

pub type Sender = actor::Sender<Event>;
pub type Receiver = actor::Receiver<Event>;

/// Registered on the application element, which reports them for every
/// element in the app.
const NOTIFICATIONS: &[&str] = &[
    "AXWindowCreated",
    "AXUIElementDestroyed",
    "AXWindowMoved",
    "AXWindowResized",
    "AXMoved",
    "AXResized",
    "AXLayoutChanged",
];

/// How long a dock app must stop reporting changes before it is measured. A
/// hover reports one with every movement of the pointer.
const QUIET: Duration = Duration::from_millis(200);
/// How long a changed answer must hold before tiles move for it.
const SETTLE: Duration = Duration::from_millis(700);
const POLL: Duration = Duration::from_secs(2);

pub struct ExternalDocks {
    bundle_ids: Vec<String>,
    rx: Receiver,
    tx: Sender,
    spaces_tx: spaces::Sender,
    watched: HashMap<pid_t, Watched>,
    published: Vec<Reservation>,
    /// A changed answer waiting to show it holds, and when it was first seen.
    pending: Option<(Vec<Reservation>, Instant)>,
    /// When a dock app last reported a change it has not been measured since.
    changed_at: Option<Instant>,
    next_poll: Instant,
}

struct Watched {
    _observer: Option<Observer>,
    /// Its dock windows, and the displays and edges they were on, as of the
    /// last poll.
    windows: Vec<(CGRect, CGRect, Edge)>,
    /// Its last good measurement, kept through an Accessibility failure.
    last: Vec<Reservation>,
}

impl ExternalDocks {
    pub fn new(
        bundle_ids: Vec<String>,
        tx: Sender,
        rx: Receiver,
        spaces_tx: spaces::Sender,
    ) -> Self {
        Self {
            bundle_ids,
            rx,
            tx,
            spaces_tx,
            watched: HashMap::new(),
            published: Vec::new(),
            pending: None,
            changed_at: None,
            next_poll: Instant::now() + POLL,
        }
    }

    pub async fn run(mut self) {
        self.rewatch();
        loop {
            let wake = self.next_wake();
            tokio::select! {
                event = self.rx.recv() => {
                    let Some((span, event)) = event else { break };
                    let _guard = span.enter();
                    self.handle_event(event);
                }
                // select builds disabled branches too; keep the timer inside
                // an async block so an idle actor schedules no wakeup.
                _ = async { Timer::sleep(wake.unwrap_or_default()).await }, if wake.is_some() => {
                    self.tick();
                }
            }
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::ConfigUpdated(bundle_ids) => {
                if bundle_ids != self.bundle_ids {
                    self.bundle_ids = bundle_ids;
                    self.rewatch();
                }
            }
            Event::AppLaunched(pid, bundle_id) => {
                if bundle_id.is_some_and(|id| self.bundle_ids.contains(&id)) {
                    self.watch(pid);
                }
            }
            Event::AppTerminated(pid) => {
                if self.watched.remove(&pid).is_some() {
                    // Gone for good, so there is nothing to wait out.
                    let answer = self.collect();
                    self.publish(answer);
                }
            }
            Event::Changed => self.changed_at = Some(Instant::now()),
        }
    }

    fn next_wake(&self) -> Option<Duration> {
        let mut deadline: Option<Instant> = None;
        let mut consider = |at: Instant| deadline = Some(deadline.map_or(at, |d| d.min(at)));
        if let Some(at) = self.changed_at {
            consider(at + QUIET);
        }
        if let Some((_, since)) = &self.pending {
            consider(*since + SETTLE);
        }
        if !self.watched.is_empty() {
            consider(self.next_poll);
        }
        deadline.map(|at| at.saturating_duration_since(Instant::now()))
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if !self.watched.is_empty() && now >= self.next_poll {
            self.next_poll = now + POLL;
            self.poll();
        }
        let quiet = self.changed_at.is_some_and(|at| now >= at + QUIET);
        let settled = self.pending.as_ref().is_some_and(|(_, since)| now >= *since + SETTLE);
        if quiet || settled {
            self.changed_at = None;
            self.measure();
            let answer = self.collect();
            self.offer(answer);
        }
    }

    fn poll(&mut self) {
        let mut moved = false;
        for (&pid, watched) in &mut self.watched {
            let windows = external_dock::docked_windows(pid);
            if windows != watched.windows {
                watched.windows = windows;
                moved = true;
            }
        }
        if moved {
            self.changed_at.get_or_insert_with(Instant::now);
        }
    }

    fn measure(&mut self) {
        for (&pid, watched) in &mut self.watched {
            match external_dock::measure(pid) {
                Ok(reservations) => watched.last = reservations,
                Err(err) => debug!(pid, ?err, "could not measure dock; keeping the last answer"),
            }
        }
    }

    fn collect(&self) -> Vec<Reservation> {
        let mut answer: Vec<Reservation> =
            self.watched.values().flat_map(|w| w.last.iter().copied()).collect();
        // A stable order, so equal answers compare equal.
        answer.sort_by(|a, b| {
            (a.display.origin.x, a.display.origin.y, a.thickness)
                .partial_cmp(&(b.display.origin.x, b.display.origin.y, b.thickness))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        answer
    }

    /// Publishes `answer` once it has held for [`SETTLE`].
    fn offer(&mut self, answer: Vec<Reservation>) {
        if answer == self.published {
            self.pending = None;
            return;
        }
        // The same docks, only on displays that moved or changed size: the
        // display change has rearranged everything already, so follow it now
        // rather than leave tiles under the dock while the answer settles.
        let only_displays_moved = answer.len() == self.published.len()
            && answer
                .iter()
                .zip(&self.published)
                .all(|(a, b)| a.edge == b.edge && a.thickness == b.thickness);
        if only_displays_moved {
            self.publish(answer);
            return;
        }
        match &self.pending {
            Some((pending, since)) if *pending == answer => {
                if since.elapsed() >= SETTLE {
                    self.publish(answer);
                }
            }
            _ => self.pending = Some((answer, Instant::now())),
        }
    }

    fn publish(&mut self, answer: Vec<Reservation>) {
        self.pending = None;
        if answer == self.published {
            return;
        }
        info!(?answer, "external dock reservations changed");
        self.published = answer.clone();
        external_dock::set_reservations(answer);
        self.spaces_tx.send(spaces::Event::ScreenRefreshRequested);
    }

    /// Brings the watched apps in line with the configured bundle ids, and
    /// publishes straight away: a config change or a fresh start is not a
    /// transient to wait out.
    fn rewatch(&mut self) {
        let running: Vec<pid_t> = self
            .bundle_ids
            .iter()
            .flat_map(|id| {
                NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(
                    id,
                ))
                .to_vec()
            })
            .map(|app| app.pid())
            .collect();
        self.watched.retain(|pid, _| running.contains(pid));
        for pid in running {
            self.watch(pid);
        }
        self.changed_at = None;
        self.measure();
        let answer = self.collect();
        self.publish(answer);
    }

    fn watch(&mut self, pid: pid_t) {
        if self.watched.contains_key(&pid) {
            return;
        }
        let observer = match Observer::new_with_notification(pid) {
            Ok(builder) => {
                let tx = self.tx.clone();
                let observer = builder.install_with_notification(move |_, _| {
                    tx.send(Event::Changed);
                });
                let app = AXUIElement::application(pid);
                for notification in NOTIFICATIONS {
                    if let Err(err) = observer.add_notification(&app, notification) {
                        debug!(pid, ?err, notification, "dock app notification unavailable");
                    }
                }
                Some(observer)
            }
            Err(err) => {
                warn!(pid, ?err, "could not observe dock app; relying on polling");
                None
            }
        };
        info!(pid, "watching external dock");
        self.watched.insert(pid, Watched {
            _observer: observer,
            windows: Vec::new(),
            last: Vec::new(),
        });
        self.changed_at = Some(Instant::now());
        self.next_poll = Instant::now() + POLL;
    }
}
