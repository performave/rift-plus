//! Where everything was when a display departed, reconciled against where
//! everything is once the display is back. This is what `displaced_windows =
//! "spaces"` does; the other modes are in `display_archive`.
//!
//! The window server reshuffles desktops and windows on its own at both
//! ends of an unplug, and not in one consistent way: on unplug it destroys
//! one desktop and merges its windows into the first desktop of whatever
//! survives — the *surviving* display's first, when it carries a departing
//! display's desktops over; the *departing* display's own, when that display
//! is the one being shown, as at a lid close, while its other desktops
//! migrate with their ids intact. Either way a desktop's worth of windows
//! ends up on top of a layout that is not theirs. On replug it mints a fresh
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
//!    is settled: the windows of the desktop macOS destroyed — whichever
//!    display it belonged to — get a desktop of their own, made for the
//!    purpose, with their tree, and standing where the destroyed one stood
//!    in its own display's order; the survivor's own desktops go first, the
//!    visitors behind them; and the survivor is switched back to the
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

use super::{LayoutEvent, Reactor, desktop_match};
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

/// How long after the window server last moved windows for a reconfiguration
/// an arrival on another desktop is still its doing rather than the user's.
///
/// `CHURN_SETTLE` measures from rift's own last sighting of a reshuffle,
/// which is a settle; the window server goes on moving windows well past
/// that, and over a long absence — a monitor off for a minute — it crosses
/// ten seconds easily. Asking the window server itself is the only honest
/// answer. Erring long costs a placement the user made just after a churn,
/// and the window then goes back where the record has it, which they can
/// redo; erring short writes the window server's own shuffling into the
/// record as the user's intent, and every later pass faithfully puts a
/// desktop's worth of windows in the wrong place.
const PLACEMENT_AFTER_CHURN: Duration = Duration::from_secs(30);

/// How long a recorded display the window server has stopped listing is
/// waited for before the record gives up on it.
///
/// The return only runs once every recorded display is back, which is what
/// makes it a single coherent diff rather than a series of guesses. The cost
/// is that a display which never comes back holds it up for good: a laptop
/// screen opened out of clamshell and shut again is in the record and then
/// gone, and everything else waits on it. Long enough to outlast the gaps a
/// churn leaves in the window server's own display list, and long enough to
/// outlast an ordinary unplug.
///
/// Measured, not guessed. Giving up on a display that does come back costs
/// its trees: the return reports `replaced={}` for it and restores nothing,
/// because the record no longer has it — the windows are not lost, but
/// nothing puts them back either, and an empty desktop is left over. At
/// twenty seconds that happened to a monitor unplugged for half a minute
/// while the desktop was still being used, which is an ordinary thing to do.
/// Two minutes still escapes a lid shut for the rest of a session, which is
/// the case this exists for, and leaves a plain unplug alone.
///
/// The absence is only aged while reports keep arriving, so a machine left
/// quiet with a display unplugged never gives up on it at all.
const GIVE_UP_ON_DISPLAY: Duration = Duration::from_secs(120);

/// How long a made desktop that a display keeps showing is retried before
/// it is left alone.
const RETIRE_GIVE_UP: Duration = Duration::from_secs(30);

/// How long after the return pass a window turning up somewhere other than
/// its recorded home is still the window server finishing the move, and is
/// taken back.
///
/// The pass ends once every window it waited for has arrived, and it waits
/// only for the ones that looked wrong at the instant it ran. The window
/// server goes on reassigning window-to-desktop membership afterwards, and
/// everything that could have put such a window right was dropped with the
/// record: in the 2026-09-21 incident a window arrived on the wrong desktop
/// 1.24s after `pass_done` -- a pass that reported `windows_waited_for=0` --
/// and stayed wrong for 51s, until the user moved it by hand. rift had
/// retained the correct home the whole time and had no trigger to act on it.
///
/// Short on purpose. The window server's own account of when it last moved
/// windows (`PLACEMENT_AFTER_CHURN`) says the arrival belongs to the churn
/// rather than to the user, but it is a coarse answer -- it stays true for
/// half a minute -- so the tight bound is this one. It is the same trade the
/// rest of the record makes: erring long costs a placement the user made in
/// the seconds after a return, which they can redo; erring short leaves them
/// with the state this exists to fix.
const AFTERCARE: Duration = Duration::from_secs(5);

pub(super) struct DisplayRecord {
    /// Every desktop's tree at departure, keyed by the desktop ids of then.
    layout: String,
    /// Trees taken after `layout`, for the desktops they cover: the
    /// survivor's, taken again when a display the record never knew left.
    /// See `refresh_record_survivor`.
    carried_layouts: HashMap<SpaceId, String>,
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
    /// Every desktop any report has listed since the record was taken,
    /// seeded with the ones it was taken with. Only ever read to tell a
    /// desktop's first sighting from its later ones — `seen` cannot, because
    /// it is written at a settle and says nothing about a desktop that came
    /// and went between two of them, and widening `seen` instead would
    /// change which desktops the return pairs.
    met: HashSet<SpaceId>,
    /// Of those, the ones first listed while the window server was still
    /// moving windows for a reconfiguration: macOS's own, minted for a
    /// departure or a return. One the user makes appears while the window
    /// server is quiet, so it is never in here.
    minted: HashSet<SpaceId>,
    /// Recorded displays the window server has stopped listing, and when it
    /// first stopped. Not the same as off screen: a display mid-churn is
    /// listed but shows nothing, while one that is disabled or unplugged is
    /// not listed at all. See `GIVE_UP_ON_DISPLAY`.
    absent: HashMap<String, Instant>,
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
    /// Desktops the user aimed a layout command at while a display was away.
    ///
    /// Reordering is the one edit the record cannot tell from the window
    /// server's own. A window moved to another desktop shows up in `placed`,
    /// and a layout mode change is visible in `modes`, but a window swapped
    /// past its neighbour leaves a desktop holding exactly the same windows
    /// in a different order -- which is also what a churn does to a tree it
    /// rebuilds. Reading every such difference as the user's meant a churn
    /// that reordered a desktop had its reordering preserved as though it had
    /// been asked for, and the recorded order was never put back.
    ///
    /// So rift records the commands it ran instead of trying to infer them
    /// afterwards. The window server issues none.
    user_commanded: HashSet<SpaceId>,
    /// Taken for an arrival rather than a departure: nothing left, and the
    /// record only describes the display that was already present. Such a
    /// record is a repair in progress, not a memory to keep -- it must not
    /// stand in a departure's way, and its pass must not take a desktop from
    /// the display that arrived. See `record_arrival`.
    arrival: bool,
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

/// What the return pass leaves behind when it drops the record: where each
/// window belonged, for as long as the window server may still be moving
/// them. See `AFTERCARE`.
pub(super) struct Aftercare {
    /// Where each window belongs, on the desktop ids of now. A window is
    /// taken home at most once -- the entry goes with the move -- so a
    /// correction can never turn into a tug of war with the window server.
    homes: HashMap<WindowId, SpaceId>,
    /// Windows aftercare has sent home and not yet seen arrive, with where.
    ///
    /// The Back pass registers every window it sends, so that the report of
    /// the window arriving is accepted as the window's new desktop even while
    /// a frame write for the tree it left is still pending. Aftercare runs
    /// after the pass has ended and did not: it moved the window, the arrival
    /// was overruled by that pending write, and rift went on holding the
    /// window in the tree it had left. The next departure then recorded that
    /// stale tree as the window's home, and the next aftercare "corrected" the
    /// window onto it -- a ratchet, one desktop per plug and unplug, ending
    /// with the window in no tree on a stand-in desktop nobody shows.
    pub(super) sent: HashMap<WindowId, SpaceId>,
    ended: Instant,
}

impl Aftercare {
    #[cfg(test)]
    pub(super) fn for_test(homes: HashMap<WindowId, SpaceId>) -> Self {
        Aftercare {
            homes,
            sent: HashMap::default(),
            ended: crate::sys::trace::now(),
        }
    }

    /// Where aftercare sent `wid`, while that send can still be in flight.
    pub(super) fn destination(&self, wid: WindowId) -> Option<SpaceId> {
        (self.ended.elapsed() <= AFTERCARE * 2)
            .then(|| self.sent.get(&wid).copied())
            .flatten()
    }
}

impl DisplayRecord {
    /// The desktop `wid` has been sent to and not yet assigned on, if it is
    /// one of the windows a pass waits for.
    pub(super) fn destination(&self, wid: WindowId) -> Option<SpaceId> {
        self.pass.as_ref().and_then(|pass| pass.waiting.get(&wid).copied())
    }

    /// Whether no pass is in flight, so a settle can run.
    pub(super) fn destination_free(&self) -> bool { self.pass.is_none() }

    pub(super) fn is_arrival(&self) -> bool { self.arrival }

    #[cfg(test)]
    pub(super) fn mark_arrival_for_test(&mut self) { self.arrival = true; }

    #[cfg(test)]
    pub(super) fn clear_pass_for_test(&mut self) { self.pass = None; }

    #[cfg(test)]
    pub(super) fn file_window_for_test(&mut self, wid: WindowId, space: SpaceId) {
        self.windows.insert(wid, space);
    }

    /// An arrival's record with nothing in it and its pass done.
    #[cfg(test)]
    pub(super) fn settled_arrival_for_test() -> Self {
        DisplayRecord {
            layout: String::new(),
            carried_layouts: HashMap::default(),
            members: HashMap::default(),
            modes: HashMap::default(),
            windows: HashMap::default(),
            placed: HashMap::default(),
            user_commanded: HashSet::default(),
            displays: Vec::new(),
            survivor: String::new(),
            met: HashSet::default(),
            seen: HashSet::default(),
            absent: HashMap::default(),
            minted: HashSet::default(),
            churn_seen: crate::sys::trace::now(),
            settled: true,
            stopgaps: Vec::new(),
            own_moves: HashSet::default(),
            arrival: true,
            pass: None,
        }
    }

    #[cfg(test)]
    pub(super) fn stand_in_for(&self, lost: SpaceId) -> Option<SpaceId> { self.stopgap_for(lost) }

    #[cfg(test)]
    pub(super) fn recorded_display(&self, uuid: &str) -> Option<(Vec<SpaceId>, Option<SpaceId>)> {
        self.displays
            .iter()
            .find(|d| d.uuid == uuid)
            .map(|d| (d.desktops.clone(), d.shown))
    }

    /// Takes the survivor's part again from `now`: its desktops, which
    /// windows are on each and in what order, their layout modes, and the
    /// trees to restore them from. A window the record wants on a display
    /// that is still away keeps that home -- it is only waiting on the
    /// survivor. Returns how many windows were re-filed.
    pub(super) fn refresh_survivor(
        &mut self,
        desktops: Vec<SpaceId>,
        members: HashMap<SpaceId, Vec<WindowId>>,
        modes: HashMap<SpaceId, Vec<LayoutMode>>,
        layout: String,
    ) -> usize {
        let away: HashSet<SpaceId> = self
            .displays
            .iter()
            .filter(|d| d.uuid != self.survivor)
            .flat_map(|d| d.desktops.iter().copied())
            .collect();
        // The survivor lists the desktops of the displays still away -- macOS
        // parks them there -- and the stand-ins rift made. Neither is the
        // survivor's: taken as its own, the away display's return found its
        // desktops already home and moved nothing back (`hot-swap`: Safari
        // stayed on the laptop).
        let made: HashSet<SpaceId> = self.stopgaps.iter().map(|(made, _)| *made).collect();
        let desktops: Vec<SpaceId> = desktops
            .into_iter()
            .filter(|space| !away.contains(space) && !made.contains(space))
            .collect();
        let mut refiled = 0;
        for space in &desktops {
            let wids = members.get(space).cloned().unwrap_or_default();
            for wid in &wids {
                if self.desired(*wid).is_some_and(|home| away.contains(&home)) {
                    continue;
                }
                self.placed.remove(wid);
                if self.windows.insert(*wid, *space) != Some(*space) {
                    refiled += 1;
                }
            }
            self.members.insert(*space, wids);
            if let Some(m) = modes.get(space) {
                self.modes.insert(*space, m.clone());
            }
            self.carried_layouts.insert(*space, layout.clone());
            self.user_commanded.remove(space);
        }
        self.seen.extend(desktops.iter().copied());
        self.met.extend(desktops.iter().copied());
        // Its desktops lost at the departure keep their place: their stand-ins
        // are how their windows find a replacement on the way back.
        let lost_with_stand_in: Vec<SpaceId> = self
            .displays
            .iter()
            .find(|d| d.uuid == self.survivor)
            .map(|d| {
                d.desktops
                    .iter()
                    .copied()
                    .filter(|space| {
                        !desktops.contains(space)
                            && self.stopgaps.iter().any(|(_, lost)| lost == space)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(survivor) = self.displays.iter_mut().find(|d| d.uuid == self.survivor) {
            survivor.desktops = lost_with_stand_in.into_iter().chain(desktops).collect();
        }
        refiled
    }

    /// Where the record wants `wid`: where the user put it while away, else
    /// where it was at departure.
    pub(super) fn desired(&self, wid: WindowId) -> Option<SpaceId> {
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
    pub(super) fn backdate(&mut self, by: Duration) { self.churn_seen -= by; }

    /// Ages every "the window server has stopped listing this display"
    /// stamp, so a test need not wait `GIVE_UP_ON_DISPLAY` out.
    #[cfg(test)]
    pub(super) fn backdate_absence(&mut self, by: Duration) {
        for since in self.absent.values_mut() {
            *since -= by;
        }
    }

    /// Files every desktop listed now that the record has not met before.
    /// One that turns up while the window server is still moving windows for
    /// a reconfiguration is macOS's, and may be retired when it turns out to
    /// hold nothing; one that turns up while it is quiet is the user's, and
    /// is left alone whether or not it is empty.
    pub(super) fn note_listed(&mut self, listed: impl Iterator<Item = SpaceId>, churning: bool) {
        for space in listed {
            if !self.met.insert(space) {
                continue;
            }
            if churning {
                self.minted.insert(space);
            }
        }
    }

    /// Whether `space` was first listed while the window server was moving
    /// windows for a reconfiguration, and so is macOS's rather than the
    /// user's.
    #[cfg(test)]
    pub(super) fn has_minted(&self, space: SpaceId) -> bool { self.minted.contains(&space) }

    /// Whether the window server has listed `space` since the record was
    /// taken. A desktop the return finds that is not one of these was
    /// minted for the return.
    #[cfg(test)]
    pub(super) fn has_seen(&self, space: SpaceId) -> bool { self.seen.contains(&space) }

    /// A desktop the window server listed after the record was taken and
    /// lists no longer, with the displays as they were: the user destroyed
    /// it (Mission Control, `destroy_space`), or macOS did outside any
    /// reshuffle, and either way nothing brings it back. The record forgets
    /// it — a made desktop together with the one it stood in for, so no
    /// later settle makes another — and every window filed on it: the
    /// window server has put those somewhere, and where each next turns up
    /// is where it belongs (`note_window_placed_while_away`). Returns the
    /// desktops forgotten and the windows let go.
    /// Notes which recorded displays the window server still lists, and
    /// gives up on any it has not listed for `GIVE_UP_ON_DISPLAY`. A display
    /// given up on is dropped from the record and its desktops forgotten, so
    /// its windows are filed wherever they next turn up — the same treatment
    /// a desktop destroyed while a display is away gets.
    ///
    /// The survivor is never given up on. If the window server has stopped
    /// listing that too, the record has nothing left to be about, and the
    /// answer is not to dismantle it one display at a time.
    ///
    /// Returns the displays dropped, with the desktops each took with it.
    pub(super) fn give_up_on_displays_gone_for_good(
        &mut self,
        listed: &HashSet<String>,
    ) -> Vec<(String, Vec<SpaceId>)> {
        let now = crate::sys::trace::now();
        for d in &self.displays {
            if listed.contains(&d.uuid) {
                self.absent.remove(&d.uuid);
            } else {
                self.absent.entry(d.uuid.clone()).or_insert(now);
            }
        }
        let give_up: Vec<String> = self
            .displays
            .iter()
            .map(|d| d.uuid.clone())
            .filter(|uuid| *uuid != self.survivor)
            .filter(|uuid| {
                self.absent.get(uuid).is_some_and(|since| since.elapsed() > GIVE_UP_ON_DISPLAY)
            })
            .collect();
        let mut dropped = Vec::new();
        for uuid in give_up {
            let Some(at) = self.displays.iter().position(|d| d.uuid == uuid) else {
                continue;
            };
            let gone = self.displays.remove(at);
            for desktop in &gone.desktops {
                self.forget_destroyed_desktop(*desktop);
            }
            self.absent.remove(&uuid);
            dropped.push((uuid, gone.desktops));
        }
        dropped
    }

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
    pub(super) fn recorded_desktops(&self, uuid: &str) -> Vec<SpaceId> {
        self.displays
            .iter()
            .find(|d| d.uuid == uuid)
            .map(|d| d.desktops.clone())
            .unwrap_or_default()
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
        // An arrival's record is a repair in progress. A display that leaves
        // again before that repair has finished -- `become-main` and
        // `clamshell` both do it within seconds -- must still be recorded,
        // or its return has nothing to put back.
        if self.display_archive.record.as_ref().is_some_and(|record| record.arrival) {
            info!(
                ?departed,
                "A display departed while an arrival was being repaired; recording the departure instead"
            );
            crate::sys::trace::act(
                "record_arrival",
                &serde_json::json!({ "superseded_by_departure": true }),
            );
            self.display_archive.record = None;
        }
        // A display the standing record does not know is one that arrived
        // after it was taken -- the home monitor of a commute, with the
        // office's record still waiting for the office. It stays out of the
        // record: the return waits for every display the record has, and
        // coming back to the office must not wait on the home monitor too.
        // But the record's picture of the laptop is from when the office
        // left, and settling from it for this departure moved every laptop
        // window onto desktop ids from then, out of the trees they were in;
        // they came back floating (`commute`). So the laptop's part is taken
        // again, as it is now.
        if let Some(record) = self.display_archive.record.as_ref() {
            let unknown =
                departed.iter().any(|uuid| !record.displays.iter().any(|d| &d.uuid == uuid));
            if unknown && record.pass.is_none() {
                self.refresh_record_survivor(&departed);
            }
            debug!(
                ?departed,
                "A display departed while a record stands; the record stands"
            );
            return;
        }
        let mut displays: Vec<RecordedDisplay> = match self.display_archive.whole_displays.as_ref()
        {
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
        // Belt and braces on top of the check in `note_display_set`: a
        // desktop can only be one display's. If a departing display and one
        // that stays both claim one, it is the departing display's — the
        // survivor cannot have gained a desktop in the instant it left, and
        // letting the claim stand would have the return drag the departing
        // display's own desktop onto the survivor.
        let taken: HashSet<SpaceId> = displays
            .iter()
            .filter(|d| departed.contains(&d.uuid))
            .flat_map(|d| d.desktops.iter().copied())
            .collect();
        for d in displays.iter_mut().filter(|d| !departed.contains(&d.uuid)) {
            let before = d.desktops.len();
            d.desktops.retain(|space| !taken.contains(space));
            if d.desktops.len() != before {
                warn!(
                    display = %d.uuid,
                    "The window server had already given a departing display's desktops away; recording them as the departing display's"
                );
            }
        }
        // A desktop rift is already retiring belongs to no layout: macOS
        // minted it for a return, or rift made it at a departure, and it is
        // only still here because a display was showing it. Recorded as a
        // display's own, its going read as a loss -- the survivor was made a
        // stand-in for the windows of a desktop that had none, and the return
        // kept macOS's next fresh desktop as its replacement, one desktop
        // more for every plug and unplug.
        let retiring: HashSet<SpaceId> =
            self.display_archive.retiring.iter().map(|(space, _)| *space).collect();
        if !retiring.is_empty() {
            for d in &mut displays {
                d.desktops.retain(|space| !retiring.contains(space));
                if d.shown.is_some_and(|space| retiring.contains(&space)) {
                    d.shown = None;
                }
            }
        }

        let from_pre_churn = self.display_archive.fresh_pre_churn().is_some();
        let homes = self
            .display_archive
            .fresh_pre_churn()
            .map(|pre| pre.homes.clone())
            .unwrap_or_default();
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
        let mut listed = 0usize;
        for (space, wids) in &members {
            for wid in wids {
                listed += 1;
                if self.state.windows.window(*wid).is_some() {
                    windows.insert(*wid, *space);
                }
            }
        }
        // The trees speak only for desktops the workspace manager has
        // initialised, which in practice means desktops that have been shown.
        // A window on a desktop rift has never shown is in no tree -- and is
        // reported as tiled, because it is not floating either -- so it was
        // simply missing from the record, and a churn that merged it onto the
        // survivor's desktop had nothing to put it back with. The pre-churn
        // snapshot's `homes` knows where it was; fill from there, and only
        // where the trees said nothing, so a tree's answer always wins.
        let mut from_homes = 0usize;
        for (wid, space) in &homes {
            if windows.contains_key(wid) || self.state.windows.window(*wid).is_none() {
                continue;
            }
            windows.insert(*wid, *space);
            from_homes += 1;
        }
        // Where the record's window list came from and what it cost on the
        // way. A departure recorded with fewer windows than the display set
        // had cannot put them back, and the count alone never said whether
        // the shortfall was the snapshot's or the filter's -- which left
        // "windows_moved=0" unexplainable from a dump.
        // Which tracked windows the record does not account for, by name. A
        // count alone says a record is short; it does not say of what, and
        // the answer decides whether the shortfall is a desktop the snapshot
        // never covered or a window the engine had already let go of.
        let missing: Vec<u32> = self
            .state
            .windows
            .iter_windows()
            .filter(|(wid, _)| !windows.contains_key(wid))
            .map(|(wid, _)| wid.idx.get())
            .take(12)
            .collect();
        crate::sys::trace::act(
            "record_members",
            &serde_json::json!({
                "pre_churn": from_pre_churn,
                "spaces": members.len(),
                "spaces_with_windows": members.values().filter(|w| !w.is_empty()).count(),
                "listed": listed,
                "recorded": windows.len(),
                "from_homes": from_homes,
                "missing": missing,
            }),
        );
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
        // A fresh departure supersedes whatever the last return was still
        // watching for.
        self.display_archive.aftercare = None;
        self.display_archive.record = Some(DisplayRecord {
            layout,
            carried_layouts: HashMap::default(),
            members,
            modes,
            windows,
            placed: HashMap::default(),
            user_commanded: HashSet::default(),
            displays,
            survivor,
            met: seen.clone(),
            seen,
            absent: HashMap::default(),
            minted: HashSet::default(),
            churn_seen: now,
            settled: false,
            stopgaps: Vec::new(),
            own_moves: HashSet::default(),
            arrival: false,
            pass: None,
        });
    }

    /// The standing record's survivor, as it is now. See `record_departure`.
    fn refresh_record_survivor(&mut self, departed: &[String]) {
        let Some(survivor) = self.display_archive.record.as_ref().map(|r| r.survivor.clone())
        else {
            return;
        };
        let desktops: Vec<SpaceId> = self
            .display_archive
            .whole_displays
            .as_ref()
            .and_then(|whole| whole.iter().find(|d| d.uuid == survivor))
            .map(|d| d.desktops.clone())
            .or_else(|| self.space_state.display_space_ids.get(&survivor).cloned())
            .unwrap_or_default();
        if desktops.is_empty() {
            return;
        }
        let (layout, members, modes) = match self.display_archive.fresh_pre_churn() {
            Some(pre) => (pre.layout.clone(), pre.members.clone(), pre.modes.clone()),
            None => {
                let engine = &mut self.layout_manager.layout_engine;
                let members: HashMap<SpaceId, Vec<WindowId>> = desktops
                    .iter()
                    .map(|space| (*space, engine.windows_on_space_in_layout_order(*space)))
                    .collect();
                let modes: HashMap<SpaceId, Vec<LayoutMode>> = desktops
                    .iter()
                    .map(|space| (*space, engine.layout_modes_on_space(*space)))
                    .collect();
                match engine.snapshot_current_layout_lightly(&self.state.windows) {
                    Ok(layout) => (layout, members, modes),
                    Err(error) => {
                        warn!(%error, "Could not take the survivor's layout again");
                        return;
                    }
                }
            }
        };
        let Some(record) = self.display_archive.record.as_mut() else {
            return;
        };
        let refiled = record.refresh_survivor(desktops.clone(), members, modes, layout);
        info!(
            ?departed,
            survivor = %survivor,
            desktops = ?desktops.iter().map(SpaceId::get).collect::<Vec<_>>(),
            refiled,
            "A display the record never knew departed; took the survivor's part of the record again"
        );
        crate::sys::trace::act(
            "record_refresh_survivor",
            &(desktops.iter().map(SpaceId::get).collect::<Vec<_>>(), refiled),
        );
    }

    /// A display arriving with no record standing: record the display that
    /// was already here, as it was, so its return pass can put it back.
    ///
    /// A departure leaves a record, and the display's return is what that
    /// record is for. An *arrival* nobody departed for has none -- after rift
    /// restarts or is redeployed while undocked, or around the pseudo display
    /// a lid reports -- and macOS reshuffles on an arrival too. In the recorded
    /// clamshell trace from Eric's machine, the LG arrived with no record open
    /// and two windows changed desktop. In the guest, one attach with a stack
    /// on the desktop switched the display to a fresh empty desktop, moved
    /// three of five windows to another new one where rift built them a fresh
    /// tree (the stack gone), and left the other two in no tree at all. Nothing
    /// put any of it back, and the next departure then recorded the damage as
    /// though it were the arrangement.
    ///
    /// The record taken here describes the display that was already present
    /// and nothing else, so its return pass -- which `begin_display_homing`
    /// runs straight away, because every display it names is on screen --
    /// brings that display's desktops, windows, trees and shown desktop back,
    /// and leaves the arriving display with whatever macOS gave it.
    ///
    /// Deliberately narrow:
    ///
    /// - only from a fresh pre-churn snapshot. Taken at the first window to
    ///   leave its tree, it is the last reading of the arrangement before the
    ///   reshuffle; a live reading now would record the reshuffle itself.
    /// - only when exactly one display was here before. The return pass hands
    ///   desktops it does not recognise to "the display that stayed", which
    ///   with several displays already present could move one of them to
    ///   another. The laptop alone, then the LG, is the case this is for.
    /// - only in `spaces` mode, which is the only mode with a record.
    pub(super) fn record_arrival(&mut self, active_displays: &[String]) {
        if self.display_archive.record.is_some() {
            return;
        }
        let Some(whole) = self.display_archive.whole_displays.clone() else {
            return;
        };
        let arrived: Vec<String> = active_displays
            .iter()
            .filter(|uuid| !whole.iter().any(|d| &d.uuid == *uuid))
            .cloned()
            .collect();
        if arrived.is_empty() || !whole.iter().all(|d| active_displays.contains(&d.uuid)) {
            return;
        }
        let skip = |why: &str| {
            debug!(
                ?arrived,
                why, "A display arrived with no record; not recording one"
            );
            crate::sys::trace::act("record_arrival", &serde_json::json!({ "skipped": why }));
        };
        if whole.len() != 1 {
            skip("more than one display was already here");
            return;
        }
        // The snapshot a first window leaving its tree takes. On a plug where
        // macOS has only moved frames by the time the arrival is seen, nothing
        // has left a tree and there is none -- and then the live trees are
        // still the state from before the arrival, so take it from them.
        if self.display_archive.fresh_pre_churn().is_none() {
            self.capture_pre_churn_layout();
        }
        let Some(pre) = self.display_archive.fresh_pre_churn() else {
            skip("no pre-churn snapshot could be taken");
            return;
        };
        let (layout, members, modes, homes) = (
            pre.layout.clone(),
            pre.members.clone(),
            pre.modes.clone(),
            pre.homes.clone(),
        );

        let recorded: HashSet<SpaceId> =
            whole.iter().flat_map(|d| d.desktops.iter().copied()).collect();
        let mut windows: HashMap<WindowId, SpaceId> = HashMap::default();
        for (space, wids) in &members {
            for wid in wids {
                if self.state.windows.window(*wid).is_some() {
                    windows.insert(*wid, *space);
                }
            }
        }
        for (wid, space) in &homes {
            if !windows.contains_key(wid) && self.state.windows.window(*wid).is_some() {
                windows.insert(*wid, *space);
            }
        }
        // Only what was on the display that was here. Anything else in the
        // snapshot is somewhere this record makes no claim about.
        windows.retain(|_, space| recorded.contains(space));

        // Desktops now listed on the display that was here which it did not
        // have before the arrival: macOS made them for the arrival -- nobody
        // makes a desktop in the second a reconfiguration takes -- and one left
        // empty is litter the pass may retire rather than keep.
        let now = self.display_space_ids_now();
        let minted: HashSet<SpaceId> = whole
            .iter()
            .flat_map(|d| now.get(&d.uuid).cloned().unwrap_or_default())
            .filter(|space| !recorded.contains(space))
            .collect();

        let survivor = whole[0].uuid.clone();
        crate::sys::trace::act(
            "record_arrival",
            &serde_json::json!({
                "arrived": arrived.len(),
                "windows": windows.len(),
                "desktops": recorded.len(),
                "shown": whole[0].shown.map(|s| s.get()),
                "minted": minted.iter().map(|s| s.get()).collect::<Vec<_>>(),
            }),
        );
        info!(
            ?arrived,
            %survivor,
            windows = windows.len(),
            shown = ?whole[0].shown,
            "A display arrived with no record; recorded the one already here, to put it back"
        );
        let now_instant = crate::sys::trace::now();
        self.display_archive.aftercare = None;
        self.display_archive.record = Some(DisplayRecord {
            layout,
            carried_layouts: HashMap::default(),
            members,
            modes,
            windows,
            placed: HashMap::default(),
            user_commanded: HashSet::default(),
            met: recorded.clone(),
            seen: recorded,
            displays: whole,
            survivor,
            absent: HashMap::default(),
            minted,
            churn_seen: now_instant,
            settled: false,
            stopgaps: Vec::new(),
            own_moves: HashSet::default(),
            arrival: true,
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
        // Every recorded display is on screen: this report is a return, and
        // the return puts everything back itself. Settling for a departure
        // here would add the desktops macOS mints for the return to the ones
        // seen while a display was away, and the return would then take them
        // for desktops the user made rather than the replacements they are —
        // leaving the destroyed desktop standing on the stopgap for good and
        // macOS's new one empty beside it.
        if record.displays.iter().all(|d| on_screen.contains(&d.uuid.as_str())) {
            return outcome;
        }
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
        // Only what a display still on screen lists. `now` starts from the
        // reactor's own display map, which is updated after this runs, so it
        // still carries the departing display's desktops under its name —
        // and counting those as listed is what hid the one macOS destroyed
        // on the way out, the desktop whose windows are now somebody else's
        // problem.
        let listed_all: HashSet<SpaceId> = now
            .iter()
            .filter(|(uuid, _)| on_screen.contains(&uuid.as_str()))
            .flat_map(|(_, spaces)| spaces.iter().copied())
            .collect();
        let on_survivor: Vec<SpaceId> = now.get(&survivor.uuid).cloned().unwrap_or_default();
        // The survivor's own desktops, by the id each has now: a destroyed
        // one is represented by the desktop made for it.
        let own_now: Vec<(SpaceId, SpaceId)> = survivor
            .desktops
            .iter()
            .map(|then| (record.stopgap_for(*then).unwrap_or(*then), *then))
            .collect();
        // The desktops of the displays that are away, the same way. macOS
        // does not carry over the desktop a departing display was showing:
        // it destroys that one and merges its windows into whatever the
        // survivor is showing, while the display's other desktops migrate
        // with their ids intact. So a departed display loses a desktop just
        // as the survivor can, and its windows are stranded on somebody
        // else's desktop just the same — but neither `kept` nor `visitors`
        // sees it, because it is on no display at all any more. Only ones
        // with windows to rescue: a desktop that went empty needs nothing,
        // and an empty desktop is what the window server reaps first.
        let away_now: Vec<(SpaceId, SpaceId)> = record
            .displays
            .iter()
            .filter(|d| d.uuid != survivor.uuid)
            .flat_map(|d| d.desktops.iter().copied())
            .map(|then| (record.stopgap_for(then).unwrap_or(then), then))
            .filter(|(_, then)| !record.windows_desired_on(*then).is_empty())
            .collect();
        let destroyed: Vec<(SpaceId, SpaceId)> = own_now
            .iter()
            .chain(away_now.iter())
            .copied()
            .filter(|(now, _)| !listed_all.contains(now))
            .collect();
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

        // A desktop for each destroyed desktop's windows. macOS lists the
        // visitors first; each new one goes after the last of them, and the
        // walk below puts it in place.
        //
        // Every destroyed desktop that had windows gets one, not only the
        // first. Undocking from two monitors destroys the desktop each was
        // showing, and with a stand-in for the first alone the second's
        // window was left merged among the laptop's, out of any tree
        // (`dock-two-monitors`: a Safari came back floating).
        //
        // Only for a desktop that had windows. The survivor's own desktop can
        // be an empty one macOS minted it when an arrival took the desktop it
        // was showing; macOS reaps it at the next departure, and a stand-in
        // for its windows -- of which it had none -- was made every time, and
        // one was left standing after the last unplug: a desktop more than
        // before the display was ever plugged in.
        let mut stopgaps: Vec<(SpaceId, SpaceId, SpaceId)> = Vec::new();
        let mut anchor = visitors.last().or(on_survivor.last()).copied();
        for (gone, lost) in destroyed
            .iter()
            .copied()
            .filter(|(_, lost)| !record.windows_desired_on(*lost).is_empty())
        {
            match anchor.filter(|_| addition).and_then(scripting_addition::create_space_after) {
                Some(made) => {
                    info!(display = %survivor.uuid, lost = lost.get(), gone = gone.get(), made = made.get(), "Made the survivor a desktop for the windows of the one macOS destroyed");
                    now.entry(survivor.uuid.clone()).or_default().push(made);
                    stopgaps.push((made, gone, lost));
                    anchor = Some(made);
                }
                None => {
                    warn!(display = %survivor.uuid, lost = lost.get(), scripting_addition = addition, "Could not make the survivor a desktop; its windows stay merged in among the visitors until the other display is back")
                }
            }
        }
        let made_for = |then: SpaceId| {
            stopgaps.iter().find(|(_, _, lost)| *lost == then).map(|(made, _, _)| *made)
        };

        // The survivor's own desktops first — the made one, then the kept
        // ones — and the visitors behind them, each moved behind the
        // previous only when it is not there already. A desktop made for a
        // departed display's destroyed one is that display's, not the
        // survivor's: it goes among the visitors, where the one it stands in
        // for stood, so the away display's desktops keep their order.
        let mut own: Vec<SpaceId> = stopgaps
            .iter()
            .filter(|(_, _, lost)| survivor.desktops.contains(lost))
            .map(|(made, _, _)| *made)
            .collect();
        own.extend(kept.iter().copied());
        let visitors: Vec<SpaceId> = record
            .displays
            .iter()
            .filter(|d| d.uuid != survivor.uuid)
            .flat_map(|d| d.desktops.iter().copied())
            .map(|then| made_for(then).or_else(|| record.stopgap_for(then)).unwrap_or(then))
            .filter(|s| now.get(&survivor.uuid).is_some_and(|listed| listed.contains(s)))
            .collect();
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
        for &(made, _, lost) in &stopgaps {
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
        let stands_in = |s: SpaceId| made_for(s).or_else(|| record.stopgap_for(s)).unwrap_or(s);
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
            made = ?stopgaps.iter().map(|(made, _, _)| made.get()).collect::<Vec<_>>(),
            desktop_moves,
            windows_moved = sent.len(),
            sent_again = astray.len(),
            "Settled the survivor while the other display is away"
        );
        crate::sys::trace::act("settle", &(desktop_moves, sent.len()));

        for &(made, gone, _) in &stopgaps {
            self.remap_space_state(gone, made);
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
        for (made, gone, lost) in stopgaps {
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
    /// The user aimed a layout command at these desktops. Only meaningful
    /// while a record stands, and only for a desktop of a display that is
    /// away — a command on a display that is present is not something the
    /// return has to preserve, because nothing is going to overwrite it.
    pub(super) fn note_user_layout_command(&mut self, spaces: &[SpaceId]) {
        let Some(record) = self.display_archive.record.as_mut() else {
            return;
        };
        for space in spaces {
            record.user_commanded.insert(*space);
        }
    }

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
            || crate::sys::display_churn::since_windows_last_moved()
                .is_some_and(|since| since < PLACEMENT_AFTER_CHURN)
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
        // Before anything waits on it: a display the window server has
        // stopped listing altogether is not coming back by itself, and the
        // return below waits for every recorded display.
        let listed_displays: HashSet<String> = self.display_space_ids_now().into_keys().collect();
        if let Some(record) = self.display_archive.record.as_mut() {
            for (uuid, desktops) in record.give_up_on_displays_gone_for_good(&listed_displays) {
                warn!(
                    display = %uuid,
                    desktops = ?desktops.iter().map(SpaceId::get).collect::<Vec<_>>(),
                    "The window server has stopped listing a display of the record; giving up on it so the rest can be put back"
                );
                crate::sys::trace::act("record_give_up", &(uuid, desktops.len()));
            }
        }
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

        // Where every window the window server knows of is now. A destroyed
        // desktop cannot be identified — it does not exist — so what stands
        // in for it is decided by the one thing that outlives it: the
        // windows that were on it. Window server ids survive a churn, an
        // unplug and a restart of rift; desktop ids survive none of them.
        let where_now: HashMap<WindowId, SpaceId> = self
            .state
            .windows
            .iter_tracked_window_server_ids()
            .filter_map(|wsid| {
                let wid = self.state.windows.tracked_window_id(wsid)?;
                Some((wid, crate::sys::window_server::window_space(wsid)?))
            })
            .collect();

        // A destroyed desktop is one the record has that is listed nowhere
        // now; a fresh one is listed under a display now, recorded nowhere,
        // not one rift made, and not seen before the return — one that came
        // along with some other display meanwhile stands in for nothing.
        // Each destroyed desktop takes the fresh one holding most of its
        // windows, best match first. Order is the tie-break and the answer
        // when the windows cannot speak — an empty desktop has nothing to
        // match on, and macOS lists the replacement first — so a churn that
        // moved nothing pairs exactly as it did before.
        // Matched across every display at once, not display by display. A
        // desktop does not reliably come back to the display it left: the
        // recorded clamshell trace has a space change display twenty times in
        // one session and the abuse trace fifteen, which is ordinary macOS
        // behaviour and not corruption. Pairing within each display made the
        // replacement unfindable whenever macOS minted it on the other one,
        // and a destroyed desktop that finds no replacement is a desktop that
        // vanishes with its windows.
        let mut was: Vec<desktop_match::Desktop> = Vec::new();
        for d in &record.displays {
            for (index, space) in d.desktops.iter().enumerate() {
                was.push(desktop_match::Desktop {
                    space: *space,
                    display: Some(d.uuid.clone()),
                    index,
                    windows: record.members.get(space).cloned().unwrap_or_default(),
                });
            }
        }
        let mut current: Vec<desktop_match::Desktop> = Vec::new();
        for (uuid, desktops) in now.iter() {
            for (index, space) in desktops.iter().enumerate() {
                current.push(desktop_match::Desktop {
                    space: *space,
                    display: Some(uuid.clone()),
                    index,
                    windows: where_now
                        .iter()
                        .filter(|(_, at)| *at == space)
                        .map(|(wid, _)| *wid)
                        .collect(),
                });
            }
        }
        // Stable input, so the matching does not depend on HashMap order.
        was.sort_by_key(|d| d.space.get());
        current.sort_by_key(|d| d.space.get());
        let matching = desktop_match::match_desktops(&was, &current);
        if !matching.unclaimed.is_empty() {
            // Desktops the window server lists that no recorded desktop's
            // windows account for: minted for the return, or made by the user
            // while a display was away. Which of the two is decided below, on
            // provenance; logging them here is what makes a churn that went
            // wrong readable in a trace afterwards, since this is the point
            // where the count of desktops rift can explain stops matching the
            // count it can see.
            debug!(
                unclaimed = ?matching.unclaimed.iter().map(|s| s.get()).collect::<Vec<_>>(),
                "Desktops no recorded desktop's windows speak for"
            );
        }

        let mut subst: HashMap<SpaceId, SpaceId> = HashMap::default();
        let mut paired: HashSet<SpaceId> = HashSet::default();
        for d in &was {
            // Only a desktop that is really gone needs standing in for; one
            // still listed is itself.
            if listed_all.contains(&d.space) {
                continue;
            }
            let Some(now_space) = matching.get(d.space) else {
                continue;
            };
            // Content has to be what won it. The matching also pairs two
            // empty desktops on position, which is right in general but wrong
            // here: an empty recorded desktop would take the one fresh
            // desktop that the destroyed desktop's exiled windows are waiting
            // for, and order -- which is all either of them has -- is the
            // fallback's job, further down, where the recorded order decides.
            let overlap = record
                .members
                .get(&d.space)
                .map(|were| were.iter().filter(|w| where_now.get(*w) == Some(&now_space)).count())
                .unwrap_or(0);
            if overlap == 0 {
                continue;
            }
            // And only a genuinely new desktop can stand in for it. One the
            // record already holds, one rift made at departure, or one that
            // came along with another display meanwhile, stands in for
            // nothing -- the same three exclusions the per-display pairing
            // made, kept because they are about provenance rather than about
            // which display a desktop is on.
            if recorded_all.contains(&now_space)
                || stopgaps.contains_key(&now_space)
                || record.seen.contains(&now_space)
            {
                continue;
            }
            debug!(
                lost = d.space.get(),
                now = now_space.get(),
                "Paired a destroyed desktop with the one holding its windows"
            );
            subst.insert(d.space, now_space);
            paired.insert(now_space);
        }

        // Then, per display and in order, whatever the windows could not
        // speak for. A desktop whose windows rift itself exiled to the
        // survivor at departure has nothing left on it to match against, and
        // the desktop minted for its return is empty too -- so content says
        // nothing about either and order is the whole answer. macOS lists the
        // replacement where the original was, so this is right far more often
        // than it is wrong, and it is what the per-display pairing did for
        // every case. It runs second so that a desktop the windows *can*
        // speak for is never claimed by position first.
        for d in &record.displays {
            let destroyed: Vec<SpaceId> = d
                .desktops
                .iter()
                .copied()
                .filter(|s| !listed_all.contains(s) && !subst.contains_key(s))
                .collect();
            let fresh: Vec<SpaceId> = now
                .get(&d.uuid)
                .into_iter()
                .flatten()
                .copied()
                .filter(|s| {
                    !recorded_all.contains(s)
                        && !stopgaps.contains_key(s)
                        && !record.seen.contains(s)
                        && !paired.contains(s)
                })
                .collect();
            for (lost, now_space) in destroyed.into_iter().zip(fresh) {
                subst.insert(lost, now_space);
                paired.insert(now_space);
            }
        }

        // And last, across displays. A desktop whose windows cannot speak for
        // it and whose own display was given no fresh desktop is otherwise
        // left with nothing to stand in for it, however many unclaimed fresh
        // desktops are sitting on the other display -- because macOS mints the
        // replacement wherever it likes, not where the loss was. Preferring
        // the display's own is why this runs last: it only ever picks up what
        // both earlier passes left.
        let leftover_fresh: Vec<SpaceId> = record
            .displays
            .iter()
            .flat_map(|d| now.get(&d.uuid).into_iter().flatten())
            .copied()
            .filter(|s| {
                !recorded_all.contains(s)
                    && !stopgaps.contains_key(s)
                    && !record.seen.contains(s)
                    && !paired.contains(s)
            })
            .collect();
        let leftover_lost: Vec<SpaceId> = record
            .displays
            .iter()
            .flat_map(|d| d.desktops.iter())
            .copied()
            .filter(|s| !listed_all.contains(s) && !subst.contains_key(s))
            .collect();
        for (lost, now_space) in leftover_lost.into_iter().zip(leftover_fresh) {
            debug!(
                lost = lost.get(),
                now = now_space.get(),
                "Paired a destroyed desktop with a replacement minted on another display"
            );
            subst.insert(lost, now_space);
            paired.insert(now_space);
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
        // the display was away, and belongs to the display that stayed —
        // macOS files the survivor's desktops under a returning display as
        // readily as it files anything else.
        //
        // One with nothing on it is not worth the trip. macOS mints desktops
        // freely across a reconfiguration and never takes them away again,
        // so an empty one of its making is litter: it is retired below
        // rather than moved. Only ones it minted, though — a desktop first
        // listed while the window server was quiet is the user's, and an
        // empty one of those is kept. See `note_listed`.
        let candidates: Vec<(String, SpaceId)> = record
            .displays
            .iter()
            .flat_map(|d| {
                now.get(&d.uuid).into_iter().flatten().copied().map(|s| (d.uuid.clone(), s))
            })
            .filter(|(_, s)| {
                !recorded_all.contains(s)
                    && !paired.contains(s)
                    && !stopgaps.contains_key(s)
                    && !retiring.contains(s)
            })
            .collect();
        let occupied: HashSet<SpaceId> = self
            .state
            .windows
            .iter_tracked_window_server_ids()
            .filter_map(crate::sys::window_server::window_space)
            .collect();
        let spare: Vec<SpaceId> = candidates
            .iter()
            .filter(|(_, s)| record.minted.contains(s) && !occupied.contains(s))
            .map(|(_, s)| *s)
            .collect();
        let unknown: Vec<SpaceId> =
            candidates.into_iter().map(|(_, s)| s).filter(|s| !spare.contains(s)).collect();

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
                    // Only the user's reordering is kept. Theirs comes with
                    // a command aimed at the desktop, or with a window they
                    // moved onto or off it; the window server's comes with
                    // neither.
                    let user_moved_something = record
                        .placed
                        .iter()
                        .any(|(w, (to, _))| *to == here || record.windows.get(w) == Some(x));
                    if record.user_commanded.contains(x)
                        || record.user_commanded.contains(&here)
                        || user_moved_something
                    {
                        return true;
                    }
                    debug!(
                        desktop = x.get(),
                        "The order changed with no command and no placement behind it; \
                         restoring the recorded one"
                    );
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
            // `remap_space_state`, spelt out: the record is still borrowed.
            self.layout_manager
                .layout_engine
                .remap_space(&mut self.state.windows, from, *new);
            self.fullscreen_slots.remap_space(from, *new);
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
                // After an arrival, a desktop the arriving display is showing
                // stays with it, whoever it used to belong to: taking a
                // display's shown -- and possibly only -- desktop off it
                // leaves that display with nothing to show.
                if record.arrival
                    && self
                        .space_state
                        .screens
                        .iter()
                        .any(|screen| screen.space == Some(space) && screen.display_uuid != d.uuid)
                {
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

        // The desktops macOS minted for the return that nothing claimed go
        // the way of the ones rift makes: destroyed once no display is
        // showing them, and left alone if one still is.
        for space in spare {
            if self.display_archive.retiring.iter().any(|(s, _)| *s == space) {
                continue;
            }
            info!(
                desktop = space.get(),
                "A desktop macOS minted for the return holds nothing; retiring it"
            );
            self.display_archive.retiring.push((space, crate::sys::trace::now()));
        }

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
    /// windows gone back — as soon as no display shows them and nothing is
    /// on them. Destroying a desktop a display is showing is not something
    /// Dock survives, and the display that took one along comes back
    /// showing it, so the switch away that the return ordered has usually
    /// not landed by the time the return is over. The windows are the same
    /// story: the return asks the window server to move them off, and when
    /// the pass ends on its deadline rather than on their arrival they are
    /// all still there. Neither is a reason to give the desktop up, only to
    /// come back to it — retried on every space change until it can go, and
    /// abandoned only after `RETIRE_GIVE_UP`.
    pub(super) fn retire_made_desktops(&mut self) {
        if self.display_archive.retiring.is_empty() {
            return;
        }
        let listed: HashSet<SpaceId> =
            self.display_space_ids_now().into_values().flatten().collect();
        let occupied: HashSet<SpaceId> = self
            .state
            .windows
            .iter_tracked_window_server_ids()
            .filter_map(crate::sys::window_server::window_space)
            .collect();
        let retiring = std::mem::take(&mut self.display_archive.retiring);
        for (made, since) in retiring {
            if !listed.contains(&made) {
                continue;
            }
            // Emptiness is decided when a desktop is put up for retirement,
            // and the windows are still moving then. A window still on it is
            // reason enough to leave it be for now: destroying it would hand
            // that window to whatever desktop macOS picks.
            let wait = if self.shown_live(made) {
                Some("a display is still showing it")
            } else if occupied.contains(&made) {
                Some("it still has windows on it")
            } else {
                None
            };
            if let Some(why) = wait {
                if since.elapsed() > RETIRE_GIVE_UP {
                    warn!(
                        desktop = made.get(),
                        why, "Giving up on destroying the desktop made at departure"
                    );
                } else {
                    self.display_archive.retiring.push((made, since));
                }
                continue;
            }
            if scripting_addition::destroy_space(made.get()) {
                info!(desktop = made.get(), "Destroyed the desktop made at departure");
                // And forget what was keyed on it. Nothing can return to a
                // destroyed desktop's id, so its workspaces are unreachable --
                // but they stayed in the workspace store, so a churn that made
                // and unmade a desktop added one to the workspace count every
                // cycle and never gave it back.
                self.layout_manager.layout_engine.forget_space(made);
            } else {
                warn!(
                    desktop = made.get(),
                    "Could not destroy the desktop made at departure"
                );
            }
        }
    }

    /// A window has turned up on a desktop the return pass did not put it
    /// on, while the window server is still moving windows for the
    /// reconfiguration. The pass is over and the record is gone, but the
    /// home it worked out is not: send the window back to it.
    ///
    /// Only windows the record knew, only desktops the return put back, only
    /// while `AFTERCARE` holds, and only while the window server itself says
    /// it has been moving windows for a display change. A window the user
    /// moves fails that last test and stays where they put it.
    pub(super) fn correct_straggler_after_return(&mut self, wid: WindowId, space: SpaceId) {
        let Some(aftercare) = self.display_archive.aftercare.as_ref() else {
            return;
        };
        if aftercare.ended.elapsed() > AFTERCARE {
            self.display_archive.aftercare = None;
            return;
        }
        // A fresh departure has its own record, and that record owns the
        // question from here.
        if self.display_archive.record.is_some() {
            self.display_archive.aftercare = None;
            return;
        }
        let Some(home) = aftercare.homes.get(&wid).copied() else {
            return;
        };
        if home == space {
            return;
        }
        if !crate::sys::display_churn::since_windows_last_moved()
            .is_some_and(|since| since < PLACEMENT_AFTER_CHURN)
        {
            return;
        }
        let Some(wsid) = self.state.windows.window(wid).and_then(|state| state.info.sys_id) else {
            return;
        };
        if !scripting_addition::is_available() {
            warn!(
                ?wid,
                space = space.get(),
                home = home.get(),
                "A window came back on the wrong desktop after the return; sending it home needs the scripting addition"
            );
            return;
        }
        self.display_archive
            .aftercare
            .as_mut()
            .expect("checked above")
            .homes
            .remove(&wid);
        if scripting_addition::move_window_to_space(wsid.as_u32(), home.get()) {
            if let Some(aftercare) = self.display_archive.aftercare.as_mut() {
                aftercare.sent.insert(wid, home);
            }
            self.note_window_sent_to_space(wsid);
            info!(
                ?wid,
                landed = space.get(),
                home = home.get(),
                "The window server put a window on the wrong desktop after the return; sent it home"
            );
            crate::sys::trace::act("record_aftercare", &(wid.idx.get(), space.get(), home.get()));
        } else {
            warn!(
                ?wid,
                landed = space.get(),
                home = home.get(),
                "Could not send a window home after the return"
            );
        }
    }

    /// A display the standing record does not know has arrived -- another
    /// monitor on the same port while the recorded one is away -- and the
    /// window server has carried one of the record's windows onto it. macOS
    /// does that for a window it remembers on that display, and the record,
    /// which has the window on a desktop still here, is the better answer:
    /// the window was on the laptop a moment ago and nobody moved it. Send it
    /// back, as rift's own move, so it is not then filed as the user's
    /// placement. Only while the window server is moving windows for the
    /// change, and only onto a desktop still listed; a window the user moves
    /// later fails the first test and stays where they put it.
    /// The same, for every window of the record, asked of the window server.
    /// A window macOS carries onto an arriving display does not always get a
    /// notice of its own -- rift can learn of the move from the display
    /// change's refresh alone -- so each report of a coherent display set
    /// looks for them too.
    pub(super) fn keep_record_windows_off_unknown_displays(&mut self) {
        let Some(record) = self.display_archive.record.as_ref() else {
            return;
        };
        // A departure's record only. An arrival's record is itself the repair
        // of what the arrival scrambled, with its own rule for the desktop the
        // new display shows; this taking windows off that desktop overruled it.
        if record.arrival
            || record.pass.is_some()
            || !crate::sys::display_churn::since_windows_last_moved()
                .is_some_and(|since| since < PLACEMENT_AFTER_CHURN)
        {
            return;
        }
        let wids: Vec<WindowId> =
            record.windows.keys().chain(record.placed.keys()).copied().collect();
        for wid in wids {
            let Some(space) = self
                .state
                .windows
                .window(wid)
                .and_then(|state| state.info.sys_id)
                .and_then(crate::sys::window_server::window_space)
            else {
                continue;
            };
            self.keep_record_window_off_an_unknown_display(wid, space);
        }
    }

    pub(super) fn keep_record_window_off_an_unknown_display(
        &mut self,
        wid: WindowId,
        space: SpaceId,
    ) {
        let Some(record) = self.display_archive.record.as_ref() else {
            return;
        };
        if record.arrival || record.pass.is_some() {
            return;
        }
        if !crate::sys::display_churn::since_windows_last_moved()
            .is_some_and(|since| since < PLACEMENT_AFTER_CHURN)
        {
            return;
        }
        let now = self.display_space_ids_now();
        let Some(owner) = now
            .iter()
            .find(|(_, spaces)| spaces.contains(&space))
            .map(|(uuid, _)| uuid.clone())
        else {
            return;
        };
        if record.displays.iter().any(|d| d.uuid == owner) {
            return;
        }
        let Some(recorded_home) = record.desired(wid) else {
            return;
        };
        // A home macOS has since replaced -- a display arriving as main
        // replaces the laptop's desktop -- is wherever the windows recorded on
        // it went: the tiled ones travel with the replacement, and a floating
        // window carried to the arriving display instead had nowhere to go
        // back to (`commute`: a TextEdit alone on the home monitor's desktop).
        let listed = |s: SpaceId| now.values().flatten().any(|l| *l == s);
        let home = if listed(recorded_home) {
            recorded_home
        } else {
            let mut counts: HashMap<SpaceId, usize> = HashMap::default();
            for other in record.windows_desired_on(recorded_home) {
                if other == wid {
                    continue;
                }
                if let Some(at) = self
                    .state
                    .windows
                    .window(other)
                    .and_then(|state| state.info.sys_id)
                    .and_then(crate::sys::window_server::window_space)
                    .filter(|at| listed(*at) && *at != space)
                {
                    *counts.entry(at).or_default() += 1;
                }
            }
            let Some((replacement, _)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
                return;
            };
            replacement
        };
        if home == space {
            return;
        }
        let Some(wsid) = self.state.windows.window(wid).and_then(|state| state.info.sys_id) else {
            return;
        };
        if !scripting_addition::is_available() {
            warn!(
                ?wid,
                landed = space.get(),
                home = home.get(),
                "A window was carried onto a display the record does not know; sending it back needs the scripting addition"
            );
            return;
        }
        if scripting_addition::move_window_to_space(wsid.as_u32(), home.get()) {
            if let Some(record) = self.display_archive.record.as_mut() {
                record.own_moves.insert(wid);
            }
            // On its way home, for everything that asks: its leaving the
            // desktop it was carried to must not replace the slot it is going
            // back to, as it did before this was registered.
            self.display_archive.kept_off.insert(wid, (home, crate::sys::trace::now()));
            self.note_window_sent_to_space(wsid);
            info!(
                ?wid,
                landed = space.get(),
                display = %owner,
                home = home.get(),
                "The window server carried a window onto a display the record does not know; sent it back"
            );
            crate::sys::trace::act(
                "record_kept_off_unknown",
                &(wid.idx.get(), space.get(), home.get()),
            );
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
        let carried_layouts = record.carried_layouts.clone();
        let modes = record.modes.clone();
        let layout_settings = self.config.settings.layout.clone();
        for (from, to) in &pass.restores {
            let layout = carried_layouts.get(from).unwrap_or(&layout);
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
                // The tree carries the layout mode, though, so a desktop
                // that did have one is left tiled when it was stacked. The
                // record has the modes; put those back on their own.
                Err(error) => {
                    debug!(from = from.get(), space = to.get(), %error, "Did not restore a desktop's layout");
                    let Some(modes) = modes.get(from) else {
                        continue;
                    };
                    let put_back = self.layout_manager.layout_engine.adopt_layout_modes_on_space(
                        &self.state.windows,
                        *to,
                        modes,
                    );
                    if put_back > 0 {
                        warn!(
                            from = from.get(),
                            space = to.get(),
                            workspaces = put_back,
                            %error,
                            "A desktop's tree could not be put back; its layout modes were"
                        );
                    }
                }
            }
        }
        crate::sys::trace::act("pass_done", &(format!("{:?}", pass.stage), pass.restores.len()));
        if pass.stage == Stage::Back {
            // The record answers "where does this window belong", and the
            // window server is not necessarily finished asking. Keep that
            // answer alive on its own for a few seconds past the pass.
            let homes: HashMap<WindowId, SpaceId> = record
                .windows
                .keys()
                .chain(record.placed.keys())
                .copied()
                .collect::<HashSet<WindowId>>()
                .into_iter()
                .filter_map(|wid| record.desired(wid).map(|space| (wid, space)))
                .filter(|(_, space)| pass.ids.contains_key(space))
                .collect();
            self.display_archive.aftercare = (!homes.is_empty()).then(|| Aftercare {
                homes,
                sent: HashMap::default(),
                ended: crate::sys::trace::now(),
            });
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
