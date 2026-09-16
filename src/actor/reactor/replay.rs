//! Recording the reactor's input and replaying it offline.
//!
//! A trace holds the config, the layout, and then every event the reactor
//! handled together with every answer it got from the system while handling
//! them (see `sys::trace`). Replaying one through a fresh reactor reproduces
//! the recorded session exactly, with the frames rift would have written
//! captured instead of sent — and checked against the invariants a window
//! manager must keep no matter what the apps and the window server do.
//!
//! Record on the running instance with `rift-cli execute trace start
//! <name>.trace` / `... trace stop`; drop the file into `tests/traces` and it
//! is replayed on every `cargo test`.

use std::cell::RefCell;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use objc2_core_foundation::CGRect;
#[cfg(test)]
use tempfile::NamedTempFile;
use tracing::Span;

use super::{Event, Reactor};
use crate::actor::app::{AppThreadHandle, Request, WindowId, WindowInventoryToken};
use crate::actor::{self};
use crate::common::config::Config;
use crate::layout_engine::LayoutEngine;
use crate::sys::event::MouseState;
use crate::sys::geometry::{CGRectExt, SameAs};
use crate::sys::trace::{self, SysLine};

thread_local! {
    static DESERIALIZE_THREAD_HANDLE: RefCell<Option<AppThreadHandle>> = RefCell::new(None);
}

pub(super) fn deserialize_app_thread_handle() -> AppThreadHandle {
    DESERIALIZE_THREAD_HANDLE
        .with(|handle| handle.borrow().clone().expect("No deserialize thread handle set!"))
}

pub struct Record {
    file: Option<File>,
    #[cfg(test)]
    temp: Option<NamedTempFile>,
}

impl Record {
    pub fn new(path: Option<&Path>) -> Self {
        Self {
            file: path.map(|path| File::create(path).unwrap()),
            #[cfg(test)]
            temp: None,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(temp: NamedTempFile) -> Self { Self { file: None, temp: Some(temp) } }

    #[cfg(test)]
    #[allow(unused)]
    pub(super) fn temp(&mut self) -> Option<&mut NamedTempFile> { self.temp.as_mut() }

    fn file(&mut self) -> Option<&mut File> {
        #[cfg(test)]
        return self.file.as_mut().or(self.temp.as_mut().map(|temp| temp.as_file_mut()));
        #[cfg(not(test))]
        self.file.as_mut()
    }

    pub(super) fn start(&mut self, config: &Config, layout: &LayoutEngine) {
        self.start_with_state(config, layout, None, None);
    }

    /// Starts a recording whose header also carries the transaction store,
    /// so the replay's frame writes carry the ids the live ones did and the
    /// apps' echoed reports are accepted or discarded exactly as live.
    pub(super) fn start_with_state(
        &mut self,
        config: &Config,
        layout: &LayoutEngine,
        windows: Option<&crate::model::window_store::WindowStore>,
        transactions: Option<&super::transaction_manager::TransactionManager>,
    ) {
        // JSON, not RON: `LayoutSettings` is `#[serde(flatten)]`ed, which RON
        // cannot read back when it holds enum values.
        let config = serde_json::to_string(&config).unwrap();
        let layout = layout.serialize_to_string();
        // A real recording goes through `sys::trace`, which interleaves the
        // system's answers with the events. The test temp file keeps the
        // old event-only shape.
        if let Some(file) = self.file.take() {
            trace::start_recording(file);
            trace::write_line(&config);
            trace::write_line(&layout);
            if let Some(windows) = windows
                && let Ok(json) = serde_json::to_string(windows)
            {
                trace::write_line(&format!("Windows {json}"));
            }
            if let Some(transactions) = transactions
                && let Ok(json) = serde_json::to_string(&transactions.store.snapshot())
            {
                trace::write_line(&format!("Transactions {json}"));
            }
            return;
        }
        let Some(file) = self.file() else { return };
        write!(file, "{config}\n{layout}\n").unwrap();
    }

    pub(super) fn on_event(&mut self, event: &Event) {
        if trace::is_recording() {
            if let Ok(json) = serde_json::to_string(&event) {
                trace::write_line(&format!("Ev {} {json}", trace::elapsed_ms()));
            }
            return;
        }
        let Ok(line) = ron::ser::to_string(&event) else {
            return;
        };
        let Some(file) = self.file() else { return };
        write!(file, "{line}\n").unwrap();
    }
}

/// One line of a trace after its two header lines.
#[derive(Debug)]
pub enum TraceLine {
    Event {
        ms: u64,
        event: Event,
    },
    Sys(SysLine),
    /// A frame written live.
    Out(trace::OutLine),
}

type ReadTrace = (
    Config,
    LayoutEngine,
    Option<crate::model::window_store::WindowStore>,
    Vec<TraceLine>,
    Option<Vec<crate::model::tx_store::TxSnapshotEntry>>,
);

/// Reads a trace, giving every app in it `handle` to send its requests to.
/// The token a `WindowsDiscovered` gets when the trace predates tokens.
/// Live tokens start at request id 1, so this never collides with one.
const LEGACY_INVENTORY_TOKEN: WindowInventoryToken = WindowInventoryToken {
    request_id: 0,
    topology_revision: 0,
};

/// Traces recorded before an inventory carried a token (or a `successful`
/// flag) still replay: the missing fields are filled in here, and the
/// placeholder token is swapped for the one the replayed reactor has in
/// flight when the event is fed to it — see `fill_legacy_inventory_token`.
fn legacy_windows_discovered(json: &str) -> Option<Event> {
    let mut value: serde_json::Value = serde_json::from_str(json).ok()?;
    let fields = value.get_mut("WindowsDiscovered")?.as_object_mut()?;
    if !fields.contains_key("token") {
        fields.insert(
            "token".to_string(),
            serde_json::to_value(LEGACY_INVENTORY_TOKEN).ok()?,
        );
    }
    fields.entry("successful").or_insert(serde_json::Value::Bool(true));
    serde_json::from_value(value).ok()
}

/// A recorded reactor accepted every inventory it was handed; the replayed
/// one only accepts the request it has in flight for that app, so answer
/// that one. With nothing in flight the placeholder stays and the reactor
/// discards the inventory as unsolicited, as it would live.
fn fill_legacy_inventory_token(reactor: &Reactor, event: &mut Event) {
    if let Event::WindowsDiscovered { pid, token, .. } = event
        && *token == LEGACY_INVENTORY_TOKEN
        && let Some(in_flight) = reactor.window_inventory_manager.in_flight.get(pid)
    {
        *token = *in_flight;
    }
}

fn read_trace_with_handle(path: &Path, handle: AppThreadHandle) -> anyhow::Result<ReadTrace> {
    let file = BufReader::new(File::open(path)?);
    DESERIALIZE_THREAD_HANDLE.with(|h| h.borrow_mut().replace(handle));
    let mut lines = file.lines();
    let header = lines.next().expect("Empty trace")?;
    let config = serde_json::from_str(&header)
        .or_else(|_| ron::de::from_str(&header))
        .map_err(|error: ron::error::SpannedError| anyhow::anyhow!("config header: {error}"))?;
    let layout =
        LayoutEngine::deserialize_snapshot_from_str(&lines.next().expect("Expected layout line")?)?;
    let mut windows = None;
    let mut transactions = None;
    let mut out = Vec::new();
    for (number, line) in lines.enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Windows ") {
            windows = Some(serde_json::from_str(rest)?);
        } else if let Some(rest) = line.strip_prefix("Transactions ") {
            transactions = Some(serde_json::from_str(rest)?);
        } else if let Some(rest) = line.strip_prefix("Out ") {
            out.push(TraceLine::Out(serde_json::from_str(rest)?));
        } else if let Some(rest) = line.strip_prefix("Ev ") {
            let (ms, event) = rest.split_once(' ').expect("Ev line without an event");
            let event = match serde_json::from_str(event) {
                Ok(event) => event,
                Err(json_error) => match legacy_windows_discovered(event) {
                    Some(event) => event,
                    None => ron::de::from_str(event).map_err(|_| {
                        anyhow::anyhow!(
                            "line {}: event does not deserialize: {json_error}: {}",
                            number + 3,
                            &event[..event.len().min(160)]
                        )
                    })?,
                },
            };
            out.push(TraceLine::Event { ms: ms.parse()?, event });
        } else if let Some(rest) = line.strip_prefix("Sys ") {
            out.push(TraceLine::Sys(serde_json::from_str(rest)?));
        } else if let Some(rest) = line.strip_prefix("Sys") {
            out.push(TraceLine::Sys(ron::de::from_str(rest)?));
        } else if line.starts_with("Act ") {
            // Off-reactor activity notes (event tap, app actors, resolver):
            // for humans reading the trace, inert on replay.
            continue;
        } else if line.starts_with("Flight ") {
            anyhow::bail!(
                "this is a flight-recorder dump (`rift-cli execute trace dump`): it has no \
                 state header and is for reading, not replay"
            );
        } else {
            out.push(TraceLine::Event {
                ms: 0,
                event: ron::de::from_str(&line)?,
            });
        }
    }
    Ok((config, layout, windows, out, transactions))
}

/// Replays a trace's events through a fresh reactor, handing each request
/// the reactor makes of an app to `on_request`. The system's recorded
/// answers are replayed too.
pub fn replay(
    path: &Path,
    mut on_request: impl FnMut(Span, Request) + Send + 'static,
) -> anyhow::Result<()> {
    let report = replay_trace_with(path, |span, request| on_request(span, request))?;
    tracing::info!(
        events = report.events,
        writes = report.writes.len(),
        violations = report.violations.len(),
        misses = report.misses.len(),
        "Replay finished"
    );
    Ok(())
}

/// A frame rift wrote to a window during a replay.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayWrite {
    pub ms: u64,
    pub window: WindowId,
    pub frame: CGRect,
    /// Index of the trace line that caused it.
    pub event_index: usize,
}

/// What a replay did, and where it broke the rules.
#[derive(Debug, Default)]
pub struct ReplayReport {
    pub events: usize,
    /// Every request the reactor made of an app, by kind.
    pub requests: usize,
    pub request_kinds: std::collections::BTreeMap<String, usize>,
    /// Windows tracked and tiled/floating when the replay ended.
    pub final_windows: Vec<String>,
    pub writes: Vec<ReplayWrite>,
    /// Invariant violations, each naming the trace line it happened at.
    pub violations: Vec<String>,
    /// Questions the recording had no answer for. A replay with misses did
    /// not reproduce the recorded session; its verdict is not to be trusted.
    pub misses: Vec<String>,
    /// Questions answered by the next recorded answer of the same kind
    /// (float drift in a key). Informational.
    pub drifts: Vec<String>,
    /// The frames written live, from the recording.
    #[cfg(test)]
    pub live_writes: Vec<trace::OutLine>,
    /// Every change of a window's place (floating / tiled on which spaces),
    /// with the trace line that caused it.
    pub state_changes: Vec<String>,
    /// Where the replay stopped reproducing the recording: the first frame
    /// write the replay made that live did not, or live made that the replay
    /// did not. The recording is open-loop — the apps' and the window
    /// server's reactions in it are to live's writes — so from here on the
    /// recorded answers describe a run that no longer exists.
    pub diverged: Option<String>,
    /// Violations after the divergence. Reported, since they may still be
    /// telling, but not a verdict: the inputs that produced them are not
    /// ones this rift would have been given.
    pub after_divergence: Vec<String>,
}

impl ReplayReport {
    #[cfg(test)]
    pub fn is_clean(&self) -> bool {
        // Once the replay has diverged from the recording, the code under
        // test is off the recorded script; questions the recording never
        // heard (a new query added since the fixture was recorded) are
        // expected there, and are reported rather than failed — the same
        // treatment post-divergence violations get.
        self.violations.is_empty() && (self.misses.is_empty() || self.diverged.is_some())
    }

    fn violation(&mut self, text: String) {
        if self.diverged.is_some() {
            self.after_divergence.push(text);
        } else {
            self.violations.push(text);
        }
    }
}

/// Matches the replay's frame writes against the recording's, to find where
/// the two runs part.
struct LiveWrites {
    writes: Vec<(trace::OutLine, bool)>,
    /// The frame the replay last wrote to each window.
    last_replay: std::collections::HashMap<WindowId, CGRect>,
    /// The frame each window was last reported at (the recording's window
    /// store to begin with, then every frame report).
    reported: std::collections::HashMap<WindowId, CGRect>,
}

impl LiveWrites {
    /// How far apart a replay write and its live counterpart may be: the
    /// replay writes at the event's time, live when the app thread got to it.
    const SLACK_MS: u64 = 500;

    fn new(writes: &[trace::OutLine], reactor: &Reactor) -> Self {
        Self {
            writes: writes.iter().map(|write| (write.clone(), false)).collect(),
            last_replay: std::collections::HashMap::new(),
            reported: reactor
                .state
                .windows
                .iter_windows()
                .map(|(wid, state)| (wid, state.frame_monotonic))
                .collect(),
        }
    }

    /// A replay write: the live write it reproduces is matched off, or the
    /// runs have parted.
    fn on_replay_write(&mut self, write: &ReplayWrite, report: &mut ReplayReport) {
        let position_only = write.frame.size.width < 0.0;
        if !position_only {
            self.last_replay.insert(write.window, write.frame);
        }
        if report.diverged.is_some() {
            return;
        }
        // A write that puts a window where it already is changes nothing
        // for the app or the window server; live not making it (the replay
        // lays out at its synthetic launch, live had long since) does not
        // part the runs.
        if self
            .reported
            .get(&write.window)
            .is_some_and(|reported| reported.same_as(write.frame))
        {
            return;
        }
        let matches = |live: &trace::OutLine| {
            live.pid == write.window.pid
                && live.idx == write.window.idx.get()
                && live.ms.abs_diff(write.ms) <= Self::SLACK_MS
                && (live.frame.0 - write.frame.origin.x).abs() <= 1.0
                && (live.frame.1 - write.frame.origin.y).abs() <= 1.0
                && (position_only
                    || ((live.frame.2 - write.frame.size.width).abs() <= 1.0
                        && (live.frame.3 - write.frame.size.height).abs() <= 1.0))
        };
        // The nearest in time, not the first: a recording of a bounce has
        // the same frame written every few hundred milliseconds.
        if let Some((_, matched)) = self
            .writes
            .iter_mut()
            .filter(|(live, matched)| !*matched && matches(live))
            .min_by_key(|(live, _)| live.ms.abs_diff(write.ms))
        {
            *matched = true;
            return;
        }
        let nearest = self
            .writes
            .iter()
            .filter(|(live, matched)| {
                !*matched
                    && live.pid == write.window.pid
                    && live.idx == write.window.idx.get()
                    && live.ms.abs_diff(write.ms) <= Self::SLACK_MS
            })
            .map(|(live, _)| {
                format!(
                    "({},{} {}x{}) @{}ms",
                    live.frame.0, live.frame.1, live.frame.2, live.frame.3, live.ms
                )
            })
            .next()
            .unwrap_or_else(|| "nothing".to_string());
        report.diverged = Some(format!(
            "line {} @{}ms: replay wrote ({},{} {}x{}) to {:?}; live wrote {nearest}",
            write.event_index,
            write.ms,
            write.frame.origin.x,
            write.frame.origin.y,
            write.frame.size.width,
            write.frame.size.height,
            write.window
        ));
    }

    /// An app's acknowledgement of a write (`requested`) is the echo of
    /// live's write. One that lands a frame the replay never sent is the
    /// recording answering a run this is not.
    fn on_event(&mut self, index: usize, ms: u64, event: &Event, report: &mut ReplayReport) {
        let Event::WindowFrameChanged(wid, frame, _, requested, _) = event else {
            return;
        };
        self.reported.insert(*wid, *frame);
        if report.diverged.is_some() || !requested.0 {
            return;
        }
        let sent = self.last_replay.get(wid);
        // Origin, and a size no larger than asked for: an app may refuse
        // to grow to its slot, never to move somewhere else.
        let echoes = sent.is_some_and(|sent| {
            (sent.origin.x - frame.origin.x).abs() <= 1.0
                && (sent.origin.y - frame.origin.y).abs() <= 1.0
                && frame.size.width <= sent.size.width + 1.0
                && frame.size.height <= sent.size.height + 1.0
        });
        if !echoes {
            report.diverged = Some(format!(
                "line {index} @{ms}ms: {wid:?} acknowledged a write of ({},{} {}x{}); the replay last wrote {}",
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
                sent.map_or("nothing".to_string(), |sent| format!(
                    "({},{} {}x{})",
                    sent.origin.x, sent.origin.y, sent.size.width, sent.size.height
                ))
            ));
        }
    }

    /// Time has passed: a live write the replay has not reproduced by now
    /// is one it is not going to.
    fn at(&mut self, ms: u64, index: usize, report: &mut ReplayReport) {
        if report.diverged.is_some() {
            return;
        }
        if let Some((live, _)) = self
            .writes
            .iter()
            .find(|(live, matched)| !*matched && live.ms + Self::SLACK_MS < ms)
        {
            report.diverged = Some(format!(
                "line {index} @{ms}ms: live wrote ({},{} {}x{}) to {}:{} @{}ms and the replay did not",
                live.frame.0, live.frame.1, live.frame.2, live.frame.3, live.pid, live.idx, live.ms
            ));
        }
    }
}

/// Replays a trace and checks the invariants every window manager must keep
/// whatever the apps and the window server do:
///
/// 1. No frame is written to a window while the user is dragging it — from
///    the moment the button goes down, not from the app's first report of
///    the drag, which can trail the window server's hand-off of the window
///    to the other display. Judged when the button comes up, once the
///    windows the user was moving are known.
/// 2. No empty (0x0) frame is ever written.
/// 3. A window is in at most one place: one tree, or floating — never both,
///    never two trees.
/// 4. A window is not bounced between displays: two writes of the same
///    window to different displays within 1.5 seconds, with no button
///    release and no command in between, is rift moving it on its own.
/// 5. A floating window is written a frame only on a command's behalf.
#[cfg(test)]
pub fn replay_trace(path: &Path) -> anyhow::Result<ReplayReport> {
    replay_trace_with(path, |_, _| {})
}

fn replay_trace_with(
    path: &Path,
    mut on_request: impl FnMut(Span, Request),
) -> anyhow::Result<ReplayReport> {
    let (tx, mut rx) = actor::channel();
    let (config, layout, windows, lines, transactions) =
        read_trace_with_handle(path, AppThreadHandle::new_for_test(tx))?;
    let answers: Vec<SysLine> = lines
        .iter()
        .filter_map(|line| match line {
            TraceLine::Sys(sys) => Some(sys.clone()),
            _ => None,
        })
        .collect();
    // The button state is in the event stream — the tap reports drags only
    // while it is down — but the recording has answers only for the moments
    // live happened to ask. A question asked between two of those (a space
    // change 30 ms before the first frame report of a drag) would get the
    // stale earlier answer, and rift's handling of the drag would differ
    // from live's. Synthesise the state from the events instead.
    let mut answers = answers;
    let mut button_down = false;
    for line in &lines {
        let TraceLine::Event { ms, event } = line else {
            continue;
        };
        let down = match event {
            Event::MouseDragged { .. } => true,
            Event::MouseUp | Event::MouseMoved(_) => false,
            _ => continue,
        };
        if down != button_down {
            button_down = down;
            let state = if down {
                MouseState::Down
            } else {
                MouseState::Up
            };
            answers.push(SysLine {
                ms: *ms,
                kind: "mouse_state".to_string(),
                key: "null".to_string(),
                answer: (state as u8).to_string(),
            });
        }
    }
    answers.sort_by_key(|line| line.ms);
    let live_writes: Vec<trace::OutLine> = lines
        .iter()
        .filter_map(|line| match line {
            TraceLine::Out(out) => Some(out.clone()),
            _ => None,
        })
        .collect();
    trace::begin_replay(answers);
    let (broadcast_tx, _) = actor::channel();
    // As at startup: a loaded engine is finished with the live config.
    let mut layout = layout;
    layout.finish_loading(
        &config.virtual_workspaces,
        &config.settings.layout,
        Some(broadcast_tx.clone()),
    );
    let mut reactor = Reactor::new(config, layout, Record::new(None), broadcast_tx, None, false);
    if let Some(windows) = windows {
        reactor.state.windows = windows;
    }
    match transactions {
        Some(entries) => reactor.transaction_manager.store.restore(&entries),
        None => {
            // A trace from before the header carried transactions: the first
            // id each app echoes back is the one live had last sent, so
            // start each window's counter there.
            let mut seeded = std::collections::HashSet::new();
            for line in &lines {
                if let TraceLine::Event {
                    event: Event::WindowFrameChanged(wid, _, Some(seen), ..),
                    ..
                } = line
                    && seeded.insert(*wid)
                    && let Some(wsid) =
                        reactor.state.windows.window(*wid).and_then(|state| state.info.sys_id)
                {
                    reactor.transaction_manager.set_last_sent_txid(wsid, *seen);
                }
            }
        }
    }
    let mut live = LiveWrites::new(&live_writes, &reactor);
    let mut report = ReplayReport {
        #[cfg(test)]
        live_writes,
        ..ReplayReport::default()
    };
    let mut checker = Checker::default();

    for (index, line) in lines.into_iter().enumerate() {
        let TraceLine::Event { ms, event } = line else {
            continue;
        };
        trace::replay_set_now(ms);
        live.at(ms, index, &mut report);
        live.on_event(index, ms, &event, &mut report);
        checker.index = index;
        checker.before(&event, &reactor, &mut report);
        let dropping = matches!(event, Event::MouseUp).then(|| {
            format!(
                "line {index}: drop: in drag {:?}, session {:?}",
                reactor.window_in_drag(),
                reactor.get_active_drag_session().map(|s| (
                    s.window,
                    s.origin_space,
                    s.settled_space
                ))
            )
        });
        report.events += 1;
        let mut event = event;
        fill_legacy_inventory_token(&reactor, &mut event);
        // `RIFT_REPLAY_WATCH=<window idx>`: after every event, where that
        // window is — which trees hold it and which space it is assigned to.
        // For chasing a window that two trees are fighting over.
        let watched = std::env::var("RIFT_REPLAY_WATCH").ok().and_then(|s| s.parse::<u32>().ok());
        let event_name = watched.map(|_| format!("{event:?}").chars().take(90).collect::<String>());
        reactor.handle_event(event);
        if let Some(idx) = watched
            && let Some(wid) = reactor
                .state
                .windows
                .iter_windows()
                .map(|(wid, _)| wid)
                .find(|wid| wid.idx.get() == idx)
        {
            let engine = &reactor.layout_manager.layout_engine;
            let tiled: Vec<u64> = engine
                .virtual_workspace_manager()
                .initialized_spaces()
                .into_iter()
                .filter(|space| engine.is_window_tiled(*space, wid))
                .map(|space| space.get())
                .collect();
            eprintln!(
                "watch line {index} @{ms}ms {} => tiled_on={tiled:?} assigned={:?} floating={}",
                event_name.unwrap_or_default(),
                reactor.assigned_space_for_window_id(wid).map(|s| s.get()),
                engine.is_window_floating(wid)
            );
        }
        if let Some(note) = dropping {
            report.state_changes.push(note);
        }
        while let Ok((span, request)) = rx.try_recv() {
            report.requests += 1;
            let name = format!("{request:?}");
            let name = name.split(['(', ' ', '{']).next().unwrap_or("").to_string();
            *report.request_kinds.entry(name).or_default() += 1;
            for (window, frame) in frames_in_request(&request) {
                let write = ReplayWrite {
                    ms,
                    window,
                    frame,
                    event_index: index,
                };
                live.on_replay_write(&write, &mut report);
                checker.on_write(&write, &reactor, &mut report);
                report.writes.push(write);
            }
            on_request(span, request);
        }
        checker.after_event(index, &reactor, &mut report);
    }
    (report.misses, report.drifts) = trace::end_replay();
    let engine = &reactor.layout_manager.layout_engine;
    let spaces: Vec<_> = reactor.iter_active_spaces().collect();
    report.final_windows.push(format!(
        "active spaces {:?}, screens {}, workspaces {:?}",
        spaces,
        reactor.space_state.screens.len(),
        spaces
            .iter()
            .map(|space| (space.get(), engine.active_workspace(*space).is_some()))
            .collect::<Vec<_>>()
    ));
    for (wid, state) in reactor.state.windows.iter_windows() {
        let tiled: Vec<_> =
            spaces.iter().filter(|space| engine.is_window_tiled(**space, wid)).collect();
        report.final_windows.push(format!(
            "{wid:?} {:?} admitted={} floating={} tiled_on={tiled:?}",
            state.info.title.chars().take(20).collect::<String>(),
            state.is_admitted(),
            engine.is_window_floating(wid)
        ));
    }
    Ok(report)
}

fn frames_in_request(request: &Request) -> Vec<(WindowId, CGRect)> {
    match request {
        Request::SetWindowFrame(wid, frame, _, _) => vec![(*wid, *frame)],
        // A position-only write, marked by a negative size.
        Request::SetWindowPos(wid, pos, _, _) => {
            vec![(
                *wid,
                CGRect::new(*pos, objc2_core_foundation::CGSize::new(-1.0, -1.0)),
            )]
        }
        Request::SetBatchWindowFrame(frames, _, _) => frames.clone(),
        Request::AnimationFrame { wid, frame, .. } => vec![(*wid, *frame)],
        _ => Vec::new(),
    }
}

#[derive(Default)]
struct Checker {
    /// Last seen (floating, tiled-on) per window, to report changes.
    places: std::collections::HashMap<WindowId, (bool, Vec<crate::sys::screen::SpaceId>)>,
    /// The frame each window last reported itself at.
    reported: std::collections::HashMap<WindowId, CGRect>,
    /// Index of the last command event; a float may be written a new frame
    /// only on a command's behalf.
    last_command: Option<usize>,
    index: usize,
    /// The window the user is dragging: from the first frame report with the
    /// button down until the button comes up.
    dragging: Option<WindowId>,
    /// Frame writes since the button went down, judged when it comes up.
    button_down_writes: Option<Vec<ReplayWrite>>,
    /// Windows the app reported moving with the button down, since it went
    /// down.
    moved_by_user: std::collections::HashSet<WindowId>,
    /// The last write per window: (ms, index of the display the frame's
    /// centre landed on, epoch).
    last_write: std::collections::HashMap<WindowId, (u64, usize, usize)>,
    /// Bumped by every button release and every command — the only things
    /// that may legitimately put a window on another display.
    epoch: usize,
    /// The windows a drop just released and the event index of the release.
    /// The drop itself may write the dragged float once — a seam-straddling
    /// drop is finished onto one display — and invariant 5 must not read
    /// that as rift moving a float on its own.
    recent_drop: Option<(std::collections::HashSet<WindowId>, usize)>,
}

impl Checker {
    fn before(&mut self, event: &Event, reactor: &Reactor, report: &mut ReplayReport) {
        match event {
            Event::WindowFrameChanged(wid, frame, _, _, mouse) => {
                self.reported.insert(*wid, *frame);
                if *mouse == Some(MouseState::Down) {
                    self.dragging = Some(*wid);
                    self.moved_by_user.insert(*wid);
                    self.button_down_writes.get_or_insert_with(Vec::new);
                }
            }
            Event::MouseDragged { .. } => {
                self.button_down_writes.get_or_insert_with(Vec::new);
            }
            Event::MouseUp | Event::MouseMoved(_) => self.button_released(reactor, report),
            Event::Command(_) => {
                self.last_command = Some(self.index);
                self.epoch += 1;
            }
            _ => {}
        }
    }

    /// 1. Which window the user was moving is known for certain only once
    ///    the button is up: the app's first report of the drag can trail
    ///    the window server's hand-off of the window to the other display
    ///    by tens of milliseconds, and a write in that gap is the one that
    ///    yanks the window out from under the cursor.
    fn button_released(&mut self, reactor: &Reactor, report: &mut ReplayReport) {
        self.dragging = None;
        let Some(writes) = self.button_down_writes.take() else {
            return;
        };
        self.epoch += 1;
        let mut moved = std::mem::take(&mut self.moved_by_user);
        moved.extend(reactor.window_in_drag());
        self.recent_drop = Some((moved.clone(), self.index));
        for write in writes {
            if moved.contains(&write.window) {
                report.violation(format!(
                    "line {}: wrote {:?} to {:?} while the button was down on it (the app reported the drag only later)",
                    write.event_index, write.frame, write.window
                ));
            }
        }
    }

    fn on_write(&mut self, write: &ReplayWrite, reactor: &Reactor, report: &mut ReplayReport) {
        if self.dragging == Some(write.window) {
            report.violation(format!(
                "line {}: wrote {:?} to {:?} while the user was dragging it (reactor drag session: {:?})",
                write.event_index,
                write.frame,
                write.window,
                reactor.window_in_drag()
            ));
        } else if let Some(writes) = &mut self.button_down_writes {
            writes.push(write.clone());
        }
        if write.frame.size.width == 0.0 || write.frame.size.height == 0.0 {
            report.violation(format!(
                "line {}: wrote an empty frame {:?} to {:?}",
                write.event_index, write.frame, write.window
            ));
        }
        if write.frame.size.width < 0.0 {
            return;
        }
        // 5. A float is where the app says it is. Rift writes it a frame only
        //    on a command's behalf (toggle, placement, resize). Anything else
        //    is rift moving a floating window on its own — a remembered frame,
        //    a centring — which is what "my float jumped" always was.
        let floating = reactor.layout_manager.layout_engine.is_window_floating(write.window);
        let commanded = self.last_command.is_some_and(|at| write.event_index <= at + 2);
        let dropped =
            self.recent_drop.as_ref().is_some_and(|(set, at)| {
                set.contains(&write.window) && write.event_index <= at + 2
            }) || reactor.drag_manager.seam_finish.is_some_and(|finish| {
                finish.window == write.window
                    && finish.fitted.unwrap_or(finish.dropped_at).same_as(write.frame)
            });
        if floating
            && !commanded
            && !dropped
            && self
                .reported
                .get(&write.window)
                .is_some_and(|reported| !reported.same_as(write.frame))
        {
            report.violation(format!(
                "line {}: moved floating window {:?} to {:?} without a command (it reported itself at {:?})",
                write.event_index,
                write.window,
                write.frame,
                self.reported.get(&write.window)
            ));
        }
        // 4. Which display the window server has the window on is no
        //    defence: the server's report lags rift's own writes, and
        //    following it is exactly how the bounce happens.
        let centre = write.frame.mid();
        let Some(display) = reactor
            .space_state
            .screens
            .iter()
            .position(|screen| screen.frame.contains(centre))
        else {
            return;
        };
        if let Some(&(ms, previous, epoch)) = self.last_write.get(&write.window)
            && epoch == self.epoch
            && previous != display
            && write.ms.saturating_sub(ms) <= 1500
        {
            report.violation(format!(
                "line {}: moved {:?} from display {previous} (written @{ms}ms) to display {display} (@{}ms) with no button release or command in between",
                write.event_index, write.window, write.ms
            ));
        }
        self.last_write.insert(write.window, (write.ms, display, self.epoch));
    }

    fn after_event(&mut self, index: usize, reactor: &Reactor, report: &mut ReplayReport) {
        self.index = index;
        let engine = &reactor.layout_manager.layout_engine;
        let spaces: Vec<_> = reactor.iter_active_spaces().collect();
        for (wid, _) in reactor.state.windows.iter_windows() {
            let tiled_in: Vec<_> = spaces
                .iter()
                .copied()
                .filter(|space| engine.is_window_tiled(*space, wid))
                .collect();
            let floating = engine.is_window_floating(wid);
            let place = (floating, tiled_in.clone());
            if self.places.get(&wid) != Some(&place) {
                report.state_changes.push(format!(
                    "line {index}: {wid:?} floating={floating} tiled_on={tiled_in:?}"
                ));
                self.places.insert(wid, place);
            }
            if tiled_in.len() > 1 {
                report.violation(format!(
                    "line {index}: {wid:?} is tiled on two spaces at once: {tiled_in:?}"
                ));
            }
            if floating && !tiled_in.is_empty() {
                report.violation(format!(
                    "line {index}: {wid:?} is floating and tiled on {tiled_in:?} at once"
                ));
            }
        }
    }
}

/// Every trace under `tests/traces` must replay cleanly.
#[cfg(test)]
mod trace_tests {
    use super::*;

    #[test]
    fn recorded_traces_replay_cleanly() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/traces");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut failures = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("trace") {
                continue;
            }
            let report = replay_trace(&path).expect("trace replays");
            eprintln!(
                "{}: {} events, {} requests {:?}, {} frame writes, {} violations ({} after divergence), {} unanswered, {} drifted\n  final: {}",
                path.file_name().unwrap().to_string_lossy(),
                report.events,
                report.requests,
                report.request_kinds,
                report.writes.len(),
                report.violations.len(),
                report.after_divergence.len(),
                report.misses.len(),
                report.drifts.len(),
                report.final_windows.join("\n         ")
            );
            for write in &report.writes {
                eprintln!(
                    "  replay write @{}ms line {}: {:?} <- ({},{} {}x{})",
                    write.ms,
                    write.event_index,
                    write.window,
                    write.frame.origin.x,
                    write.frame.origin.y,
                    write.frame.size.width,
                    write.frame.size.height
                );
            }
            for change in &report.state_changes {
                eprintln!("  {change}");
            }
            if let Some(diverged) = &report.diverged {
                eprintln!("  diverged from the recording: {diverged}");
                for violation in &report.after_divergence {
                    eprintln!("  after divergence: {violation}");
                }
            }
            for write in &report.live_writes {
                eprintln!(
                    "  live   write @{}ms: {}:{} <- ({},{} {}x{})",
                    write.ms,
                    write.pid,
                    write.idx,
                    write.frame.0,
                    write.frame.1,
                    write.frame.2,
                    write.frame.3
                );
            }
            if !report.is_clean() {
                failures.push(format!(
                    "{}:\n  violations:\n    {}\n  unanswered questions:\n    {}",
                    path.display(),
                    report.violations.join("\n    "),
                    report.misses.join("\n    ")
                ));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }
}
