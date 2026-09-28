//! Native tabs: one window on screen, one window-server window per tab.
//!
//! AppKit's window tabbing (Finder, TextEdit, Preview, Xcode, …) keeps every
//! tab of a group as a window of its own, stacked at one frame, and shows one
//! at a time. Switching tabs orders the shown one out and the chosen one in,
//! in the same few milliseconds and at the same frame. To the user that is one
//! window; to rift it was two unrelated events:
//!
//! - the tab left behind looked like a window gone into native fullscreen, so
//!   it was taken out of its tree and given a fullscreen slot;
//! - the tab brought forward was a stranger, placed by the app rules — floated
//!   by a catch-all, so the tile "untiled itself" — or, if rift had seen it
//!   before, put back by its own old fullscreen slot, whose snapshot also
//!   brought back other tabs still hidden. Those ghosts took tiles of their
//!   own, and every arrange wrote a frame to them that AppKit applied to the
//!   whole group.
//!
//! This recognises the switch instead: a window of an app stops being drawn on
//! a desktop, and within a moment another window of the same app starts being
//! drawn on that desktop at the same frame. The tab arriving takes the leaving
//! tab's place — its leaf in the tree, swapped in place, or its float — and its
//! floating choice, and neither is given a fullscreen slot. Closing a tab,
//! which shows the next one, reads the same way and wants the same outcome.
//!
//! Whichever half is heard second triggers the hand-off. A tab rift has never
//! seen is the awkward case: the window server announces it by id before
//! discovery can say which window it is, and the leaving tab's removal comes
//! in between. The leaving tab is held in its tree through that gap, so the
//! newcomer can be swapped in exactly; a tab that cannot be (an old slot is
//! kept for it) is placed beside the neighbour the leaving tab had.

use std::time::{Duration, Instant};

use objc2_core_foundation::CGRect;
use tracing::info;

use super::{LayoutEvent, Reactor};
use crate::actor::app::{WindowId, pid_t};
use crate::layout_engine::Slot;
use crate::model::VirtualWorkspaceId;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{self, WindowServerId, WindowServerInfo};

/// How far apart the two halves of a tab switch may be heard. They arrive in
/// the same millisecond or two; discovery of a tab rift has never seen takes
/// a few more. Long enough for that, short enough that a window closed and a
/// sibling opened later is not taken for a switch.
const TAB_SWITCH_WINDOW: Duration = Duration::from_secs(1);

/// A discovery sweep for the app that comes back this long after a hold began
/// without the new tab in it is not going to bring it: the tab was not a
/// window rift manages. One sooner may be a sweep asked for before the tab
/// appeared.
const HOLD_SURVIVES_EARLY_SWEEPS: Duration = Duration::from_millis(250);

/// AppKit puts every tab of a group at one frame; this only absorbs rounding
/// between the accessibility and window-server readings of it.
const SAME_FRAME_TOLERANCE: f64 = 2.0;

#[derive(Default)]
pub(super) struct NativeTabs {
    departures: Vec<Departure>,
    arrivals: Vec<Arrival>,
    holds: Vec<Hold>,
}

/// A window that stopped being drawn, and the place it had when it did.
#[derive(Debug, Clone)]
struct Departure {
    window: WindowId,
    space: SpaceId,
    workspace: Option<VirtualWorkspaceId>,
    frame: CGRect,
    /// Where it sat in the tree, for a replacement found only after it was
    /// taken out. Tree layouts only.
    slot: Option<Slot>,
    floating: bool,
    user_floating: Option<bool>,
    at: Instant,
}

/// A window that started being drawn with no departure to take over yet.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    window: WindowId,
    at: Instant,
}

/// A departed tab kept in its tree while the window-server window that
/// appeared in its place waits for discovery to name it.
#[derive(Debug, Clone, Copy)]
struct Hold {
    window: WindowId,
    arriving: WindowServerId,
    at: Instant,
}

impl NativeTabs {
    fn prune_matches(&mut self, now: Instant) {
        self.departures.retain(|d| now.duration_since(d.at) < TAB_SWITCH_WINDOW);
        self.arrivals.retain(|a| now.duration_since(a.at) < TAB_SWITCH_WINDOW);
    }

    fn forget(&mut self, window: WindowId) {
        self.departures.retain(|d| d.window != window);
        self.arrivals.retain(|a| a.window != window);
    }

    fn hold_for(&self, window: WindowId) -> Option<Hold> {
        self.holds.iter().copied().find(|hold| hold.window == window)
    }
}

fn frames_match(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() <= SAME_FRAME_TOLERANCE
        && (a.origin.y - b.origin.y).abs() <= SAME_FRAME_TOLERANCE
        && (a.size.width - b.size.width).abs() <= SAME_FRAME_TOLERANCE
        && (a.size.height - b.size.height).abs() <= SAME_FRAME_TOLERANCE
}

impl Reactor {
    /// `window` has just been ordered out. Remember the place it had, in case
    /// a tab of its group is being brought forward to take it; if that tab
    /// was heard first, hand the place over now. Returns whether the layout
    /// changed.
    pub(super) fn note_native_tab_departure(&mut self, window: WindowId) -> bool {
        let now = crate::sys::trace::now();
        let mut changed = self.prune_native_tabs(now);
        self.native_tabs.forget(window);
        if self.is_mission_control_active() {
            return changed;
        }
        let Some(state) = self.state.windows.window(window) else {
            return changed;
        };
        let Some(wsid) = state.info.sys_id.filter(|_| state.is_admitted()) else {
            return changed;
        };
        // Where it was drawn, as recorded: asked now, the window server
        // already answers that an ordered-out window is on no desktop.
        let Some(space) = self
            .state
            .windows
            .window_server_space(wsid)
            .or_else(|| self.assigned_space_for_window_id(window))
        else {
            return changed;
        };
        let engine = &self.layout_manager.layout_engine;
        let floating = engine.is_window_floating(window);
        if !floating && !engine.is_window_tiled(space, window) {
            return changed;
        }
        let frame = window_server::live_window_frame(wsid).unwrap_or(state.frame_monotonic);
        let user_floating = self.state.windows.user_floating(window);
        let departure = Departure {
            window,
            space,
            workspace: engine.virtual_workspace_manager().workspace_for_window(
                &self.state.windows,
                space,
                window,
            ),
            frame,
            slot: engine.slot_of(space, window),
            floating,
            user_floating,
            at: now,
        };
        let arrival = self
            .native_tabs
            .arrivals
            .iter()
            .rev()
            .map(|arrival| arrival.window)
            .find(|arriving| self.takes_over(&departure, *arriving));
        match arrival {
            Some(arriving) => changed |= self.hand_off_native_tab(departure, arriving),
            None => self.native_tabs.departures.push(departure),
        }
        changed
    }

    /// `window` has just been ordered in, or discovered already drawn. If a
    /// tab of its group has just left the same place, take that tab's place.
    /// Returns whether the layout changed.
    pub(super) fn note_native_tab_arrival(&mut self, window: WindowId) -> bool {
        let now = crate::sys::trace::now();
        let changed = self.prune_native_tabs(now);
        if self.is_mission_control_active() {
            return changed;
        }
        // A window that was on screen a moment ago is coming back, not
        // switching in: a desktop switched away and back, an app hidden and
        // shown. Two windows of one app can share a frame without being tabs
        // -- a stack gives its members one -- and without this they would
        // trade places on the way back.
        if self.native_tabs.departures.iter().any(|d| d.window == window) {
            self.native_tabs.forget(window);
            return changed;
        }
        let departure = self
            .native_tabs
            .departures
            .iter()
            .rev()
            .find(|departure| self.takes_over(departure, window))
            .cloned();
        match departure {
            Some(departure) => self.hand_off_native_tab(departure, window) || changed,
            None => {
                self.native_tabs.arrivals.retain(|a| a.window != window);
                self.native_tabs.arrivals.push(Arrival { window, at: now });
                changed
            }
        }
    }

    /// The window server has put a window rift does not track yet on `space`.
    /// If it is a tab taking the place of one that just left, hold that tab in
    /// its tree until discovery names the newcomer.
    pub(super) fn note_untracked_native_tab(
        &mut self,
        wsid: WindowServerId,
        space: SpaceId,
        live_info: Option<&WindowServerInfo>,
    ) {
        let now = crate::sys::trace::now();
        self.prune_native_tabs(now);
        // The pid from rift's own record first: the live one is right in
        // production and invented under test.
        let Some(pid) = self
            .state
            .windows
            .get_window_server_info(wsid)
            .map(|info| info.pid)
            .or_else(|| live_info.map(|info| info.pid))
        else {
            return;
        };
        let Some(frame) = window_server::live_window_frame(wsid)
            .or_else(|| self.state.windows.get_window_server_info(wsid).map(|info| info.frame))
        else {
            return;
        };
        let Some(departure) = self
            .native_tabs
            .departures
            .iter()
            .rev()
            .find(|d| {
                d.window.pid == pid
                    && d.space == space
                    && frames_match(d.frame, frame)
                    && self.native_tabs.hold_for(d.window).is_none()
            })
            .cloned()
        else {
            return;
        };
        self.native_tabs.holds.push(Hold {
            window: departure.window,
            arriving: wsid,
            at: now,
        });
        crate::sys::trace::act(
            "native_tab",
            &(departure.window.idx.get(), wsid.as_u32(), "held for discovery"),
        );
    }

    /// The window server reports `wsid` gone from `space`. If it is a tab held
    /// for the tab replacing it, leave it in its tree and only note that it is
    /// hidden; the hand-off will take it out. Returns whether it was held.
    pub(super) fn keep_held_native_tab(&mut self, wsid: WindowServerId, space: SpaceId) -> bool {
        let Some(window) = self.state.windows.tracked_window_id(wsid) else {
            return false;
        };
        let Some(hold) = self.native_tabs.hold_for(window) else {
            return false;
        };
        let fresh = crate::sys::trace::now().duration_since(hold.at) < TAB_SWITCH_WINDOW;
        // A closed tab is gone, not hidden; the ordinary teardown runs and its
        // departure places the newcomer beside the neighbour it had.
        let hidden_but_alive = window_server::window_ordered_in(wsid) != Some(true)
            && window_server::get_window(wsid).is_some();
        if !fresh || !hidden_but_alive {
            self.native_tabs.holds.retain(|h| h.window != window);
            return false;
        }
        self.state.windows.set_window_server_space(wsid, Some(space));
        self.state.windows.mark_window_hidden(wsid);
        true
    }

    /// Discovery for `pid` is done. A hold still standing after it was not
    /// taken over -- the newcomer is not a window rift manages -- and is let
    /// go.
    pub(super) fn after_native_tab_discovery(&mut self, pid: pid_t) -> bool {
        let now = crate::sys::trace::now();
        let released: Vec<Hold> = self
            .native_tabs
            .holds
            .iter()
            .copied()
            .filter(|hold| {
                hold.window.pid == pid && now.duration_since(hold.at) >= HOLD_SURVIVES_EARLY_SWEEPS
            })
            .collect();
        self.release_native_tab_holds(released)
    }

    /// Drop matches too old to be one tab switch, and let go of expired holds.
    fn prune_native_tabs(&mut self, now: Instant) -> bool {
        self.native_tabs.prune_matches(now);
        let expired: Vec<Hold> = self
            .native_tabs
            .holds
            .iter()
            .copied()
            .filter(|hold| now.duration_since(hold.at) >= TAB_SWITCH_WINDOW)
            .collect();
        self.release_native_tab_holds(expired)
    }

    /// No tab came to take these places: take the held tabs out of their
    /// trees, as their removal would have done had it not been held.
    fn release_native_tab_holds(&mut self, holds: Vec<Hold>) -> bool {
        let mut changed = false;
        for hold in holds {
            self.native_tabs.holds.retain(|h| h.window != hold.window);
            let hidden = self
                .state
                .windows
                .window(hold.window)
                .and_then(|w| w.info.sys_id)
                .is_some_and(|wsid| !self.state.windows.is_window_visible(wsid));
            if hidden {
                crate::sys::trace::act(
                    "native_tab",
                    &(hold.window.idx.get(), hold.arriving.as_u32(), "hold released"),
                );
                self.send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(hold.window));
                changed = true;
            }
        }
        changed
    }

    /// Whether `arriving` is a tab of `departure`'s group brought forward in
    /// its place: same app, same desktop, same frame, and the tab that left
    /// is really not drawn any more.
    fn takes_over(&self, departure: &Departure, arriving: WindowId) -> bool {
        if arriving == departure.window || arriving.pid != departure.window.pid {
            return false;
        }
        let Some(state) = self.state.windows.window(arriving) else {
            return false;
        };
        let Some(wsid) = state.info.sys_id.filter(|_| state.is_admitted()) else {
            return false;
        };
        if !self.state.windows.is_window_visible(wsid) {
            return false;
        }
        let space = window_server::window_space(wsid)
            .or_else(|| self.state.windows.window_server_space(wsid));
        if space != Some(departure.space) {
            return false;
        }
        let frame = window_server::live_window_frame(wsid).unwrap_or(state.frame_monotonic);
        if !frames_match(frame, departure.frame) {
            return false;
        }
        let left_wsid = self.state.windows.window(departure.window).and_then(|w| w.info.sys_id);
        left_wsid.is_none_or(|wsid| window_server::window_ordered_in(wsid) != Some(true))
    }

    fn hand_off_native_tab(&mut self, departure: Departure, arriving: WindowId) -> bool {
        self.native_tabs.forget(departure.window);
        self.native_tabs.forget(arriving);
        self.native_tabs.holds.retain(|h| h.window != departure.window);
        // Neither has been to native fullscreen. The leaving tab's slot would
        // bring it back as a ghost beside the tab now showing; the arriving
        // tab's, left from an earlier switch, would replay a snapshot with its
        // hidden siblings in it.
        self.fullscreen_slots.forget(departure.window);
        self.fullscreen_slots.forget(arriving);
        // The group floats or tiles as one window. Recorded as the user's
        // choice so the app rules, which see a new window, leave it alone: a
        // catch-all float is exactly what untiled a tab brought forward.
        self.state
            .windows
            .set_user_floating(arriving, departure.user_floating.unwrap_or(departure.floating));
        let engine = &mut self.layout_manager.layout_engine;
        let swapped = engine.hand_off_native_tab(
            &mut self.state.windows,
            departure.space,
            departure.window,
            arriving,
        );
        let placed = swapped
            || (!departure.floating
                && departure.workspace.is_some_and(|workspace| {
                    engine.place_native_tab(
                        &mut self.state.windows,
                        departure.space,
                        workspace,
                        arriving,
                        departure.slot,
                    )
                }));
        info!(
            left = ?departure.window,
            ?arriving,
            floating = departure.floating,
            swapped,
            placed,
            "Native tab switch: the tab brought forward takes the place of the one left"
        );
        crate::sys::trace::act(
            "native_tab",
            &(
                departure.window.idx.get(),
                arriving.idx.get(),
                match (swapped, placed) {
                    (true, _) => "swapped in",
                    (false, true) => "placed beside the old neighbour",
                    (false, false) => "nothing to hand off",
                },
                departure.floating,
            ),
        );
        placed
    }
}
