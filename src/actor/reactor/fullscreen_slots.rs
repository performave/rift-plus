//! Puts a window back where it was after native fullscreen.
//!
//! Native fullscreen moves a window to a space of its own, and rift takes it
//! out of its tree; the siblings reflow. When it comes back, it used to be
//! appended after the selection, somewhere else, with the old split ratios
//! gone. This remembers the window's slot on the way out and reinstates it on
//! the way back:
//!
//! - the layout is snapshotted *before* the window leaves;
//! - on exit, that layout is restored outright — same structure, nesting and
//!   ratios — for every window the snapshot still finds; windows that arrived
//!   meanwhile keep the places they have;
//! - only if the snapshot matches nothing is the window re-inserted next to
//!   its old neighbour on the same side with its old share. This is a last
//!   resort, not an equal: splitting a neighbour that sits in a stack puts
//!   the window *into* the stack, on top of it.
//!
//! The slot is read from the addition that puts the window back in the tree,
//! not from the event that announces the return. The window server says the
//! window is home again before it says the display has left the fullscreen
//! space, so the restoration that reads the exit is skipped for a space that
//! is not active yet, and the window is added back a moment later by whichever
//! path notices it next. Hanging the slot off the addition means every one of
//! those paths puts the window back where it was.

use std::time::{Duration, Instant};

/// How long after the window server last moved windows for a display change a
/// window found on another display's desktop than its slot's was carried
/// there by that change, rather than moved by the user.
const SLOT_SEND_BACK_AFTER_CHURN: Duration = Duration::from_secs(10);

use tracing::{debug, info, warn};

use super::{LayoutEvent, Reactor};
use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::layout_engine::{RestoreRequest, RestoreScope, RestoreSource, Slot};
use crate::sys::screen::SpaceId;

#[derive(Default)]
pub(super) struct FullscreenSlots {
    slots: HashMap<WindowId, FullscreenSlot>,
}

struct FullscreenSlot {
    space: SpaceId,
    /// The engine as it was with the window still in its tree.
    snapshot: String,
    anchor: Option<Slot>,
    /// When this reading was taken, to tell a slot from before a display
    /// churn from one taken while the window server was still shuffling.
    taken: Instant,
}

impl FullscreenSlots {
    pub(super) fn forget(&mut self, window: WindowId) { self.slots.remove(&window); }

    /// A desktop came back under a new id. A slot keyed on the old one is
    /// waiting for a window that will now come home to the new one; left
    /// alone, the window's return did not match it and the slot went unused.
    pub(super) fn remap_space(&mut self, old: SpaceId, new: SpaceId) {
        for slot in self.slots.values_mut() {
            if slot.space == old {
                slot.space = new;
            }
        }
    }
}

impl Reactor {
    /// Move everything keyed on desktop `old` onto `new`: the layout engine's
    /// state and the fullscreen slots waiting for a window to come back to it.
    /// Every remap goes through here, so neither can be missed.
    pub(super) fn remap_space_state(&mut self, old: SpaceId, new: SpaceId) {
        self.layout_manager.layout_engine.remap_space(&mut self.state.windows, old, new);
        self.fullscreen_slots.remap_space(old, new);
    }

    /// Called from the layout-event sink just before the removal that takes
    /// a window entering native fullscreen out of its tree.
    pub(super) fn record_fullscreen_slot(&mut self, window: WindowId) {
        // The first removal is the only truthful one. A native fullscreen
        // transition churns the tree — the window is taken out, put back by a
        // path that does not know where it is going, and taken out again — and
        // by the second removal it sits wherever that path dropped it: a window
        // tiled on the left records itself as the right-hand one. Later
        // removals find the work already done.
        let Some(space) = self.space_of_tiled_window(window) else {
            crate::sys::trace::act(
                "fullscreen_slot",
                &(window.idx.get(), "not tiled; not recorded"),
            );
            return;
        };
        // A slot for another space is a leftover: the window was put back
        // there by a path that never consulted it, and it would only be
        // dropped on the way back as "landed elsewhere". Its truth is gone;
        // this removal's is the one to keep.
        if let Some(existing) = self.fullscreen_slots.slots.get(&window) {
            if existing.space == space {
                return;
            }
            // Unless a display change is still settling. Then the window
            // server is moving windows between spaces itself and the trees
            // follow it one space at a time, so which space holds the window
            // answers differently from one millisecond to the next; the
            // reading from before it started is the one that means anything.
            // Replacing it walked a window's slot off its own desktop, onto
            // the departed display's, and from there onto whichever surviving
            // desktop sorted first — and the restore then built a tile for it
            // on a desktop it had never been on.
            if let Some(churn_began) = self.display_archive.churn_began()
                && existing.taken < churn_began
            {
                crate::sys::trace::act(
                    "fullscreen_slot",
                    &(
                        window.idx.get(),
                        "churn settling; slot kept",
                        existing.space.get(),
                        space.get(),
                    ),
                );
                return;
            }
            // Nor while rift itself is sending the window to the slot's own
            // desktop. Aftercare sends a window macOS dropped elsewhere back
            // home, and its leaving the desktop it was dropped on says nothing
            // about where it belongs -- recorded, it replaced the slot it was
            // on its way back to, and the window came home to none and was
            // appended at the end.
            if self.display_archive.homing_destination(window) == Some(existing.space) {
                crate::sys::trace::act(
                    "fullscreen_slot",
                    &(
                        window.idx.get(),
                        "going home; slot kept",
                        existing.space.get(),
                        space.get(),
                    ),
                );
                return;
            }
            crate::sys::trace::act(
                "fullscreen_slot",
                &(
                    window.idx.get(),
                    "stale slot replaced",
                    existing.space.get(),
                    space.get(),
                ),
            );
            self.fullscreen_slots.slots.remove(&window);
        }
        let engine = &mut self.layout_manager.layout_engine;
        if engine.is_window_floating(window) {
            // A float comes back as a float; its frame is kept elsewhere.
            return;
        }
        let anchor = engine.slot_of(space, window);
        // While a display change is settling, the tree this removal can see is
        // already the wrong one. The window server moves windows between
        // desktops one at a time, so by the second removal the snapshot no
        // longer has the first window in it, by the third it has neither, and
        // every one of them is restored on the way back — last one wins, over a
        // tree its predecessors had already been cut out of. A stack came back
        // with its members in the order they happened to return, and one whose
        // container emptied did not come back at all. The pre-churn snapshot is
        // the same tree for all of them: taken once, before the first window
        // left, which is the arrangement the user is owed.
        let pre_churn = self.display_archive.fresh_pre_churn().map(|pre| pre.layout.clone());
        let engine = &mut self.layout_manager.layout_engine;
        let snapshot = match pre_churn {
            Some(layout) => layout,
            None => match engine.snapshot_current_layout_lightly(&self.state.windows) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    debug!(?window, %error, "Could not snapshot the layout for a fullscreen slot");
                    return;
                }
            },
        };
        crate::sys::trace::act(
            "fullscreen_slot",
            &(
                window.idx.get(),
                "recorded",
                space.get(),
                anchor.map(|slot| slot.anchor.idx.get()),
            ),
        );
        self.fullscreen_slots.slots.insert(window, FullscreenSlot {
            space,
            snapshot,
            anchor,
            taken: crate::sys::trace::now(),
        });
        // Taken as a display change carries the window to another display's
        // desktop, the slot's own desktop is where it belongs; nothing later
        // may ask for it to be put back, since a window that stays visible
        // is never "ordered in" again.
        self.send_back_to_slot_display(window, space);
    }

    /// The display a desktop is on: the screen showing it, else the display
    /// the window server lists it under.
    fn display_of_space(&self, space: SpaceId) -> Option<String> {
        self.space_state
            .screens
            .iter()
            .find(|screen| screen.space == Some(space))
            .map(|screen| screen.display_uuid.clone())
            .or_else(|| {
                self.space_state
                    .display_space_ids
                    .iter()
                    .find(|(_, spaces)| spaces.contains(&space))
                    .map(|(uuid, _)| uuid.clone())
            })
    }

    /// A window whose slot waits on `space` that the window server has put on
    /// another display's desktop while moving windows for a display change.
    /// As the external became main, a TextEdit whose frame now lay in the
    /// other display's part of the arrangement was moved to that display's
    /// desktop; the slot waited on its own, and every restore failed to place
    /// it because it was not there. Send it back, as aftercare does after a
    /// return, and count it as on its way home so its leaving the other
    /// desktop does not replace the slot.
    fn send_back_to_slot_display(&mut self, window: WindowId, space: SpaceId) {
        let Some(since_churn) = crate::sys::display_churn::since_windows_last_moved()
            .filter(|since| *since < SLOT_SEND_BACK_AFTER_CHURN)
        else {
            return;
        };
        // A drop or a command since the change may have put the window there
        // on purpose; only a move the display change made is undone.
        if self.last_user_input.is_some_and(|input| input.elapsed() < since_churn) {
            return;
        }
        // Not for a window rift itself is moving, nor while a departure's
        // record stands or any record's pass runs: an unplug and the return
        // after it move windows between displays on purpose, and those passes
        // own them. An arrival's record once its pass is done is different --
        // it is the repair of what the arrival scrambled, and macOS carrying a
        // window it remembers onto the new display a beat after that pass is
        // more of the same scramble (`plain-replug`: a laptop window moved to
        // the external at the first plug, and every unplug after made it a
        // stand-in desktop of its own). A desktop macOS handed the new display
        // whole goes with its windows, so the slot is on that display too and
        // nothing is sent.
        if self
            .display_archive
            .record
            .as_ref()
            .is_some_and(|record| !record.is_arrival() || !record.destination_free())
            || self.display_archive.homing_destination(window).is_some()
        {
            return;
        }
        let Some(wsid) = self.state.windows.window(window).and_then(|w| w.info.sys_id) else {
            return;
        };
        let Some(now) = crate::sys::window_server::window_space(wsid) else {
            return;
        };
        if now == space {
            return;
        }
        let (here, there) = (self.display_of_space(space), self.display_of_space(now));
        if here.is_none() || there.is_none() || here == there {
            return;
        }
        if !crate::sys::scripting_addition::is_available() {
            return;
        }
        if crate::sys::scripting_addition::move_window_to_space(wsid.as_u32(), space.get()) {
            self.note_window_sent_to_space(wsid);
            self.display_archive.kept_off.insert(window, (space, crate::sys::trace::now()));
            info!(
                ?window,
                from = now.get(),
                home = space.get(),
                "A display change carried a window to another display; sent it back to its slot"
            );
            crate::sys::trace::act(
                "fullscreen_slot",
                &(
                    window.idx.get(),
                    "carried across; sent back",
                    now.get(),
                    space.get(),
                ),
            );
        }
    }

    /// Which space's tree is holding `window` right now.
    ///
    /// Not the workspace assignment: a native fullscreen transition clears that
    /// before the removals that need it, so asking the assignment answers
    /// `None` exactly when a slot most needs recording. The tree still has the
    /// window at that point, so ask the tree, and keep the assignment only as
    /// the fast path.
    fn space_of_tiled_window(&self, window: WindowId) -> Option<SpaceId> {
        let engine = &self.layout_manager.layout_engine;
        if let Some(space) = self.assigned_space_for_window_id(window)
            && engine.is_window_tiled(space, window)
        {
            return Some(space);
        }
        engine
            .virtual_workspace_manager()
            .initialized_spaces()
            .into_iter()
            .find(|space| engine.is_window_tiled(*space, window))
    }

    /// The window is back on `space`. Adding it to the layout is what
    /// reinstates its slot, in `reinstate_fullscreen_slot`; returns whether
    /// the active layout changed, like
    /// `restore_window_to_active_layout_if_visible`.
    pub(super) fn restore_window_to_layout_after_fullscreen(
        &mut self,
        window: WindowId,
        space: SpaceId,
    ) -> bool {
        self.restore_window_to_active_layout_if_visible(window, space)
    }

    /// The window server has just ordered `window` in. If it is on its way
    /// back from native fullscreen and nothing has put it in its tree yet,
    /// this is the moment.
    ///
    /// Leaving fullscreen, the window server orders the window out, moves it
    /// to its user space, moves the display there, and only then orders it
    /// back in. An inventory taken in between leaves the window out — an AX
    /// window without an on-screen peer — and the space change that would
    /// have re-added it has come and gone. The order-in is the last event
    /// that mentions the window, so it has to do the adding; a window with no
    /// slot waiting is an ordinary order-in and is left alone.
    pub(super) fn restore_ordered_in_window_after_fullscreen(&mut self, window: WindowId) -> bool {
        let Some(space) = self
            .fullscreen_slots_awaiting_insertion()
            .into_iter()
            .find_map(|(waiting, space)| (waiting == window).then_some(space))
        else {
            return false;
        };
        if !self.is_space_active(space) {
            return false;
        }
        // Ask the window server where the window is, not the active spaces.
        // Switching desktops orders the new desktop's windows in before rift
        // hears that the display changed, so the desktop being left still
        // counts as active: switching to a window's own desktop matched a slot
        // it had left on the other one, inserted it into that tree, and
        // replayed the slot's old snapshot -- two tiles swapped on every
        // switch. A window ordered in on another desktop is not coming home
        // here. If it lives in that desktop's tree and nothing is sending it
        // home, the slot is left over from a move, and would fire again at
        // every switch; let it go.
        if let Some(wsid) = self.state.windows.window(window).and_then(|w| w.info.sys_id)
            && let Some(now) = crate::sys::window_server::window_space(wsid)
            && now != space
            && crate::sys::window_server::space_is_user(now.get())
        {
            let stale = self.layout_manager.layout_engine.is_window_tiled(now, window)
                && self.display_archive.homing_destination(window) != Some(space);
            if stale {
                self.fullscreen_slots.forget(window);
            }
            crate::sys::trace::act(
                "fullscreen_slot",
                &(
                    window.idx.get(),
                    if stale {
                        "ordered in elsewhere; stale slot dropped"
                    } else {
                        "ordered in elsewhere; not restoring"
                    },
                    space.get(),
                    now.get(),
                ),
            );
            return false;
        }
        crate::sys::trace::act(
            "fullscreen_slot",
            &(window.idx.get(), "ordered in; restoring", space.get()),
        );
        self.restore_window_to_layout_after_fullscreen(window, space)
    }

    /// The slots whose window is not in its tree yet. Asked before a layout
    /// event is applied; whichever of these windows is tiled afterwards was
    /// put there by that event, whatever kind of event it was. Discovery
    /// reconciles an app's windows from the store rather than from its own
    /// payload, so the event that inserts a returning window need not so much
    /// as name it — this is the only test that catches every path.
    pub(super) fn fullscreen_slots_awaiting_insertion(&self) -> Vec<(WindowId, SpaceId)> {
        let engine = &self.layout_manager.layout_engine;
        self.fullscreen_slots
            .slots
            .iter()
            .filter(|(window, slot)| !engine.is_window_tiled(slot.space, **window))
            .map(|(window, slot)| (*window, slot.space))
            .collect()
    }

    /// `window` has just been added back to the tree on `space`. Move it to
    /// the slot it left, and report whether the layout changed.
    pub(super) fn reinstate_fullscreen_slot(&mut self, window: WindowId, space: SpaceId) -> bool {
        let Some(slot) = self.fullscreen_slots.slots.remove(&window) else {
            return false;
        };
        if slot.space != space {
            debug!(?window, "Fullscreen exit landed on another space; slot dropped");
            crate::sys::trace::act(
                "fullscreen_slot",
                &(
                    window.idx.get(),
                    "landed elsewhere; dropped",
                    slot.space.get(),
                    space.get(),
                ),
            );
            return false;
        }

        // A snapshot from before the user's last layout command would undo
        // that command: a slot recorded at a plug, restored after Safari had
        // been made fullscreen, put Safari back in its tile. Put the window
        // back beside its old neighbour instead, and leave the rest alone.
        let older_than_a_command =
            self.last_layout_command.is_some_and(|commanded| commanded > slot.taken);
        if older_than_a_command {
            crate::sys::trace::act(
                "fullscreen_slot",
                &(
                    window.idx.get(),
                    "snapshot predates a layout command; re-anchoring",
                ),
            );
            if let Some(anchor) = slot.anchor
                && anchor.anchor != window
                && self.layout_manager.layout_engine.restore_slot(space, anchor, window)
            {
                crate::sys::trace::act("fullscreen_slot", &(window.idx.get(), "re-anchored"));
                return true;
            }
            return false;
        }
        let request = RestoreRequest {
            scope: RestoreScope::Workspace,
            active_space: space,
            source: RestoreSource::CurrentSpace,
            from_space: None,
        };
        let layout_settings = self.config.settings.layout.clone();
        match self.layout_manager.layout_engine.restore_layout_from_snapshot(
            &slot.snapshot,
            request,
            &mut self.state.windows,
            &layout_settings,
        ) {
            // A restore that put the others back and not this window leaves
            // it where it was: the "ordered in" report that asked for it can
            // come while the window is still in another desktop's tree, sent
            // home and not yet arrived. Used up then, the slot was gone when
            // the window did arrive a moment later, and it was inserted below
            // its old neighbour instead of above. Keep it for the arrival.
            Ok(report)
                if report.matched > 0
                    && !self.layout_manager.layout_engine.is_window_tiled(space, window) =>
            {
                crate::sys::trace::act(
                    "fullscreen_slot",
                    &(
                        window.idx.get(),
                        "restored without it; slot kept",
                        report.matched,
                    ),
                );
                self.fullscreen_slots.slots.insert(window, slot);
                self.send_back_to_slot_display(window, space);
                return false;
            }
            Ok(report) if report.matched > 0 => {
                info!(
                    ?window,
                    matched = report.matched,
                    "Fullscreen exit: layout put back as it was"
                );
                crate::sys::trace::act(
                    "fullscreen_slot",
                    &(window.idx.get(), "restored", report.matched),
                );
                return true;
            }
            Ok(report) => debug!(
                ?window,
                ?report,
                "Fullscreen slot snapshot matched nothing; re-anchoring"
            ),
            Err(error) => {
                warn!(?window, %error, "Fullscreen slot snapshot could not be restored; re-anchoring")
            }
        }

        if let Some(anchor) = slot.anchor
            && anchor.anchor != window
            && self.layout_manager.layout_engine.restore_slot(space, anchor, window)
        {
            info!(?window, anchor = ?anchor.anchor, side = ?anchor.side, "Fullscreen exit: window put back beside its old neighbour");
            crate::sys::trace::act("fullscreen_slot", &(window.idx.get(), "re-anchored"));
            return true;
        }
        crate::sys::trace::act("fullscreen_slot", &(window.idx.get(), "nothing matched"));
        false
    }

    pub(super) fn note_fullscreen_slot_lifecycle(&mut self, event: &LayoutEvent) {
        match event {
            LayoutEvent::WindowRemovedPreserveFloating(window) => {
                self.record_fullscreen_slot(*window)
            }
            LayoutEvent::WindowRemoved(window) => {
                // A plain removal is not always the end of the window. Several
                // paths take a window out of the tree for a moment —
                // reconciliation, the visibility restore, discovery — and a
                // native fullscreen transition trips them before rift ever
                // hears about the fullscreen space. The window is coming back,
                // and whichever path puts it back drops it beside the
                // selection, so this is the last moment its real place can be
                // read. Record from here too; only a window the store has let
                // go of has no place worth keeping. A window in the user's hand
                // is the exception: its next place is wherever the drop says.
                if !self.state.windows.contains_window(*window) {
                    self.fullscreen_slots.forget(*window);
                } else if self.window_in_drag() != Some(*window) {
                    self.record_fullscreen_slot(*window);
                }
            }
            _ => {}
        }
    }
}
