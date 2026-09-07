//! Where everything was when a display departed, reconciled against where
//! everything is once the display is back. This is what `displaced_windows =
//! "spaces"` does; the other modes are in `display_archive`.
//!
//! The window server reshuffles desktops and windows on its own at both
//! ends of an unplug, and not in one consistent way: on unplug it destroys
//! the surviving display's first desktop and merges its windows into the
//! first desktop of the ones it carries over; on replug it mints a fresh
//! desktop for the survivor, sometimes puts the merged windows back on it
//! itself, sends every desktop it filed behind the visitors along with them,
//! and can dump a kept desktop's windows elsewhere while handing that desktop
//! back. Reacting to each of those as it is noticed is a losing game: every
//! sequence not yet seen hits a case not yet written.
//!
//! So one record is taken at departure — every window's desktop, every
//! display's desktops in order and the one it shows, and every desktop's
//! tree — and everything after that is a diff against it. Twice:
//!
//! 1. Right after departure, once the window server is done, the survivor
//!    is settled: the windows of the desktop macOS destroyed get a desktop
//!    of their own, made for the purpose, with their tree; the survivor's
//!    own desktops go first; and the survivor is switched back to the
//!    desktop it was showing. The record notes the desktop it made as its
//!    own, so nothing later mistakes it for the user's. A later departure
//!    while the record stands — another display that came and went — costs
//!    the survivor its first desktop all over again, and settles it again
//!    the same way.
//! 2. When every display of the record is back and the topology is quiet,
//!    the record is diffed against what the window server reports and the
//!    differences are put right, once: destroyed desktops are paired with
//!    the fresh ones minted for the return, strayed desktops are sent back,
//!    strayed windows are sent back, each desktop's tree is restored by
//!    name, and a desktop made at departure whose windows have somewhere to
//!    go is destroyed. Why a window is somewhere else does not matter.
//!
//! In between, only what the user does edits the record: a window they
//! place is filed where they put it, and a desktop they destroy — one rift
//! made, or one of their own — is forgotten along with the windows filed
//! on it, which are filed again wherever they next turn up. The record is
//! about the displays that were there when it was taken: one that arrives
//! and leaves while it stands is neither recorded nor waited for, or a
//! display that never comes back — a monitor at another desk — would hold
//! everything else up for good.

use std::time::{Duration, Instant};

use objc2_core_foundation::CGSize;
use tracing::{debug, info, warn};

use super::{LayoutEvent, Reactor};
use crate::actor::app::WindowId;
use crate::actor::reactor::events::EventOutcome;
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::LayoutMode;
use crate::layout_engine::{RestoreRequest, RestoreScope, RestoreSource};
use crate::sys::screen::{ScreenInfo, SpaceId};
use crate::sys::scripting_addition;
use crate::sys::window_server::WindowServerId;

/// The deadline timer is shared with the archive's; this key tells the
/// record's apart from a display uuid.
pub(super) const RECORD_DEADLINE_KEY: &str = "record";

/// How long after a reshuffle a window's appearance on another desktop is
/// still the window server's doing and not the user's.
const CHURN_SETTLE: Duration = Duration::from_secs(10);

/// How long a made desktop that a display keeps showing is retried before
/// it is left alone.
const RETIRE_GIVE_UP: Duration = Duration::from_secs(30);

pub(super) struct DisplayRecord {
    taken: Instant,
    /// Every desktop's tree at departure, keyed by the desktop ids of then.
    layout: String,
    /// Every desktop's windows at departure, in layout order: what tells
    /// a desktop the user rearranged while away from one they did not.
    members: HashMap<SpaceId, Vec<WindowId>>,
    /// Every desktop's workspaces' layout modes at departure: a mode
    /// switched while away is as much a rearrangement as a reordering, and
    /// leaves the order alone.
    modes: HashMap<SpaceId, Vec<LayoutMode>>,
    /// Where every window belongs.
    windows: HashMap<WindowId, SpaceId>,
    /// Where the user put a window while a display was away, and when it
    /// was seen there. The window server's reshuffles are seen only after
    /// the windows have moved, so a placement younger than `CHURN_SETTLE`
    /// when one is seen was its doing, and is dropped then.
    placed: HashMap<WindowId, (SpaceId, Instant)>,
    displays: Vec<RecordedDisplay>,
    /// The display that stayed: its desktops go ahead of the visitors, and
    /// whatever the user makes while the others are away is its.
    survivor: String,
    /// Every desktop the window server has listed since the record was
    /// taken. One it lists at the return that is not here was minted for
    /// the return, and stands in for a destroyed one; one that is here came
    /// along with some display in the meantime and stands in for nothing.
    seen: HashSet<SpaceId>,
    /// When the window server was last seen reshuffling: the record's
    /// taking, and every settle since.
    churn_seen: Instant,
    settled: bool,
    /// Desktops rift made at departure for the windows of a destroyed one:
    /// `(made, destroyed)`. Not the user's, and not to outlive the return
    /// when the return gives their windows somewhere to go.
    stopgaps: Vec<(SpaceId, SpaceId)>,
    /// Windows rift itself has sent somewhere; their arrival is not the
    /// user's doing.
    own_moves: HashSet<WindowId>,
    pass: Option<Pass>,
}

#[derive(Clone, Debug)]
pub(super) struct RecordedDisplay {
    pub(super) uuid: String,
    /// The display's desktops in order.
    pub(super) desktops: Vec<SpaceId>,
    pub(super) shown: Option<SpaceId>,
}

/// The layout from the last departure, kept after the return: a desktop
/// the user rearranged while away keeps the rearrangement, and this is
/// what puts it back on demand.
pub(super) struct DepartureSnapshot {
    layout: String,
    /// Each desktop's id now → its id in the layout.
    then: HashMap<SpaceId, SpaceId>,
}

/// A pass in flight: windows sent somewhere, trees to restore once they
/// have arrived or the deadline has passed.
struct Pass {
    stage: Stage,
    /// Each desktop's id now → its id in the record's layout.
    ids: HashMap<SpaceId, SpaceId>,
    /// Windows sent (or found already sent) to a desktop and not yet
    /// assigned there by rift.
    waiting: HashMap<WindowId, SpaceId>,
    /// (desktop id in the record, desktop id now) for every desktop whose
    /// tree is put back.
    restores: Vec<(SpaceId, SpaceId)>,
    /// Made desktops whose windows the pass sends elsewhere: destroyed
    /// once the pass is over and nothing shows them.
    retire: Vec<SpaceId>,
    started: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Settling the survivor after departure; the record lives on.
    Away,
    /// Putting everything back after return; the record is done.
    Back,
}

impl DisplayRecord {
    /// The desktop `wid` has been sent to and not yet assigned on, if it is
    /// one of the windows a pass waits for.
    pub(super) fn destination(&self, wid: WindowId) -> Option<SpaceId> {
        self.pass.as_ref().and_then(|pass| pass.waiting.get(&wid).copied())
    }

    /// Whether no pass is in flight, so a settle can run.
    pub(super) fn destination_free(&self) -> bool { self.pass.is_none() }

    /// Where the record wants `wid`: where the user put it while away, else
    /// where it was at departure.
    fn desired(&self, wid: WindowId) -> Option<SpaceId> {
        self.placed
            .get(&wid)
            .map(|(space, _)| *space)
            .or_else(|| self.windows.get(&wid).copied())
    }

    /// The desktop made to stand in for `lost`, if one was.
    fn stopgap_for(&self, lost: SpaceId) -> Option<SpaceId> {
        self.stopgaps.iter().find(|(_, l)| *l == lost).map(|(made, _)| *made)
    }

    /// Every window the record wants on `space`.
    fn windows_desired_on(&self, space: SpaceId) -> Vec<WindowId> {
        let mut wids: Vec<WindowId> = self
            .windows
            .keys()
            .chain(self.placed.keys())
            .copied()
            .filter(|wid| self.desired(*wid) == Some(space))
            .collect();
        wids.sort_unstable();
        wids.dedup();
        wids
    }

    #[cfg(test)]
    pub(super) fn backdate(&mut self, by: Duration) {
        self.taken -= by;
        self.churn_seen -= by;
    }

    /// A desktop the window server listed after the record was taken and
    /// lists no longer, with the displays as they were: the user destroyed
    /// it (Mission Control, `destroy_space`), or macOS did outside any
    /// reshuffle, and either way nothing brings it back. The record forgets
    /// it — a made desktop together with the one it stood in for, so no
    /// later settle makes another — and every window filed on it: the
    /// window server has put those somewhere, and where each next turns up
    /// is where it belongs (`note_window_placed_while_away`). Returns the
    /// desktops forgotten and the windows let go.
    pub(super) fn forget_destroyed_desktop(
        &mut self,
        space: SpaceId,
    ) -> (Vec<SpaceId>, Vec<WindowId>) {
        let mut forgotten = vec![space];
        if let Some(at) = self.stopgaps.iter().position(|(made, _)| *made == space) {
            let (_, lost) = self.stopgaps.remove(at);
            forgotten.push(lost);
        }
        // One the record never heard of, with nothing filed on it — made
        // and unmade by the user — is no business of the record's.
        let known = forgotten.len() > 1
            || self.displays.iter().any(|d| d.desktops.contains(&space))
            || self.windows.values().any(|s| *s == space)
            || self.placed.values().any(|(s, _)| *s == space);
        if !known {
            return (Vec::new(), Vec::new());
        }
        for display in &mut self.displays {
            display.desktops.retain(|s| !forgotten.contains(s));
            if display.shown.is_some_and(|s| forgotten.contains(&s)) {
                display.shown = None;
            }
        }
        let mut windows: Vec<WindowId> = Vec::new();
        self.windows.retain(|wid, s| {
            let keep = !forgotten.contains(s);
            if !keep {
                windows.push(*wid);
            }
            keep
        });
        self.placed.retain(|wid, (s, _)| {
            let keep = !forgotten.contains(s);
            if !keep && !windows.contains(wid) {
                windows.push(*wid);
            }
            keep
        });
        // Their next arrival is the user's placement, not the tail of a
        // move of rift's.
        for wid in &windows {
            self.own_moves.remove(wid);
        }
        (forgotten, windows)
    }

    #[cfg(test)]
    pub(super) fn recorded_desktop(&self, wid: WindowId) -> Option<SpaceId> { self.desired(wid) }

    #[cfg(test)]
    pub(super) fn display_uuids(&self) -> Vec<&str> {
        self.displays.iter().map(|d| d.uuid.as_str()).collect()
    }

    #[cfg(test)]
    pub(super) fn made_desktops(&self) -> Vec<SpaceId> {
        self.stopgaps.iter().map(|(made, _)| *made).collect()
    }
}

impl Reactor {
    /// Takes the record, from the pre-churn snapshot when there is one: the
    /// window server starts moving windows before it reports the display
    /// change, and the snapshot is from before that. The displays are the
    /// last whole set the window server reported, for the same reason.
    /// A departure while a record already stands changes nothing: the
    /// state from before the first departure is the one to go back to, and
    /// the record waits for its own displays, whichever else come and go.
    pub(super) fn record_departure(&mut self, departed: Vec<String>, active_displays: &[String]) {
        if self.display_archive.record.is_some() {
            debug!(
                ?departed,
                "A display departed while a record stands; the record stands"
            );
            return;
        }
        let displays: Vec<RecordedDisplay> = match self.display_archive.whole_displays.as_ref() {
            Some(whole) => whole.clone(),
            // The display set from before the change: the reactor's own
            // state is only updated after this runs.
            None => self
                .space_state
                .display_space_ids
                .iter()
                .map(|(uuid, desktops)| RecordedDisplay {
                    uuid: uuid.clone(),
                    desktops: desktops.clone(),
                    shown: self
                        .space_state
                        .screens
                        .iter()
                        .find(|screen| &screen.display_uuid == uuid)
                        .and_then(|screen| screen.space),
                })
                .collect(),
        };
        let departed: Vec<String> = departed
            .into_iter()
            .filter(|uuid| displays.iter().any(|d| &d.uuid == uuid))
            .collect();
        if departed.is_empty() {
            debug!("The departed display was not in the last whole display set; nothing to record");
            return;
        }
        let Some(survivor) = displays
            .iter()
            .find(|d| active_displays.contains(&d.uuid) && !departed.contains(&d.uuid))
            .map(|d| d.uuid.clone())
        else {
            debug!(
                ?departed,
                "No display of the last whole set stayed; nothing to record"
            );
            return;
        };
        let (layout, members, modes) = match self.display_archive.fresh_pre_churn() {
            Some(pre) => (pre.layout.clone(), pre.members.clone(), pre.modes.clone()),
            None => {
                let engine = &mut self.layout_manager.layout_engine;
                let spaces = engine.virtual_workspace_manager().initialized_spaces();
                let members: HashMap<SpaceId, Vec<WindowId>> = spaces
                    .iter()
                    .map(|space| (*space, engine.windows_on_space_in_layout_order(*space)))
                    .collect();
                let modes: HashMap<SpaceId, Vec<LayoutMode>> = spaces
                    .iter()
                    .map(|space| (*space, engine.layout_modes_on_space(*space)))
                    .collect();
                match engine.snapshot_current_layout_lightly(&self.state.windows) {
                    Ok(layout) => (layout, members, modes),
                    Err(error) => {
                        warn!(%error, "Could not record the layout at departure");
                        return;
                    }
                }
            }
        };
        let mut windows = HashMap::default();
        for (space, wids) in &members {
            for wid in wids {
                if self.state.windows.window(*wid).is_some() {
                    windows.insert(*wid, *space);
                }
            }
        }
        let seen: HashSet<SpaceId> =
            displays.iter().flat_map(|d| d.desktops.iter().copied()).collect();
        info!(
            ?departed,
            %survivor,
            windows = windows.len(),
            displays = ?displays.iter().map(|d| (d.uuid.as_str(), &d.desktops, d.shown)).collect::<Vec<_>>(),
            "Display departed; recorded where everything was, to be put back when it returns"
        );
        crate::sys::trace::act("record", &(windows.len(), displays.len()));
        let now = crate::sys::trace::now();
        self.display_archive.record = Some(DisplayRecord {
            taken: now,
            layout,
            members,
            modes,
            windows,
            placed: HashMap::default(),
            displays,
            survivor,
            seen,
            churn_seen: now,
            settled: false,
            stopgaps: Vec::new(),
            own_moves: HashSet::default(),
            pass: None,
        });
    }

    /// Right after a departure: the survivor gets its own state back as far
    /// as it can while a display is away. The windows of the desktop macOS
    /// destroyed get a desktop made for them, with their tree; the
    /// survivor's own desktops are put ahead of the visitors; and the
    /// survivor is switched back to the desktop it was showing. Run again
    /// at every later departure, for the desktop that one destroys — a
    /// made one included: its windows get another — and after a wake, in
    /// case the Mac went to sleep before Dock had carried out the previous
    /// settle: whatever it did not do is done again.
    pub(super) fn settle_after_departure(&mut self, screens: &[ScreenInfo]) -> EventOutcome {
        let outcome = EventOutcome::default();
        let Some(record) = self.display_archive.record.as_mut() else {
            return outcome;
        };
        if record.pass.is_some() {
            return outcome;
        }
        // Whatever turned up somewhere else just before this reshuffle was
        // seen was the window server's doing, not the user's.
        record.placed.retain(|_, (_, at)| at.elapsed() >= CHURN_SETTLE);
        let record = self.display_archive.record.as_ref().expect("checked above");
        let on_screen: Vec<&str> = screens.iter().map(|s| s.display_uuid.as_str()).collect();
        let Some(survivor) = record
            .displays
            .iter()
            .find(|d| d.uuid == record.survivor && on_screen.contains(&d.uuid.as_str()))
            .or_else(|| record.displays.iter().find(|d| on_screen.contains(&d.uuid.as_str())))
        else {
            return outcome;
        };
        let addition = scripting_addition::is_available();
        let mut now = self.display_space_ids_now();
        let listed_all: HashSet<SpaceId> = now.values().flatten().copied().collect();
        let on_survivor: Vec<SpaceId> = now.get(&survivor.uuid).cloned().unwrap_or_default();
        // The survivor's own desktops, by the id each has now: a destroyed
        // one is represented by the desktop made for it.
        let own_now: Vec<(SpaceId, SpaceId)> = survivor
            .desktops
            .iter()
            .map(|then| (record.stopgap_for(*then).unwrap_or(*then), *then))
            .collect();
        let destroyed: Vec<(SpaceId, SpaceId)> =
            own_now.iter().copied().filter(|(now, _)| !listed_all.contains(now)).collect();
        let kept: Vec<SpaceId> = own_now
            .iter()
            .map(|(now, _)| *now)
            .filter(|now| on_survivor.contains(now))
            .collect();
        let visitors: Vec<SpaceId> = record
            .displays
            .iter()
            .filter(|d| d.uuid != survivor.uuid)
            .flat_map(|d| d.desktops.iter().copied())
            .filter(|s| on_survivor.contains(s))
            .collect();
        // Windows a previous settle sent to a made desktop that never got
        // there, by the window server's word: sent again below.
        let astray: Vec<(WindowId, WindowServerId)> = record
            .stopgaps
            .iter()
            .filter(|(made, _)| listed_all.contains(made))
            .flat_map(|(made, lost)| {
                record.windows_desired_on(*lost).into_iter().map(move |wid| (wid, *made))
            })
            .filter_map(|(wid, made)| {
                let state = self.state.windows.window(wid)?;
                let wsid = state.info.sys_id?;
                let actual = crate::sys::window_server::window_space(wsid)
                    .or_else(|| self.assigned_space_for_window_id(wid));
                (actual != Some(made)).then_some((wid, wsid))
            })
            .collect();
        // A later departure that destroyed nothing of the survivor's is not
        // a reason to reorder its desktops or switch what it shows.
        if record.settled && destroyed.is_empty() && astray.is_empty() {
            let record = self.display_archive.record.as_mut().expect("checked above");
            record.seen.extend(listed_all);
            return outcome;
        }
        // Only a desktop lost or made is a reason to reorder the survivor's
        // desktops and switch what it shows; windows sent again are not.
        let reshuffled = !record.settled || !destroyed.is_empty();
        let survivor = survivor.clone();

        // A desktop for the destroyed desktop's windows. macOS lists the
        // visitors first; the new one goes after the last of them, and the
        // walk below puts it first.
        let mut stopgap: Option<(SpaceId, SpaceId, SpaceId)> = None;
        if let Some((gone, lost)) = destroyed.first().copied() {
            if destroyed.len() > 1 {
                warn!(
                    ?destroyed,
                    "Several of the survivor's desktops were destroyed; only the first gets a desktop of its own meanwhile"
                );
            }
            let anchor = visitors.last().or(on_survivor.last()).copied();
            match anchor.filter(|_| addition).and_then(scripting_addition::create_space_after) {
                Some(made) => {
                    info!(display = %survivor.uuid, lost = lost.get(), gone = gone.get(), made = made.get(), "Made the survivor a desktop for the windows of the one macOS destroyed");
                    now.entry(survivor.uuid.clone()).or_default().push(made);
                    stopgap = Some((made, gone, lost));
                }
                None => {
                    warn!(display = %survivor.uuid, lost = lost.get(), scripting_addition = addition, "Could not make the survivor a desktop; its windows stay merged in among the visitors until the other display is back")
                }
            }
        }

        // The survivor's own desktops first — the made one, then the kept
        // ones — and the visitors behind them, each moved behind the
        // previous only when it is not there already.
        let mut own: Vec<SpaceId> = stopgap.iter().map(|(made, _, _)| *made).collect();
        own.extend(kept.iter().copied());
        let desired: Vec<SpaceId> = own.iter().chain(visitors.iter()).copied().collect();
        let mut desktop_moves = 0usize;
        if addition && reshuffled && !own.is_empty() {
            let mut order: Vec<SpaceId> = now.get(&survivor.uuid).cloned().unwrap_or_default();
            let mut anchor: Option<SpaceId> = None;
            for space in desired {
                let Some(at) = order.iter().position(|s| *s == space) else {
                    continue;
                };
                let Some(after) = anchor else {
                    // The first desired desktop goes behind nothing; instead
                    // everything else is walked behind it.
                    anchor = Some(space);
                    continue;
                };
                let wanted = order.iter().position(|s| *s == after).map(|i| i + 1);
                if wanted == Some(at) {
                    anchor = Some(space);
                    continue;
                }
                if scripting_addition::move_space_after_space(space.get(), after.get(), false) {
                    desktop_moves += 1;
                    order.retain(|s| *s != space);
                    let to = order
                        .iter()
                        .position(|s| *s == after)
                        .map(|i| i + 1)
                        .unwrap_or(order.len());
                    order.insert(to, space);
                    anchor = Some(space);
                } else {
                    warn!(desktop = space.get(), "Could not reorder a desktop");
                }
            }
        }

        // The destroyed desktop's windows onto the made one, with its tree:
        // the tree is remapped now, and put back from the record once the
        // windows have arrived.
        let mut waiting: HashMap<WindowId, SpaceId> = HashMap::default();
        let mut restores = Vec::new();
        let mut sent: Vec<(WindowId, WindowServerId)> = Vec::new();
        if let Some((made, _, lost)) = stopgap {
            let merged: Vec<(WindowId, Option<WindowServerId>)> = record
                .windows
                .iter()
                .filter(|(wid, desktop)| {
                    record.desired(**wid) == Some(**desktop) && **desktop == lost
                })
                .filter_map(|(wid, _)| {
                    self.state.windows.window(*wid).map(|state| (*wid, state.info.sys_id))
                })
                .collect();
            for (wid, wsid) in merged {
                let Some(wsid) = wsid else {
                    continue;
                };
                if scripting_addition::move_window_to_space(wsid.as_u32(), made.get()) {
                    sent.push((wid, wsid));
                    waiting.insert(wid, made);
                } else {
                    warn!(?wid, "Could not move a window to the survivor's made desktop");
                }
            }
            restores.push((lost, made));
        }
        for (wid, wsid) in &astray {
            let Some(made) = record
                .desired(*wid)
                .and_then(|lost| record.stopgap_for(lost))
                .filter(|made| listed_all.contains(made))
            else {
                continue;
            };
            if scripting_addition::move_window_to_space(wsid.as_u32(), made.get()) {
                sent.push((*wid, *wsid));
                waiting.insert(*wid, made);
                if !restores.contains(&(record.desired(*wid).expect("checked"), made)) {
                    restores.push((record.desired(*wid).expect("checked"), made));
                }
            } else {
                warn!(
                    ?wid,
                    "Could not move a window to the survivor's made desktop again"
                );
            }
        }

        // Back to the desktop it was showing; the made one stands in for
        // the destroyed one.
        let stands_in = |s: SpaceId| match stopgap {
            Some((made, _, lost)) if s == lost => made,
            _ => record.stopgap_for(s).unwrap_or(s),
        };
        let shown = survivor.shown.map(stands_in);
        let showing = screens
            .iter()
            .find(|screen| screen.display_uuid == survivor.uuid)
            .and_then(|screen| screen.space);
        if let Some(shown) = shown
            && reshuffled
            && now.get(&survivor.uuid).is_some_and(|listed| listed.contains(&shown))
            && showing != Some(shown)
            && addition
            && !scripting_addition::focus_space(shown.get())
        {
            warn!(
                space = shown.get(),
                "Could not switch the survivor back to the desktop it was showing"
            );
        }

        info!(
            display = %survivor.uuid,
            destroyed = ?destroyed.iter().map(|(now, _)| now.get()).collect::<Vec<_>>(),
            made = ?stopgap.map(|(made, _, _)| made.get()),
            desktop_moves,
            windows_moved = sent.len(),
            sent_again = astray.len(),
            "Settled the survivor while the other display is away"
        );
        crate::sys::trace::act("settle", &(desktop_moves, sent.len()));

        if let Some((made, gone, _)) = stopgap {
            self.layout_manager
                .layout_engine
                .remap_space(&mut self.state.windows, gone, made);
            self.layout_manager
                .layout_engine
                .update_space_display(made, Some(survivor.uuid.clone()));
        }
        for (_, wsid) in &sent {
            self.note_window_sent_to_space(*wsid);
        }
        let immediate = waiting.is_empty();
        let record = self.display_archive.record.as_mut().expect("checked above");
        record.settled = true;
        record.churn_seen = crate::sys::trace::now();
        record.seen.extend(listed_all);
        if let Some((made, gone, lost)) = stopgap {
            record.stopgaps.retain(|(m, _)| *m != gone);
            record.stopgaps.push((made, lost));
            record.seen.insert(made);
        }
        record.own_moves.extend(sent.iter().map(|(wid, _)| *wid));
        if restores.is_empty() {
            return outcome;
        }
        record.pass = Some(Pass {
            stage: Stage::Away,
            ids: HashMap::default(),
            waiting,
            restores,
            retire: Vec::new(),
            started: crate::sys::trace::now(),
        });
        if immediate {
            return self.finish_pass();
        }
        self.schedule_homing_deadline(RECORD_DEADLINE_KEY.to_string());
        outcome
    }

    /// A window arriving on a user desktop once the churn has settled and
    /// while a display is still away was put there by the user, and that
    /// is where it belongs from now on. A window the record does not know
    /// — opened meanwhile — is recorded where it is, so it too is left
    /// alone.
    pub(super) fn note_window_placed_while_away(&mut self, wid: WindowId, space: SpaceId) {
        let on_screen: Vec<&str> = self
            .space_state
            .screens
            .iter()
            .map(|screen| screen.display_uuid.as_str())
            .collect();
        let Some(record) = self.display_archive.record.as_mut() else {
            return;
        };
        if record.own_moves.remove(&wid) {
            return;
        }
        if record.pass.is_some()
            || record.churn_seen.elapsed() < CHURN_SETTLE
            || record.displays.iter().all(|d| on_screen.contains(&d.uuid.as_str()))
        {
            return;
        }
        if record.windows.get(&wid) == Some(&space) {
            record.placed.remove(&wid);
            return;
        }
        if record.placed.get(&wid).is_some_and(|(there, _)| *there == space) {
            return;
        }
        record.placed.insert(wid, (space, crate::sys::trace::now()));
        info!(
            ?wid,
            space = space.get(),
            "Window placed by the user while a display is away; recorded there"
        );
        crate::sys::trace::act("record_edit", &(wid.idx.get(), space.get()));
    }

    /// Once every display of the record is back: diff the record against
    /// what the window server reports and put the differences right.
    pub(super) fn reconcile_record(&mut self) -> EventOutcome {
        let mut outcome = EventOutcome::default();
        let Some(record) = self.display_archive.record.as_ref() else {
            return outcome;
        };
        let on_screen: Vec<String> = self
            .space_state
            .screens
            .iter()
            .map(|screen| screen.display_uuid.clone())
            .collect();
        if !record.displays.iter().all(|d| on_screen.contains(&d.uuid)) {
            return outcome;
        }
        match record.pass.as_ref().map(|pass| pass.stage) {
            Some(Stage::Back) => return outcome,
            // The display came back before the settling was over; whatever
            // has landed is restored and the rest goes with the return.
            Some(Stage::Away) => outcome.absorb(self.finish_pass()),
            None => {}
        }
        let record = self.display_archive.record.as_ref().expect("kept by finish_pass");
        let now = self.display_space_ids_now();
        let listed_all: HashSet<SpaceId> = now.values().flatten().copied().collect();
        let recorded_all: HashSet<SpaceId> =
            record.displays.iter().flat_map(|d| d.desktops.iter().copied()).collect();
        let stopgaps: HashMap<SpaceId, SpaceId> = record.stopgaps.iter().copied().collect();
        let stand_in: HashMap<SpaceId, SpaceId> =
            record.stopgaps.iter().map(|(made, lost)| (*lost, *made)).collect();
        let retiring: Vec<SpaceId> =
            self.display_archive.retiring.iter().map(|(space, _)| *space).collect();

        // A destroyed desktop is one the record has that is listed nowhere
        // now; a fresh one is listed under a display now, recorded nowhere,
        // not one rift made, and not seen before the return — one that came
        // along with some other display meanwhile stands in for nothing.
        // macOS lists the replacement first, so they pair in order. Fresh
        // desktops beyond the destroyed ones were made by the user.
        let mut subst: HashMap<SpaceId, SpaceId> = HashMap::default();
        let mut paired: HashSet<SpaceId> = HashSet::default();
        for d in &record.displays {
            let destroyed: Vec<SpaceId> =
                d.desktops.iter().copied().filter(|s| !listed_all.contains(s)).collect();
            let fresh: Vec<SpaceId> = now
                .get(&d.uuid)
                .into_iter()
                .flatten()
                .copied()
                .filter(|s| {
                    !recorded_all.contains(s)
                        && !stopgaps.contains_key(s)
                        && !record.seen.contains(s)
                })
                .collect();
            for (old, new) in destroyed.into_iter().zip(fresh) {
                subst.insert(old, new);
                paired.insert(new);
            }
        }
        // A destroyed desktop is represented by its replacement, else by the
        // desktop made for it at departure, which then stays as it is.
        let map = |space: SpaceId| {
            subst
                .get(&space)
                .copied()
                .or_else(|| stand_in.get(&space).copied())
                .unwrap_or(space)
        };
        // Whatever is listed anywhere that the record does not know, no
        // destroyed desktop accounts for and rift did not make was made by
        // the user (or left behind by a display that came and went) while
        // the display was away, and belongs to the display that stayed.
        let unknown: Vec<SpaceId> = record
            .displays
            .iter()
            .flat_map(|d| now.get(&d.uuid).into_iter().flatten().copied())
            .filter(|s| {
                !recorded_all.contains(s)
                    && !paired.contains(s)
                    && !stopgaps.contains_key(s)
                    && !retiring.contains(s)
            })
            .collect();

        // A desktop the user rearranged while away keeps its arrangement:
        // its tree is not put back from the record, and what it shows now
        // becomes its tree at every screen size, so it goes back to its
        // display as the user left it. Rearranged means its windows are
        // not in the order the record has — counting only the windows the
        // record has on it and that are on it now, so nothing the churn
        // moved on or off it counts — or a workspace's layout mode is not
        // the one the record has.
        let touched: HashSet<SpaceId> = record
            .displays
            .iter()
            .flat_map(|d| d.desktops.iter().copied())
            .filter(|x| {
                // Where the desktop's tree lives now: on the desktop made
                // for it, if it was destroyed and one was.
                let here = record.stopgap_for(*x).unwrap_or(*x);
                if !listed_all.contains(&map(*x)) {
                    return false;
                }
                // Recorded on this desktop, and on it now: what the user
                // could have rearranged.
                let counts = |w: &WindowId| {
                    record.windows.get(w) == Some(x)
                        && self
                            .state
                            .windows
                            .window(*w)
                            .and_then(|state| state.info.sys_id)
                            .and_then(crate::sys::window_server::window_space)
                            .or_else(|| self.assigned_space_for_window_id(*w))
                            == Some(here)
                };
                let then: Vec<WindowId> =
                    record.members.get(x).into_iter().flatten().copied().filter(counts).collect();
                let now: Vec<WindowId> = self
                    .layout_manager
                    .layout_engine
                    .windows_on_space_in_layout_order(here)
                    .into_iter()
                    .filter(counts)
                    .collect();
                if then != now {
                    return true;
                }
                let modes_now = self.layout_manager.layout_engine.layout_modes_on_space(here);
                record
                    .modes
                    .get(x)
                    .is_some_and(|then| !modes_now.is_empty() && *then != modes_now)
            })
            .collect();

        // The destroyed desktops' trees onto the fresh ids, before any window
        // is looked at: the remap throws away whatever rift had already set
        // up on the fresh desktop, assignments included. A tree that went to
        // a made desktop at departure comes from there.
        for (old, new) in &subst {
            let from = record.stopgap_for(*old).unwrap_or(*old);
            self.layout_manager
                .layout_engine
                .remap_space(&mut self.state.windows, from, *new);
        }
        for d in &record.displays {
            for space in now.get(&d.uuid).into_iter().flatten() {
                self.layout_manager
                    .layout_engine
                    .update_space_display(*space, Some(d.uuid.clone()));
            }
        }
        for x in &touched {
            self.layout_manager.layout_engine.adopt_active_layout_for_all_sizes(map(*x));
        }

        let shown_now = |uuid: &str| -> Option<SpaceId> {
            self.space_state
                .screens
                .iter()
                .find(|screen| screen.display_uuid == uuid)
                .and_then(|screen| screen.space)
        };

        // Desktops: each display gets its recorded desktops back, in order,
        // walked so that each one lands behind the previous.
        let mut desktop_moves = 0usize;
        let addition = scripting_addition::is_available();
        // What each display lists, kept current through the moves below.
        let mut now = now;
        for d in &record.displays {
            let mut desired: Vec<SpaceId> = d.desktops.iter().map(|s| map(*s)).collect();
            if d.uuid == record.survivor {
                desired.extend(unknown.iter().copied());
            }
            let on_d: Vec<SpaceId> = now.get(&d.uuid).cloned().unwrap_or_default();
            let mut anchor: Option<SpaceId> = None;
            for space in desired {
                if !listed_all.contains(&space) {
                    continue;
                }
                if on_d.contains(&space) {
                    anchor = Some(space);
                    continue;
                }
                let Some(after) = anchor.or_else(|| shown_now(&d.uuid)) else {
                    continue;
                };
                if !addition {
                    warn!(display = %d.uuid, desktop = space.get(), "A desktop went with another display; sending it back needs the scripting addition");
                    continue;
                }
                if scripting_addition::move_space_after_space(space.get(), after.get(), false) {
                    info!(display = %d.uuid, desktop = space.get(), after = after.get(), "Sent a desktop back to the display it belongs to");
                    desktop_moves += 1;
                    anchor = Some(space);
                    for listed in now.values_mut() {
                        listed.retain(|s| *s != space);
                    }
                    now.entry(d.uuid.clone()).or_default().push(space);
                } else {
                    warn!(display = %d.uuid, desktop = space.get(), "Could not send a desktop back");
                }
            }
        }

        // Windows: every one not where the record says, by the window
        // server's word, is sent there. One the server already has there —
        // macOS put it back itself — is only waited for, until rift's own
        // assignment agrees, so the restore has something to match.
        let mut waiting: HashMap<WindowId, SpaceId> = HashMap::default();
        let mut moved = 0usize;
        let mut refused = 0usize;
        let mut sent: Vec<(WindowId, WindowServerId)> = Vec::new();
        for wid in record.windows.keys().chain(record.placed.keys()) {
            let Some(desired) = record.desired(*wid) else {
                continue;
            };
            let desired = map(desired);
            if !listed_all.contains(&desired) {
                continue;
            }
            let Some(state) = self.state.windows.window(*wid) else {
                continue;
            };
            let wsid = state.info.sys_id;
            let assigned = self.assigned_space_for_window_id(*wid);
            let actual = wsid.and_then(crate::sys::window_server::window_space).or(assigned);
            if actual == Some(desired) {
                if assigned.is_none() {
                    // Rift had it on the fresh desktop already and the
                    // remap above dropped that; nothing will report it
                    // again, so it is assigned here, for the restore to
                    // match. One still assigned elsewhere is left to the
                    // report of its move, which takes it out of that tree.
                    let engine = &mut self.layout_manager.layout_engine;
                    let assigned_now = engine.active_workspace(desired).is_some_and(|workspace| {
                        engine.virtual_workspace_manager_mut().assign_window_to_workspace(
                            &mut self.state.windows,
                            desired,
                            *wid,
                            workspace,
                        )
                    });
                    if !assigned_now {
                        waiting.insert(*wid, desired);
                    }
                } else if assigned != Some(desired) {
                    waiting.insert(*wid, desired);
                }
                continue;
            }
            let Some(wsid) = wsid else {
                continue;
            };
            if addition && scripting_addition::move_window_to_space(wsid.as_u32(), desired.get()) {
                moved += 1;
                sent.push((*wid, wsid));
            } else {
                refused += 1;
            }
            waiting.insert(*wid, desired);
        }
        for (_, wsid) in &sent {
            self.note_window_sent_to_space(*wsid);
        }

        // Each display back on the desktop it was showing.
        for d in &record.displays {
            let Some(shown) = d.shown.map(map) else {
                continue;
            };
            if now.get(&d.uuid).is_some_and(|listed| listed.contains(&shown))
                && shown_now(&d.uuid) != Some(shown)
                && !scripting_addition::focus_space(shown.get())
            {
                warn!(display = %d.uuid, space = shown.get(), "Could not switch the display back to the desktop it was showing");
            }
        }

        let ids: HashMap<SpaceId, SpaceId> = record
            .displays
            .iter()
            .flat_map(|d| d.desktops.iter().copied())
            .map(|space| (map(space), space))
            .filter(|(now, _)| listed_all.contains(now))
            .collect();
        let restores: Vec<(SpaceId, SpaceId)> = ids
            .iter()
            .map(|(now, then)| (*then, *now))
            .filter(|(from, _)| !touched.contains(from))
            .collect();
        // A made desktop is done with once its windows have a replacement
        // desktop to go to; one whose destroyed desktop got no replacement
        // keeps its windows and stays.
        let retire: Vec<SpaceId> = record
            .stopgaps
            .iter()
            .filter(|(_, lost)| subst.contains_key(lost))
            .map(|(made, _)| *made)
            .collect();
        if refused > 0 {
            warn!(
                refused,
                scripting_addition = addition,
                "Windows could not be sent back; macOS 26 has no other way to move them (see sys::scripting_addition)"
            );
        }
        info!(
            replaced = ?subst,
            desktop_moves,
            windows_moved = moved,
            windows_waited_for = waiting.len(),
            restores = restores.len(),
            rearranged_while_away = ?touched,
            kept_made = ?record.stopgaps.iter().filter(|(made, _)| !retire.contains(made)).map(|(made, _)| made.get()).collect::<Vec<_>>(),
            "Displays back; put everything where the record has it"
        );
        crate::sys::trace::act(
            "reconcile",
            &(desktop_moves, moved, waiting.len(), restores.len()),
        );

        let immediate = waiting.is_empty();
        let record = self.display_archive.record.as_mut().expect("checked above");
        // Rewrite the record onto the new ids, for the restores that follow.
        for desired in record.windows.values_mut() {
            *desired = map(*desired);
        }
        for (desired, _) in record.placed.values_mut() {
            *desired = map(*desired);
        }
        record.own_moves.extend(sent.iter().map(|(wid, _)| *wid));
        record.pass = Some(Pass {
            stage: Stage::Back,
            ids,
            waiting,
            restores,
            retire,
            started: crate::sys::trace::now(),
        });
        if immediate {
            outcome.absorb(self.finish_pass());
            return outcome;
        }
        self.schedule_homing_deadline(RECORD_DEADLINE_KEY.to_string());
        outcome
    }

    /// Runs after every event while a pass is in flight: once every window
    /// waited for is assigned where it was sent, restore.
    pub(super) fn advance_record(&mut self) -> Option<EventOutcome> {
        let record = self.display_archive.record.as_mut()?;
        let pass = record.pass.as_mut()?;
        let landed: Vec<WindowId> = pass
            .waiting
            .iter()
            .filter(|(wid, target)| {
                self.state.windows.window(**wid).is_none()
                    || self
                        .layout_manager
                        .layout_engine
                        .virtual_workspace_manager()
                        .workspace_info_for_window_any(&self.state.windows, **wid)
                        .is_some_and(|info| info.space == **target)
            })
            .map(|(wid, _)| *wid)
            .collect();
        if landed.is_empty() {
            return None;
        }
        pass.waiting.retain(|wid, _| !landed.contains(wid));
        if pass.waiting.is_empty() {
            return Some(self.finish_pass());
        }
        None
    }

    pub(super) fn handle_record_deadline(&mut self) -> EventOutcome {
        let Some(pass) =
            self.display_archive.record.as_ref().and_then(|record| record.pass.as_ref())
        else {
            return EventOutcome::default();
        };
        if !pass.waiting.is_empty() {
            warn!(
                still_away = ?pass.waiting.keys().collect::<Vec<_>>(),
                waited_ms = pass.started.elapsed().as_millis(),
                "Not every window had arrived before the deadline; restoring without them"
            );
        }
        self.finish_pass()
    }

    /// `RestoreDepartureLayout`: the active desktop's tree as it was when a
    /// display last departed.
    pub(super) fn restore_departure_layout(&mut self) -> EventOutcome {
        let Some(space) = self.active_display_space() else {
            self.fail_command("no active desktop");
            return EventOutcome::default();
        };
        let Some(then) = self
            .display_archive
            .last_departure
            .as_ref()
            .and_then(|snapshot| snapshot.then.get(&space).copied())
        else {
            self.fail_command(format!(
                "no layout from a display's departure for desktop {}",
                space.get()
            ));
            return EventOutcome::default();
        };
        let layout = self.display_archive.last_departure.as_ref().expect("checked").layout.clone();
        let request = RestoreRequest {
            scope: RestoreScope::Space,
            active_space: space,
            source: RestoreSource::CurrentSpace,
            from_space: Some(then),
        };
        let layout_settings = self.config.settings.layout.clone();
        match self.layout_manager.layout_engine.restore_layout_from_snapshot(
            &layout,
            request,
            &mut self.state.windows,
            &layout_settings,
        ) {
            Ok(report) => {
                info!(
                    space = space.get(),
                    from = then.get(),
                    matched = report.matched,
                    unmatched = report.unmatched,
                    "Put the desktop's layout back the way it was at the last departure"
                );
                self.layout_manager.layout_engine.adopt_active_layout_for_all_sizes(space);
            }
            Err(error) => {
                self.fail_command(format!("could not restore the desktop's layout: {error}"));
                return EventOutcome::default();
            }
        }
        let mut outcome = EventOutcome::window_membership_changed(false, true);
        if let Some(screen) =
            self.space_state.screens.iter().find(|screen| screen.space == Some(space))
        {
            let size: CGSize = screen.frame.size;
            if size.width > 0.0 && size.height > 0.0 {
                outcome = outcome.with_layout_event(LayoutEvent::SpaceExposed(space, size));
            }
        }
        outcome.with_arrange_passes(1)
    }

    /// Whether a display is showing `space` right now, by the window
    /// server's word; the reactor's own screens can be a switch behind.
    fn shown_live(&self, space: SpaceId) -> bool {
        self.space_state.screens.iter().any(|screen| {
            crate::sys::screen::current_space_for_display_uuid(&screen.display_uuid)
                .or(screen.space)
                == Some(space)
        })
    }

    /// Destroys the desktops made at departure that are done with — their
    /// windows gone back — as soon as no display shows them. Destroying a
    /// desktop a display is showing is not something Dock survives, and the
    /// display that took one along comes back showing it, so the switch
    /// away that the return ordered has usually not landed by the time the
    /// return is over. Retried on every space change until it has.
    pub(super) fn retire_made_desktops(&mut self) {
        if self.display_archive.retiring.is_empty() {
            return;
        }
        let listed: HashSet<SpaceId> =
            self.display_space_ids_now().into_values().flatten().collect();
        let retiring = std::mem::take(&mut self.display_archive.retiring);
        for (made, since) in retiring {
            if !listed.contains(&made) {
                continue;
            }
            if self.shown_live(made) {
                if since.elapsed() > RETIRE_GIVE_UP {
                    warn!(
                        desktop = made.get(),
                        "The desktop made at departure is still being shown; leaving it"
                    );
                } else {
                    self.display_archive.retiring.push((made, since));
                }
                continue;
            }
            if scripting_addition::destroy_space(made.get()) {
                info!(desktop = made.get(), "Destroyed the desktop made at departure");
            } else {
                warn!(
                    desktop = made.get(),
                    "Could not destroy the desktop made at departure"
                );
            }
        }
    }

    /// Restores the pass's trees. After the return, the record is done and
    /// the desktops made at departure whose windows went back — empty by
    /// now — are destroyed once nothing shows them.
    fn finish_pass(&mut self) -> EventOutcome {
        let Some(record) = self.display_archive.record.as_mut() else {
            return EventOutcome::default();
        };
        let Some(pass) = record.pass.take() else {
            return EventOutcome::default();
        };
        let layout = record.layout.clone();
        let layout_settings = self.config.settings.layout.clone();
        for (from, to) in &pass.restores {
            let request = RestoreRequest {
                scope: RestoreScope::Space,
                active_space: *to,
                source: RestoreSource::CurrentSpace,
                from_space: Some(*from),
            };
            match self.layout_manager.layout_engine.restore_layout_from_snapshot(
                &layout,
                request,
                &mut self.state.windows,
                &layout_settings,
            ) {
                Ok(report) => info!(
                    from = from.get(),
                    space = to.get(),
                    matched = report.matched,
                    unmatched = report.unmatched,
                    "Restored a desktop's layout"
                ),
                // A desktop never shown has no tree to put back, and says
                // so through a workspace-count mismatch; that is not news.
                Err(error) => {
                    debug!(from = from.get(), space = to.get(), %error, "Did not restore a desktop's layout")
                }
            }
        }
        crate::sys::trace::act("pass_done", &(format!("{:?}", pass.stage), pass.restores.len()));
        if pass.stage == Stage::Back {
            let record = self.display_archive.record.take().expect("taken above");
            for made in pass.retire {
                self.display_archive.retiring.push((made, crate::sys::trace::now()));
            }
            self.display_archive.last_departure = Some(DepartureSnapshot {
                layout: record.layout,
                then: pass.ids,
            });
            self.retire_made_desktops();
        }
        let mut outcome = EventOutcome::window_membership_changed(false, true);
        for screen in &self.space_state.screens {
            let Some(space) = screen.space else {
                continue;
            };
            let size: CGSize = screen.frame.size;
            if size.width > 0.0 && size.height > 0.0 {
                outcome = outcome.with_layout_event(LayoutEvent::SpaceExposed(space, size));
            }
        }
        outcome.with_window_inventory_refresh().with_arrange_passes(1)
    }
}
