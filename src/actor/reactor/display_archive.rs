//! Keeps a display's layout while the display is away, and puts it back when
//! the display returns.
//!
//! macOS does two things when a display disconnects — unplug, sleep, lid close,
//! a mode change that re-enumerates it — that between them lose a layout
//! entirely:
//!
//! 1. The display's space is destroyed, and the display gets a *brand-new*
//!    space id when it comes back. Layouts are keyed by space id, so the old
//!    tree is orphaned and the new space starts from nothing.
//! 2. Every window on it is moved to whichever display survived, and nothing
//!    ever moves them back.
//!
//! The archive answers both. When a display departs, its layout is snapshotted
//! (the same serialization the master file uses, kept in memory) together with
//! the windows that were on it. What happens to those windows meanwhile is
//! `displaced_windows`. The default, `"spaces"`, leaves them on their own
//! desktops: when the departed display was the main one macOS carries its
//! desktops over to the survivor, trees and all, and they are simply shown
//! there; a desktop macOS destroyed instead has its windows join the
//! survivor's tree as one cluster. `"float"` sends them onto the survivor's
//! own desktop as floats, and `"tile"` clusters them into its tree. When a
//! display with the same UUID reappears, the old space's workspaces are
//! remapped onto the new space id, the exiled windows are moved home through
//! the scripting addition, and once they have landed the snapshot is restored
//! onto the new space. Anything that never lands — closed in the meantime, or
//! no scripting addition — is simply left out.

use std::time::{Duration, Instant};

use dispatchr::queue;
use dispatchr::time::Time;
use objc2_core_foundation::CGSize;
use tracing::{debug, info, warn};

use super::{Event, LayoutEvent, Reactor};
use crate::actor::app::WindowId;
use crate::actor::reactor::events::EventOutcome;
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::{DisplacedWindows, LayoutMode};
use crate::layout_engine::{RestoreRequest, RestoreScope, RestoreSource};
use crate::sys::dispatch::DispatchExt;
use crate::sys::screen::{ScreenInfo, SpaceId};
use crate::sys::scripting_addition;
use crate::sys::window_server::WindowServerId;

/// How long to wait for exiled windows to arrive on the returned display
/// before restoring the layout with whatever has landed.
const HOMING_DEADLINE: Duration = Duration::from_secs(3);

/// How long a pre-churn snapshot stays usable. A display change is seen
/// within a second or two of the window server starting to move windows.
const PRE_CHURN_TTL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct DisplayArchive {
    entries: HashMap<String, ArchivedDisplay>,
    /// The layout as it was just before the window server started moving
    /// windows between spaces on its own. See `capture_pre_churn_layout`.
    pre_churn: Option<PreChurn>,
    /// Spaces mode: where everything was at departure, reconciled at
    /// return. See `display_record`.
    pub(super) record: Option<super::display_record::DisplayRecord>,
    /// Desktops rift made that could not be destroyed yet because a display
    /// was still showing them, with when they were first tried.
    pub(super) retiring: Vec<(SpaceId, Instant)>,
    /// The last record's layout, kept after its return for
    /// `RestoreDepartureLayout`.
    pub(super) last_departure: Option<super::display_record::DepartureSnapshot>,
    /// The displays as the window server last reported them whole — every
    /// screen showing a desktop, every managed display one of the screens.
    /// Mid-reshuffle it reports neither: a screen with no desktop, or the
    /// desktops filed under a display that is no screen at all. A record is
    /// taken from this, never from a report like that. See
    /// `note_display_set`.
    pub(super) whole_displays: Option<Vec<super::display_record::RecordedDisplay>>,
    /// The Mac has woken since the record was last settled: Dock may have
    /// gone to sleep before carrying the settle out, so the first whole
    /// display report after a wake runs it again.
    pub(super) recheck_after_wake: bool,
}

/// The window server starts moving windows the moment a display goes, and
/// its reports (and the AX frame notifications that follow) reach the reactor
/// before the display change itself does: by the time a display is seen to
/// have departed, rift has already re-homed some of its windows — and some
/// of the *survivor's* windows, which macOS merges into the departed
/// display's space — and pulled them out of their trees. So the first
/// cross-space removal that no drag and no rift move accounts for is taken
/// as the start of a churn, and the layout is snapshotted before it happens.
/// If a display change follows within the TTL, that snapshot is the one
/// worth keeping.
pub(super) struct PreChurn {
    pub(super) layout: String,
    /// The windows of every space, in layout order, as the trees had them.
    pub(super) members: HashMap<SpaceId, Vec<WindowId>>,
    /// The layout mode of every space's workspaces, in order.
    pub(super) modes: HashMap<SpaceId, Vec<LayoutMode>>,
    taken: Instant,
    /// Kept past the TTL: the display set went incoherent after this was
    /// taken and has not come back whole since, so the display change that
    /// consumes it is still to come — after a sleep, possibly. See
    /// `note_display_set`.
    pinned: bool,
}

pub(super) struct ArchivedDisplay {
    /// The space the display showed when it departed. Kept current through
    /// remaps so the return trip knows which workspaces to move.
    space: SpaceId,
    /// `LayoutEngine::snapshot_current_layout` at departure, before any of the
    /// display's windows were touched.
    snapshot: String,
    /// The windows that were on the display, in layout order.
    windows: Vec<ExiledWindow>,
    /// Set once the displaced windows have been arranged on the surviving
    /// display, so the arrangement is not redone on every later snapshot.
    settled: bool,
    /// Where the windows were sent when another display took over this
    /// display's space. See `evict_from_taken_over_space`.
    parked_on: Option<SpaceId>,
    /// A survivor whose own space macOS destroyed when it inherited a departed
    /// display's: its layout waits here until that display is back, because
    /// only then does the survivor get a space of its own again.
    waiting_for: Option<String>,
    /// The display's desktops when it departed. What it comes back with
    /// beyond these — the survivor's own desktops, kept or made while the
    /// display was away, that macOS filed with it — goes back to the
    /// survivor; and a window of this display that turns up on a desktop
    /// outside these while it is away has been moved there by the user.
    desktops: Vec<SpaceId>,
    homing: Option<Homing>,
}

#[derive(Clone)]
struct ExiledWindow {
    wid: WindowId,
    wsid: Option<WindowServerId>,
    /// The user's own float/tile choice before it was overridden to float.
    user_floating: Option<bool>,
    was_tiled: bool,
}

struct Homing {
    space: SpaceId,
    /// The space the snapshot has the layout under.
    from: SpaceId,
    /// Windows asked to move home, each with the space it was sent to, that
    /// the window server has not yet reported there.
    waiting: HashMap<WindowId, SpaceId>,
    started: Instant,
}

impl DisplayArchive {
    pub(super) fn is_empty(&self) -> bool { self.entries.is_empty() && self.record.is_none() }

    pub(super) fn has(&self, display_uuid: &str) -> bool { self.entries.contains_key(display_uuid) }

    #[cfg(test)]
    pub(super) fn archived_windows(&self, display_uuid: &str) -> Vec<WindowId> {
        self.entries
            .get(display_uuid)
            .map(|entry| entry.windows.iter().map(|window| window.wid).collect())
            .unwrap_or_default()
    }

    pub(super) fn is_homing(&self, display_uuid: &str) -> bool {
        self.entries.get(display_uuid).is_some_and(|entry| entry.homing.is_some())
    }

    /// The space `wid` has been sent home to and not yet reported on, if
    /// it is one of the windows a returned display is waiting for.
    pub(super) fn homing_destination(&self, wid: WindowId) -> Option<SpaceId> {
        self.entries
            .values()
            .filter_map(|entry| entry.homing.as_ref())
            .find_map(|homing| homing.waiting.get(&wid).copied())
            .or_else(|| self.record.as_ref().and_then(|record| record.destination(wid)))
    }

    /// A stay-behind archive is only actionable once the display it waits
    /// for is back on screen.
    fn is_ready(&self, display_uuid: &str, screens: &[ScreenInfo]) -> bool {
        self.entries
            .get(display_uuid)
            .and_then(|entry| entry.waiting_for.as_deref())
            .is_none_or(|waited| screens.iter().any(|screen| screen.display_uuid == waited))
    }

    pub(super) fn fresh_pre_churn(&self) -> Option<&PreChurn> {
        self.pre_churn
            .as_ref()
            .filter(|pre| pre.pinned || pre.taken.elapsed() < PRE_CHURN_TTL)
    }

    #[cfg(test)]
    pub(super) fn backdate_pre_churn(&mut self, by: Duration) {
        if let Some(pre) = self.pre_churn.as_mut() {
            pre.taken -= by;
        }
    }

    fn any_homing(&self) -> bool { self.entries.values().any(|entry| entry.homing.is_some()) }

    #[cfg(test)]
    pub(super) fn record(&self) -> Option<&super::display_record::DisplayRecord> {
        self.record.as_ref()
    }

    #[cfg(test)]
    pub(super) fn record_mut(&mut self) -> Option<&mut super::display_record::DisplayRecord> {
        self.record.as_mut()
    }
}

/// Fallback `cgdisplay-N` ids are not identities: N is reassigned on every
/// reconnect, so a layout filed under one could never be found again.
fn is_stable_display_uuid(uuid: &str) -> bool { !uuid.starts_with("cgdisplay-") }

impl Reactor {
    fn display_archive_enabled(&self) -> bool { self.config.settings.restore_display_layouts }

    /// An explicit command or drop on a displaced window while its display is
    /// away is a claim on it for the display it is on: it is no longer sent
    /// back on replug, and the display it is on takes it along wherever that
    /// display's own windows go. Automatic changes — macOS moving frames,
    /// rift reflowing — never adopt anything; only this is called for them.
    pub(super) fn adopt_displaced_window(&mut self, wid: WindowId) {
        let Some(from) = self
            .display_archive
            .entries
            .iter()
            .find(|(_, entry)| {
                entry.homing.is_none() && entry.windows.iter().any(|window| window.wid == wid)
            })
            .map(|(uuid, _)| uuid.clone())
        else {
            return;
        };
        let shown_on = self.assigned_space_for_window_id(wid).and_then(|space| {
            self.space_state
                .screens
                .iter()
                .find(|screen| screen.space == Some(space))
                .map(|screen| screen.display_uuid.clone())
        });
        if shown_on.as_deref() == Some(from.as_str()) {
            // Its own display is back on screen; nothing to claim.
            return;
        }
        let Some(state) = self.state.windows.window(wid) else {
            return;
        };
        let adopted = ExiledWindow {
            wid,
            wsid: state.info.sys_id,
            user_floating: self.state.windows.user_floating(wid),
            was_tiled: !self.layout_manager.layout_engine.is_window_floating(wid),
        };
        if let Some(entry) = self.display_archive.entries.get_mut(&from) {
            entry.windows.retain(|window| window.wid != wid);
        }
        // A display whose own space is gone takes the window along when it
        // gets a new one; a display whose space is alive simply keeps it.
        if let Some(uuid) = shown_on.as_deref()
            && let Some(entry) = self.display_archive.entries.get_mut(uuid)
        {
            entry.windows.retain(|window| window.wid != wid);
            entry.windows.push(adopted);
        }
        info!(?wid, from = %from, to = ?shown_on, "Displaced window adopted by the display it was handled on");
    }

    /// Called from the layout-event sink just before a window leaves its
    /// tree for another space without a drag. See `PreChurn`.
    pub(super) fn capture_pre_churn_layout(&mut self) {
        if !self.display_archive_enabled()
            || matches!(
                self.drag_manager.drag_state,
                super::DragState::Active { .. } | super::DragState::PendingSwap { .. }
            )
            || self.display_archive.fresh_pre_churn().is_some()
        {
            return;
        }
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
            Ok(layout) => {
                crate::sys::trace::act("pre_churn", &members.len());
                self.display_archive.pre_churn = Some(PreChurn {
                    layout,
                    members,
                    modes,
                    taken: crate::sys::trace::now(),
                    pinned: false,
                });
            }
            Err(error) => debug!(%error, "Could not take a pre-churn layout snapshot"),
        }
    }

    /// The windows on `space`: those in its trees and floats now, plus any
    /// the last authoritative snapshot placed there that the churn has
    /// already moved away. Tree order first.
    fn windows_belonging_to_space(&self, space: SpaceId) -> Vec<ExiledWindow> {
        let engine = &self.layout_manager.layout_engine;
        let mut wids = engine.windows_on_space_in_layout_order(space);
        if let Some(pre) = self.display_archive.fresh_pre_churn() {
            // The churn moves windows both ways: this space's own out, and
            // the other display's in. A window the snapshot had on some
            // other space is one the churn brought here — the survivor's,
            // merged into a visitor's desktop — and is not this space's to
            // archive or to bring back later.
            wids.retain(|wid| {
                !pre.members
                    .iter()
                    .any(|(other, members)| *other != space && members.contains(wid))
            });
            if let Some(members) = pre.members.get(&space) {
                let carried: Vec<WindowId> =
                    members.iter().copied().filter(|wid| !wids.contains(wid)).collect();
                wids.extend(carried);
            }
        }
        wids.into_iter()
            .filter_map(|wid| {
                let state = self.state.windows.window(wid)?;
                Some(ExiledWindow {
                    wid,
                    wsid: state.info.sys_id,
                    user_floating: self.state.windows.user_floating(wid),
                    was_tiled: !engine.is_window_floating(wid),
                })
            })
            .collect()
    }

    /// The layout to archive for a space that is going away: the pre-churn
    /// snapshot if there is a fresh one, else the layout as it is now.
    fn snapshot_for_departure(&mut self, space: SpaceId) -> anyhow::Result<String> {
        if let Some(pre) = self.display_archive.fresh_pre_churn() {
            return Ok(pre.layout.clone());
        }
        self.layout_manager
            .layout_engine
            .snapshot_current_layout(&self.state.windows, Some(space))
    }

    /// Every authoritative snapshot passes through here, after any departure
    /// in it has been dealt with. One that reports the displays whole is
    /// remembered as the state a record can be taken from. One that does not
    /// is the window server mid-reshuffle — a lid closing, a display half
    /// gone — and the display change that ends it may only arrive after a
    /// sleep: the pre-churn snapshot in hand (taken now if there is none) is
    /// pinned until the displays are whole again, so the record still gets
    /// the trees from before the reshuffle.
    pub(super) fn note_display_set(
        &mut self,
        screens: &[ScreenInfo],
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) -> EventOutcome {
        let mut outcome = EventOutcome::default();
        let whole = !screens.is_empty()
            && screens.iter().all(|screen| screen.space.is_some())
            && display_space_ids
                .keys()
                .all(|uuid| screens.iter().any(|screen| &screen.display_uuid == uuid));
        if whole {
            self.forget_desktops_destroyed_while_away(screens, display_space_ids);
            self.display_archive.whole_displays = Some(
                screens
                    .iter()
                    .map(|screen| super::display_record::RecordedDisplay {
                        uuid: screen.display_uuid.clone(),
                        desktops: display_space_ids
                            .get(&screen.display_uuid)
                            .cloned()
                            .unwrap_or_else(|| screen.space.into_iter().collect()),
                        shown: screen.space,
                    })
                    .collect(),
            );
            if let Some(pre) = self.display_archive.pre_churn.as_mut() {
                pre.pinned = false;
            }
            if self.display_archive.recheck_after_wake
                && self
                    .display_archive
                    .record
                    .as_ref()
                    .is_some_and(|record| record.destination_free())
            {
                self.display_archive.recheck_after_wake = false;
                outcome.absorb(self.settle_after_departure(screens));
            }
            return outcome;
        }
        if self.display_archive.fresh_pre_churn().is_none() {
            self.capture_pre_churn_layout();
        }
        if let Some(pre) = self.display_archive.pre_churn.as_mut() {
            if !pre.pinned {
                crate::sys::trace::act("pre_churn_pinned", &pre.members.len());
            }
            pre.pinned = true;
        }
        outcome
    }

    /// A desktop in the last whole report and not in this one, with the same
    /// displays on screen and no reshuffle under way, is gone for good: the
    /// user destroyed it, or macOS did on its own. The record forgets it
    /// (see `DisplayRecord::forget_destroyed_desktop`), or else the next
    /// settle — the one after a wake included — takes a desktop the user
    /// removed for one macOS destroyed, makes another in its place, sends
    /// windows the user had put elsewhere onto it and reorders around it.
    /// A made desktop Dock never got to list is not in the last whole report
    /// and is not forgotten here, so that settle still makes it again.
    fn forget_desktops_destroyed_while_away(
        &mut self,
        screens: &[ScreenInfo],
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) {
        if self.refresh_quarantine_manager.display_churn_active {
            return;
        }
        let Some(previous) = self.display_archive.whole_displays.as_ref() else {
            return;
        };
        let same_displays = previous.len() == screens.len()
            && previous.iter().all(|d| screens.iter().any(|s| s.display_uuid == d.uuid));
        if !same_displays {
            return;
        }
        let listed: HashSet<SpaceId> = display_space_ids.values().flatten().copied().collect();
        let gone: Vec<SpaceId> = previous
            .iter()
            .flat_map(|d| d.desktops.iter().copied())
            .filter(|s| !listed.contains(s))
            .collect();
        if gone.is_empty() {
            return;
        }
        let Some(record) = self.display_archive.record.as_mut() else {
            return;
        };
        if !record.destination_free() {
            return;
        }
        for space in gone {
            let (forgotten, windows) = record.forget_destroyed_desktop(space);
            if forgotten.is_empty() {
                continue;
            }
            info!(
                desktop = space.get(),
                forgotten = ?forgotten.iter().map(|s| s.get()).collect::<Vec<_>>(),
                windows = windows.len(),
                "A desktop was destroyed while a display is away; the record forgets it, and its windows are filed wherever they next turn up"
            );
            crate::sys::trace::act("record_forget", &(space.get(), windows.len()));
        }
    }

    /// Called with the new display set before the engine forgets the displays
    /// missing from it. Archives each departed display's layout and readies
    /// its windows for life on the surviving display.
    pub(super) fn archive_departed_displays(
        &mut self,
        active_displays: &[String],
        screens: &[ScreenInfo],
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) -> EventOutcome {
        let mut outcome = EventOutcome::default();
        if !self.display_archive_enabled() {
            return outcome;
        }
        let departed = self.layout_manager.layout_engine.departed_displays(active_displays);
        if self.config.settings.displaced_windows == DisplacedWindows::Spaces {
            let departed: Vec<String> = departed
                .into_iter()
                .map(|(uuid, _)| uuid)
                .filter(|uuid| is_stable_display_uuid(uuid))
                .collect();
            if !departed.is_empty() {
                self.record_departure(departed, active_displays);
                outcome.absorb(self.settle_after_departure(screens));
            }
            self.display_archive.pre_churn = None;
            return outcome;
        }
        for (uuid, space) in departed {
            if !is_stable_display_uuid(&uuid) {
                continue;
            }
            // macOS does not destroy the *main* display's space when that
            // display goes: it migrates the space, layout and all, onto a
            // surviving display, and parks that display's own space behind
            // it. Nothing has moved, so nothing is displaced by the
            // reassignment below; the eviction in `archive_display` is what
            // handles it.
            let taken_over_by = screens
                .iter()
                .find(|screen| screen.space == Some(space))
                .map(|screen| screen.display_uuid.clone());
            if let Some(survivor) = &taken_over_by {
                outcome.absorb(self.archive_survivors_lost_space(
                    survivor,
                    &uuid,
                    display_space_ids,
                ));
            }
            self.archive_display(uuid, space, taken_over_by, display_space_ids);
        }
        // Whatever was captured has served its purpose; the next churn gets
        // its own.
        self.display_archive.pre_churn = None;
        outcome
    }

    fn archive_display(
        &mut self,
        uuid: String,
        space: SpaceId,
        taken_over_by: Option<String>,
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) {
        // The survivor's own windows may already have been merged into this
        // space by the takeover, and the survivor's archive (made just
        // before) holds them. They are its, not this display's: claimed here
        // too, the return trip would carry them off to the other display.
        let claimed: HashSet<WindowId> = self
            .display_archive
            .entries
            .iter()
            .filter(|(other, _)| **other != uuid)
            .flat_map(|(_, entry)| entry.windows.iter().map(|window| window.wid))
            .collect();
        let mut windows = self.windows_belonging_to_space(space);
        windows.retain(|window| !claimed.contains(&window.wid));

        if let Some(existing) = self.display_archive.entries.get_mut(&uuid) {
            // The display left again mid-return. The snapshot from its first
            // departure is the layout the user actually built; what is on the
            // new space now is at best a partial restore of it. Keep the old
            // snapshot, but track the space it now lives under and any windows
            // that did make it home so they are brought back next time too.
            existing.space = space;
            existing.homing = None;
            existing.settled = false;
            existing.parked_on = None;
            existing.waiting_for = None;
            existing.desktops =
                self.space_state.display_space_ids.get(&uuid).cloned().unwrap_or_default();
            for window in windows {
                if !existing.windows.iter().any(|known| known.wid == window.wid) {
                    existing.windows.push(window);
                }
            }
            match taken_over_by {
                None => self.displace_archived_windows(&uuid),
                Some(survivor) => {
                    self.evict_from_taken_over_space(&uuid, space, &survivor, display_space_ids)
                }
            }
            return;
        }

        let snapshot = match self.snapshot_for_departure(space) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(display = %uuid, space = space.get(), %error, "Could not snapshot the display's layout; it will not be restored");
                return;
            }
        };
        info!(
            display = %uuid,
            space = space.get(),
            windows = ?windows.iter().map(|window| (window.wid, window.was_tiled)).collect::<Vec<_>>(),
            "Display departed; archived its layout"
        );
        self.display_archive.entries.insert(uuid.clone(), ArchivedDisplay {
            space,
            snapshot,
            windows,
            settled: false,
            parked_on: None,
            waiting_for: None,
            desktops: self.space_state.display_space_ids.get(&uuid).cloned().unwrap_or_default(),
            homing: None,
        });
        match taken_over_by {
            None => self.displace_archived_windows(&uuid),
            Some(survivor) => {
                self.evict_from_taken_over_space(&uuid, space, &survivor, display_space_ids)
            }
        }
    }

    /// When the survivor inherits the departed display's space, macOS
    /// destroys the survivor's own space outright and merges its windows
    /// into the inherited one — so they would ride back to the other display
    /// on replug. Archive the survivor's layout under the survivor's own
    /// UUID, to be restored onto whatever space it is given once the departed
    /// display has taken its space back.
    ///
    /// Nothing is done meanwhile, on purpose. The window server is what
    /// reshuffles the desktops on both unplug and replug, and everything rift
    /// did in between — a desktop of its own for the survivor, windows moved
    /// back mid-churn — it undid or moved along with the visitors on replug,
    /// while every desktop operation asked of Dock during the reshuffle was a
    /// chance to take Dock down. One pass at replug, once the topology is
    /// quiet, puts everything where it belongs.
    fn archive_survivors_lost_space(
        &mut self,
        survivor: &str,
        departed: &str,
        new_display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) -> EventOutcome {
        let outcome = EventOutcome::default();
        if self.display_archive.entries.contains_key(survivor) {
            return outcome;
        }
        // macOS destroys the survivor's *first* desktop in the takeover, not
        // necessarily the one it is showing; any others it had are carried
        // along behind the visitors. Every desktop that was the survivor's
        // and is now listed nowhere is gone; the one shown is preferred when
        // several are, and the rest are noted, since one entry per display
        // is all the archive holds.
        let shown = self
            .space_state
            .screens
            .iter()
            .find(|screen| screen.display_uuid == survivor)
            .and_then(|screen| screen.space);
        let still_listed = |space: &SpaceId| {
            new_display_space_ids.values().flatten().any(|listed| listed == space)
        };
        let mut vanished: Vec<SpaceId> = self
            .space_state
            .display_space_ids
            .get(survivor)
            .into_iter()
            .flatten()
            .copied()
            .chain(shown)
            .filter(|space| !still_listed(space))
            .collect();
        vanished.dedup();
        let Some(lost) = shown
            .filter(|space| vanished.contains(space))
            .or_else(|| vanished.first().copied())
        else {
            return outcome;
        };
        if vanished.len() > 1 {
            warn!(
                %survivor,
                ?vanished,
                kept = lost.get(),
                "Several of the survivor's desktops were destroyed; only one is archived"
            );
        }
        let windows = self.windows_belonging_to_space(lost);

        let snapshot = match self.snapshot_for_departure(lost) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!(%survivor, space = lost.get(), %error, "Could not snapshot the survivor's layout");
                return outcome;
            }
        };
        info!(
            %survivor,
            %departed,
            lost_space = lost.get(),
            windows = ?windows.iter().map(|window| window.wid).collect::<Vec<_>>(),
            "Survivor's own space was destroyed by the takeover; archived its layout until the other display is back"
        );
        self.display_archive.entries.insert(survivor.to_string(), ArchivedDisplay {
            space: lost,
            snapshot,
            windows,
            settled: true,
            parked_on: None,
            waiting_for: Some(departed.to_string()),
            desktops: self.space_state.display_space_ids.get(survivor).cloned().unwrap_or_default(),
            homing: None,
        });
        outcome
    }

    /// Each display's desktops as the window server lists them now, falling
    /// back to the last snapshot. The snapshot can be a reshuffle behind —
    /// listing a desktop destroyed a moment ago, or not yet the one just made
    /// — and asking Dock to move a desktop it no longer knows took Dock down.
    pub(super) fn display_space_ids_now(&self) -> HashMap<String, Vec<SpaceId>> {
        let mut now = self.space_state.display_space_ids.clone();
        now.extend(crate::sys::screen::managed_display_space_ids());
        now
    }

    /// The space a surviving display should go back to when it has been
    /// handed a departed display's space: one of its own, never one that
    /// came along from the departed display. macOS moves *all* of the
    /// departed display's desktops to the survivor, and by the time the
    /// takeover is seen the survivor's "last user space" may already be the
    /// one it was handed, so the departed display's own space list (from the
    /// snapshot before this one) is what tells the two apart. The survivor's
    /// previous user space is preferred; failing that, any space that was
    /// its own.
    pub(super) fn takeover_parking_space(
        &self,
        survivor: &str,
        departed: &str,
        taken_over: SpaceId,
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) -> Option<SpaceId> {
        let departed_spaces =
            self.space_state.display_space_ids.get(departed).cloned().unwrap_or_default();
        let own: Vec<SpaceId> = display_space_ids
            .get(survivor)?
            .iter()
            .copied()
            .filter(|space| *space != taken_over && !departed_spaces.contains(space))
            .collect();
        self.space_state
            .last_user_space_by_display
            .get(survivor)
            .copied()
            .filter(|space| own.contains(space))
            .or_else(|| own.first().copied())
    }

    /// Another display has inherited the departed display's space, tree and
    /// all, squeezed onto its screen. In `spaces` mode that is the point:
    /// the desktop keeps its windows and its tree, laid out for the screen
    /// it is on, and the snapshot puts the ratios back on return. In `float`
    /// mode it is the one thing the archive exists to prevent: send the
    /// departed display's windows to the survivor's own space as floats and
    /// switch the survivor back to it, so it shows the layout it had with
    /// the visitors floating over it. Both moves need the scripting
    /// addition; without it the takeover is left as macOS made it.
    fn evict_from_taken_over_space(
        &mut self,
        uuid: &str,
        taken_over: SpaceId,
        survivor: &str,
        display_space_ids: &HashMap<String, Vec<SpaceId>>,
    ) {
        match self.config.settings.displaced_windows {
            DisplacedWindows::Float => {}
            DisplacedWindows::Spaces => {
                info!(
                    display = %uuid,
                    %survivor,
                    taken_over = taken_over.get(),
                    "Space taken over by another display; its desktops stay as they are until it is back"
                );
                return;
            }
            DisplacedWindows::Tile => return,
        }
        let Some(parking) =
            self.takeover_parking_space(survivor, uuid, taken_over, display_space_ids)
        else {
            debug!(display = %uuid, %survivor, "Space taken over, but the survivor has no space of its own to go back to");
            return;
        };
        if !scripting_addition::is_available() {
            warn!(
                display = %uuid,
                %survivor,
                "Space taken over by another display; moving its windows aside needs the scripting addition"
            );
            return;
        }
        let Some(entry) = self.display_archive.entries.get_mut(uuid) else {
            return;
        };
        let mut moved = 0usize;
        let mut sent = Vec::new();
        for window in &entry.windows {
            let Some(wsid) = window.wsid else {
                continue;
            };
            if scripting_addition::move_window_to_space(wsid.as_u32(), parking.get()) {
                moved += 1;
                sent.push(wsid);
            }
        }
        entry.parked_on = Some(parking);
        for wsid in sent {
            self.note_window_sent_to_space(wsid);
        }
        info!(
            display = %uuid,
            %survivor,
            taken_over = taken_over.get(),
            parking = parking.get(),
            moved,
            "Space taken over by another display; parked its windows on the survivor's own space"
        );
        self.displace_archived_windows(uuid);
        if !scripting_addition::focus_space(parking.get()) {
            warn!(
                parking = parking.get(),
                "Could not switch the surviving display back to its own space"
            );
        }
    }

    /// Prepare a departed display's windows for the surviving display. In
    /// float mode this happens before they are reassigned: flagging them
    /// floating now means the reassignment projects them as floats, and the
    /// user-level override keeps app rules from tiling them back.
    fn displace_archived_windows(&mut self, uuid: &str) {
        if self.config.settings.displaced_windows != DisplacedWindows::Float {
            return;
        }
        let Some(entry) = self.display_archive.entries.get(uuid) else {
            return;
        };
        let tiled: Vec<WindowId> = entry
            .windows
            .iter()
            .filter(|window| window.was_tiled)
            .map(|window| window.wid)
            .collect();
        for wid in tiled {
            self.state.windows.set_user_floating(wid, true);
            self.layout_manager.layout_engine.mark_window_floating(wid);
        }
    }

    /// After a snapshot has been reconciled: in tile mode, gather each
    /// departed display's windows into one cluster of the tree they landed
    /// in, once they have all landed. The same in spaces mode, for windows
    /// whose desktop macOS destroyed: ones still on their own desktop are
    /// still assigned to the archived space, so they never get here.
    pub(super) fn settle_displaced_windows(&mut self) {
        if self.display_archive.is_empty()
            || self.config.settings.displaced_windows == DisplacedWindows::Float
        {
            return;
        }
        let uuids: Vec<String> = self
            .display_archive
            .entries
            .iter()
            .filter(|(_, entry)| !entry.settled && entry.homing.is_none())
            .map(|(uuid, _)| uuid.clone())
            .collect();
        for uuid in uuids {
            let entry = &self.display_archive.entries[&uuid];
            let tiled: Vec<WindowId> = entry
                .windows
                .iter()
                .filter(|window| window.was_tiled)
                .map(|window| window.wid)
                .filter(|wid| self.state.windows.window(*wid).is_some())
                .collect();
            let destinations: HashSet<SpaceId> =
                tiled.iter().filter_map(|wid| self.assigned_space_for_window_id(*wid)).collect();
            if destinations.contains(&entry.space) {
                // Not everything has been reassigned off the dead space yet.
                continue;
            }
            for space in destinations {
                self.layout_manager.layout_engine.cluster_windows_after_selection(space, &tiled);
            }
            if let Some(entry) = self.display_archive.entries.get_mut(&uuid) {
                entry.settled = true;
            }
        }
    }

    /// Called once the current screens are known and the engine's
    /// display→space map is up to date. Starts the return trip for every
    /// archived display that is on screen again.
    pub(super) fn begin_display_homing(&mut self) -> EventOutcome {
        self.retire_made_desktops();
        let mut outcome = self.reconcile_record();
        if self.display_archive.is_empty() {
            return outcome;
        }
        let on_screen: Vec<(String, SpaceId)> = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| Some((screen.display_uuid.clone(), screen.space?)))
            .collect();
        // Whatever else a display is waiting on, desktops of its own that
        // went with another display come back as soon as it is on screen.
        for (uuid, _) in &on_screen {
            if self.display_archive.has(uuid) {
                self.reclaim_stray_desktops(uuid);
            }
        }
        let returned: Vec<(String, SpaceId)> = on_screen
            .into_iter()
            .filter(|(uuid, _)| {
                self.display_archive.has(uuid)
                    && !self.display_archive.is_homing(uuid)
                    && self.display_archive.is_ready(uuid, &self.space_state.screens)
            })
            .collect();
        for (uuid, shown) in returned {
            // A display with several desktops can come back showing a
            // different one than it left on. The layout belongs on the desktop
            // it was built on, so if that space still exists on the display
            // the windows go there and the display is switched to it.
            let target = self.homing_target(&uuid, shown);
            outcome.absorb(self.home_display(&uuid, target));
            // Whether the return trip is still in flight or was over at once
            // (nothing to move), the display belongs on the desktop its
            // layout lives on, not on whatever macOS handed it.
            if target != shown && !scripting_addition::focus_space(target.get()) {
                warn!(display = %uuid, space = target.get(), "Could not switch the returned display to its restored desktop");
            }
        }
        outcome
    }

    /// Whatever a returning display comes back with beyond the desktops it
    /// left with is the survivor's — desktops it kept through the takeover,
    /// or made while the display was away — that macOS filed with the
    /// visitors and took along. Send them back, in order, behind the desktop
    /// the survivor is showing; the windows on them come along.
    fn reclaim_stray_desktops(&mut self, uuid: &str) {
        let Some(entry) = self.display_archive.entries.get(uuid) else {
            return;
        };
        // Only a departed display come back has strays; a survivor's entry
        // lists the desktops it lost, and everything it shows now is new.
        if entry.waiting_for.is_some() {
            return;
        }
        let listed = self.display_space_ids_now().remove(uuid).unwrap_or_default();
        let strays: Vec<SpaceId> =
            listed.into_iter().filter(|space| !entry.desktops.contains(space)).collect();
        if strays.is_empty() {
            return;
        }
        // The survivor: the display waiting for this one, else whichever
        // other display is on screen.
        let survivor = self
            .display_archive
            .entries
            .iter()
            .find(|(_, other)| other.waiting_for.as_deref() == Some(uuid))
            .map(|(other, _)| other.clone())
            .or_else(|| {
                self.space_state
                    .screens
                    .iter()
                    .find(|screen| screen.display_uuid != uuid)
                    .map(|screen| screen.display_uuid.clone())
            });
        let Some(anchor) = survivor.and_then(|survivor| {
            self.space_state
                .screens
                .iter()
                .find(|screen| screen.display_uuid == survivor)
                .and_then(|screen| screen.space)
        }) else {
            debug!(display = %uuid, ?strays, "Stray desktops, but no other display to send them to");
            return;
        };
        if !scripting_addition::is_available() {
            warn!(display = %uuid, ?strays, "The other display's desktops came back with this one; sending them back needs the scripting addition");
            return;
        }
        let mut previous = anchor;
        for space in strays {
            if scripting_addition::move_space_after_space(space.get(), previous.get(), false) {
                info!(display = %uuid, desktop = space.get(), "Sent a desktop back to the display it belongs to");
                previous = space;
            } else {
                warn!(display = %uuid, desktop = space.get(), "Could not send a desktop back");
            }
        }
    }

    /// A window of a departed display has turned up on a desktop that was
    /// not that display's: the user moved it there — Mission Control, a
    /// drag — while the display was away. It is theirs to place; the return
    /// trip must not take it back. (rift's own moves happen under homing,
    /// which is skipped here; and the survivor's windows, merged into the
    /// visitors' desktops by the takeover, are held by an entry that waits
    /// for another display, also skipped.)
    pub(super) fn note_window_appeared_while_away(&mut self, wid: WindowId, space: SpaceId) {
        let on_screen: Vec<String> = self
            .space_state
            .screens
            .iter()
            .map(|screen| screen.display_uuid.clone())
            .collect();
        let listed = self.space_state.display_space_ids.values().flatten().any(|s| *s == space);
        if !listed {
            return;
        }
        self.note_window_placed_while_away(wid, space);
        let mut adopted = Vec::new();
        for (uuid, entry) in self.display_archive.entries.iter_mut() {
            if entry.homing.is_some()
                || entry.waiting_for.is_some()
                || on_screen.contains(uuid)
                || entry.desktops.contains(&space)
                || !entry.windows.iter().any(|window| window.wid == wid)
            {
                continue;
            }
            entry.windows.retain(|window| window.wid != wid);
            adopted.push(uuid.clone());
        }
        for uuid in adopted {
            info!(?wid, display = %uuid, space = space.get(), "Window moved by the user while its display is away; it stays where it was put");
            crate::sys::trace::act("archive_adopt", &(wid.idx.get(), space.get()));
        }
    }

    pub(super) fn homing_target(&self, uuid: &str, shown: SpaceId) -> SpaceId {
        let Some(entry) = self.display_archive.entries.get(uuid) else {
            return shown;
        };
        // A survivor whose desktop was destroyed gets a fresh one from macOS
        // on replug, listed first — not necessarily shown, since macOS can
        // bring the display back on one of its kept desktops. The destroyed
        // desktop's layout belongs on the fresh one; the kept ones have
        // their own.
        if entry.waiting_for.is_some() {
            let fresh = self
                .display_space_ids_now()
                .remove(uuid)
                .unwrap_or_default()
                .into_iter()
                .find(|space| !entry.desktops.contains(space));
            return fresh.unwrap_or(shown);
        }
        let still_there = self
            .space_state
            .display_space_ids
            .get(uuid)
            .is_some_and(|spaces| spaces.contains(&entry.space));
        if still_there { entry.space } else { shown }
    }

    fn home_display(&mut self, uuid: &str, new_space: SpaceId) -> EventOutcome {
        let Some(entry) = self.display_archive.entries.get(uuid) else {
            return EventOutcome::default();
        };
        let old_space = entry.space;
        let tracked: Vec<(WindowId, Option<WindowServerId>)> = entry
            .windows
            .iter()
            .filter(|window| self.state.windows.window(window.wid).is_some())
            .map(|window| (window.wid, window.wsid))
            .collect();

        // Even when the space survived and nothing left it, the layout is
        // put back from the snapshot: while the tree was squeezed onto the
        // other display, macOS re-laid its windows and the resize reports
        // were folded into the split ratios. The snapshot has the ratios
        // from before.

        // Carry the old space's workspaces over to the new id. This is also
        // what stops the old id's state leaking forever. Not when another
        // display is still showing the old space (the clamshell case above):
        // those workspaces are that display's now, and the snapshot is
        // restored into the new space's own workspaces instead.
        // "In use" means listed as a desktop of any display, shown or not:
        // remapping a desktop's workspaces away from under it leaves that
        // desktop with no tree the next time it is shown.
        let old_space_in_use = self
            .space_state
            .screens
            .iter()
            .any(|screen| screen.space == Some(old_space) && screen.display_uuid != uuid)
            || self
                .space_state
                .display_space_ids
                .iter()
                .any(|(display, spaces)| display != uuid && spaces.contains(&old_space));
        if !old_space_in_use {
            self.layout_manager.layout_engine.remap_space(
                &mut self.state.windows,
                old_space,
                new_space,
            );
        }
        self.layout_manager
            .layout_engine
            .update_space_display(new_space, Some(uuid.to_string()));

        // Every window with a window-server id is waited for, whether or not
        // the addition took the request: the window server is the only
        // authority on where a window is, and a window that arrives on the
        // new space by any route counts. Ones that never arrive are cut off by
        // the deadline.
        let mut waiting: HashMap<WindowId, SpaceId> = HashMap::default();
        let mut refused = 0usize;
        let addition = scripting_addition::is_available();
        for (wid, wsid) in &tracked {
            let target = &new_space;
            // Where the window server has the window, not where rift has it
            // assigned: the remap above just renamed the old space to the
            // new one in every assignment, so a window macOS merged into a
            // visitor's desktop but that rift had not yet caught up with
            // read as "already there" and was never moved.
            // macOS can have done the move itself — on replug it puts the
            // windows it merged out of the destroyed desktop onto the fresh
            // one — ahead of rift's assignment catching up. Such a window is
            // not sent again, but the restore still waits for it to be
            // assigned there, or it would find nothing to match.
            let assigned = self.assigned_space_for_window_id(*wid);
            let already_there = wsid.and_then(crate::sys::window_server::window_space).or(assigned)
                == Some(*target);
            if already_there {
                if assigned != Some(*target) {
                    waiting.insert(*wid, *target);
                }
                continue;
            }
            let Some(wsid) = wsid else {
                continue;
            };
            if !addition || !scripting_addition::move_window_to_space(wsid.as_u32(), target.get()) {
                refused += 1;
            } else {
                self.note_window_sent_to_space(*wsid);
            }
            waiting.insert(*wid, *target);
        }
        if refused > 0 {
            warn!(
                display = %uuid,
                refused,
                scripting_addition = addition,
                "Windows could not be sent back to the returned display; macOS 26 has no other way to move them (see sys::scripting_addition)"
            );
        }
        info!(
            display = %uuid,
            old_space = old_space.get(),
            new_space = new_space.get(),
            moving = waiting.len(),
            "Display returned; homing its windows"
        );

        let entry = self.display_archive.entries.get_mut(uuid).expect("checked above");
        entry.space = new_space;
        let immediate = waiting.is_empty();
        entry.homing = Some(Homing {
            space: new_space,
            from: old_space,
            waiting,
            started: crate::sys::trace::now(),
        });
        if immediate {
            return self.finish_display_homing(uuid);
        }
        self.schedule_homing_deadline(uuid.to_string());
        EventOutcome::default()
    }

    pub(super) fn schedule_homing_deadline(&self, uuid: String) {
        let Some(sender) = self.communication_manager.events_tx.clone() else {
            return;
        };
        queue::main().after_f_s(
            Time::new_after(Time::NOW, HOMING_DEADLINE.as_nanos() as i64),
            (sender, uuid),
            |(sender, uuid)| sender.send(Event::DisplayHomingDeadline(uuid)),
        );
    }

    /// Runs after every event while a return trip is in flight: once every
    /// window asked to move home is reported on the new space, restore.
    pub(super) fn advance_display_homing(&mut self) -> Option<EventOutcome> {
        let record = self.advance_record();
        if !self.display_archive.any_homing() {
            return record;
        }
        let mut outcome = record.unwrap_or_default();
        let uuids: Vec<String> = self.display_archive.entries.keys().cloned().collect();
        for uuid in uuids {
            let landed = {
                let entry = self.display_archive.entries.get_mut(&uuid)?;
                let Some(homing) = entry.homing.as_mut() else {
                    continue;
                };
                let landed: HashSet<WindowId> = homing
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
                homing.waiting.retain(|wid, _| !landed.contains(wid));
                homing.waiting.is_empty()
            };
            if landed {
                outcome.absorb(self.finish_display_homing(&uuid));
            }
        }
        Some(outcome)
    }

    /// The deadline fired: restore with whatever has landed.
    pub(super) fn handle_display_homing_deadline(&mut self, uuid: &str) -> EventOutcome {
        if uuid == super::display_record::RECORD_DEADLINE_KEY {
            return self.handle_record_deadline();
        }
        let Some(entry) = self.display_archive.entries.get(uuid) else {
            return EventOutcome::default();
        };
        let Some(homing) = entry.homing.as_ref() else {
            return EventOutcome::default();
        };
        if !homing.waiting.is_empty() {
            warn!(
                display = %uuid,
                still_away = homing.waiting.len(),
                waited_ms = homing.started.elapsed().as_millis(),
                "Not every window made it home before the deadline; restoring without them"
            );
        }
        self.finish_display_homing(uuid)
    }

    fn finish_display_homing(&mut self, uuid: &str) -> EventOutcome {
        let Some(mut entry) = self.display_archive.entries.remove(uuid) else {
            return EventOutcome::default();
        };
        let Some(homing) = entry.homing.take() else {
            return EventOutcome::default();
        };
        let space = homing.space;

        // Windows that made it home get their own float/tile choice back; the
        // snapshot decides how they are projected. Ones still away keep the
        // float override, or the app rules would tile them into a tree on a
        // display they do not belong to.
        for window in &entry.windows {
            if homing.waiting.contains_key(&window.wid) {
                continue;
            }
            if self.state.windows.window(window.wid).is_some() {
                self.state.windows.restore_user_floating(window.wid, window.user_floating);
            }
        }

        let request = RestoreRequest {
            scope: RestoreScope::Space,
            active_space: space,
            source: RestoreSource::CurrentSpace,
            from_space: Some(homing.from),
        };
        let layout_settings = self.config.settings.layout.clone();
        match self.layout_manager.layout_engine.restore_layout_from_snapshot(
            &entry.snapshot,
            request,
            &mut self.state.windows,
            &layout_settings,
        ) {
            Ok(report) => {
                info!(
                    display = %uuid,
                    space = space.get(),
                    matched = report.matched,
                    unmatched = report.unmatched,
                    "Restored the display's layout"
                );
                // The restored state is the user's intent for these windows,
                // so it outranks the app rules from here on: a window the
                // snapshot tiles has no other defence against a catch-all
                // floating rule when it next appears on a space.
                for window in &entry.windows {
                    if homing.waiting.contains_key(&window.wid)
                        || self.state.windows.window(window.wid).is_none()
                        || self.assigned_space_for_window_id(window.wid) != Some(space)
                    {
                        continue;
                    }
                    let floating = self.layout_manager.layout_engine.is_window_floating(window.wid);
                    self.state.windows.set_user_floating(window.wid, floating);
                }
            }
            Err(error) => {
                warn!(display = %uuid, space = space.get(), %error, "Could not restore the display's layout");
                return EventOutcome::default();
            }
        }
        let size = self
            .space_state
            .screens
            .iter()
            .find(|screen| screen.space == Some(space))
            .map(|screen| screen.frame.size)
            .unwrap_or_else(|| CGSize::new(0.0, 0.0));
        let mut outcome = EventOutcome::window_membership_changed(false, true);
        if size.width > 0.0 && size.height > 0.0 {
            outcome = outcome.with_layout_event(LayoutEvent::SpaceExposed(space, size));
        }
        outcome.with_window_inventory_refresh().with_arrange_passes(1)
    }
}
