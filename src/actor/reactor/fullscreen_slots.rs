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
}

impl FullscreenSlots {
    pub(super) fn forget(&mut self, window: WindowId) { self.slots.remove(&window); }
}

impl Reactor {
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
        let snapshot = match engine.snapshot_current_layout_lightly(&self.state.windows) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                debug!(?window, %error, "Could not snapshot the layout for a fullscreen slot");
                return;
            }
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
        self.fullscreen_slots
            .slots
            .insert(window, FullscreenSlot { space, snapshot, anchor });
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
