use objc2_core_foundation::{CGPoint, CGRect};
use rift_protocol::StackInfo;
use tracing::trace;

use super::replay::Record;
use super::{AppState, Event, WorkspaceSwitchOrigin, WorkspaceSwitchState};
use crate::actor;
use crate::actor::app::{AppThreadHandle, WindowId, WindowInventoryToken, pid_t};
use crate::actor::drag_swap::DragManager as DragSwapManager;
use crate::actor::reactor::Reactor;
use crate::actor::reactor::animation::AnimationManager;
use crate::actor::spaces::ForwardedSpaceState;
use crate::actor::{input, menu_bar, raise_manager, stack_line, window_notify, wm_controller};
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::{LayoutMode, WindowSnappingSettings};
use crate::layout_engine::LayoutEngine;
use crate::model::broadcast::{BroadcastEvent, BroadcastSender, protocol_workspace_id};
use crate::sys::screen::{ScreenInfo, SpaceId};

/// Manages application state and rules
pub struct AppManager {
    pub apps: HashMap<pid_t, AppState>,
}

impl AppManager {
    pub fn new() -> Self { AppManager { apps: HashMap::default() } }

    pub fn reject_duplicate(&self, pid: pid_t, handle: &AppThreadHandle) -> bool {
        let Some(existing) = self.apps.get(&pid) else {
            return false;
        };
        tracing::error!(pid, "Duplicate app actor registration; retaining original actor");
        if !existing.handle.same_actor(handle) {
            _ = handle.send(crate::actor::app::Request::Terminate);
        }
        true
    }
}

/// Manages drag operations and window swapping
pub struct DragManager {
    pub drag_state: super::DragState,
    pub drag_swap_manager: DragSwapManager,
    pub skip_layout_for_window: Option<WindowId>,
    /// Whether the drop overlay is on screen. It belongs to a pending drop and
    /// is taken down as soon as there is none, whichever way the drag ended.
    pub drop_overlay_shown: bool,
    /// A drop zone the pointer has entered but not yet dwelt in. Near a zone
    /// boundary successive pointer samples land on either side of it, and
    /// following each one recomputed the preview and re-aimed the overlay
    /// dozens of times a second — the flashing. A zone change is believed
    /// only once the pointer has stayed in the new zone for a beat.
    pub zone_candidate: Option<ZoneCandidate>,
    /// The last preview the overlay was aimed at, keyed by what produced it.
    /// Working out an insert region means laying out a copy of the tree,
    /// which is far too expensive per pointer move; the region only changes
    /// when the (dragged, target, action) triple does, so it is computed once
    /// per triple and the overlay is only re-aimed when the answer changes.
    pub drop_preview_cache: Option<DropPreview>,
    /// The space a just-dropped window belongs to, held until its frame has
    /// landed there. See `DropPin`.
    pub drop_pin: Option<DropPin>,
    /// A window whose frame the app reported moving with the button down,
    /// until the button comes up. Kept apart from the drag session, which
    /// only exists for frame reports rift accepted: a report that trails
    /// rift's own last write is discarded, and a drag that starts right
    /// after a re-tile would otherwise be invisible. See
    /// `Reactor::window_in_drag`.
    pub held_window: Option<WindowId>,
    /// A floating window whose own move/resize notifications are silenced
    /// while the user drags it. See `Request::SetDragNotificationSilence`.
    pub notifications_silenced: Option<WindowId>,
    /// Throttle for keeping a dragged float's space membership matched to
    /// the display under it. See `Reactor::sync_dragged_float_space`.
    pub space_sync_at: Option<std::time::Instant>,
    /// A seam-straddling drop rift finished with a write. The write races
    /// the system's own relocation of the straddling window; whichever
    /// lands second wins. Re-asserted once the dust settles — a frame fully
    /// on one display is never relocated, so the re-assert sticks.
    pub seam_finish: Option<SeamFinish>,
}

/// See `DragManager::seam_finish`.
#[derive(Debug, Clone, Copy)]
pub struct SeamFinish {
    pub window: WindowId,
    /// Where the user let go. macOS lets a server-side drag rest overhanging
    /// the seam (clipped at its display's edge), so a straddling drop is
    /// only *watched*: rift steps in solely when the system or the app
    /// relocates the window away from this.
    pub dropped_at: objc2_core_foundation::CGRect,
    /// Where the pointer let go, for choosing the landing display if a
    /// correction becomes necessary.
    pub pointer: Option<objc2_core_foundation::CGPoint>,
    /// The correction rift wrote, once it decided one was needed.
    pub fitted: Option<objc2_core_foundation::CGRect>,
    pub at: std::time::Instant,
    pub attempts: u8,
}

impl SeamFinish {
    /// How long after the drop the placement is checked and re-asserted.
    pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);
    /// How far the window may drift from the drop before rift reads it as
    /// relocated rather than settled.
    pub const TOLERANCE: f64 = 12.0;
}

/// See `DragManager::zone_candidate`.
#[derive(Debug, Clone, Copy)]
pub struct ZoneCandidate {
    pub dragged: WindowId,
    pub target: WindowId,
    pub action: crate::actor::drag_swap::DropAction,
    pub since: std::time::Instant,
}

impl ZoneCandidate {
    /// How long the pointer has to stay in a new zone before the preview
    /// follows it there.
    pub const DWELL: std::time::Duration = std::time::Duration::from_millis(150);
}

/// See `DragManager::drop_preview_cache`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropPreview {
    pub dragged: WindowId,
    pub target: WindowId,
    pub action: crate::actor::drag_swap::DropAction,
    pub screen: CGRect,
    pub region: CGRect,
}

/// A window dropped on a target hanging over a display seam has, as far as the
/// window server is concerned, already moved to the display it hangs over: the
/// report of that arrives a beat after the drop, while the arrange that pulls
/// the window back onto the target's display is still in flight. Taken at
/// face value, that report tore the window out of the tree it was just
/// dropped into and tiled it on the other display; the arranges of the two
/// displays then fought, and the wrong one won. The pin says which space the
/// window is in until the frame write has landed, so a native-space report
/// that disagrees is treated as the stale observation it is.
#[derive(Debug, Clone, Copy)]
pub struct DropPin {
    pub window: crate::sys::window_server::WindowServerId,
    pub space: SpaceId,
    pub until: std::time::Instant,
    /// When the window server may next be asked whether the write landed.
    /// The pin is checked after *every* event; asking live on each of the
    /// ~40 pointer moves a second kept a query storm running for the whole
    /// hold after every drop.
    pub next_probe: std::time::Instant,
}

impl DropPin {
    /// How long a frame write is given to land before the window server is
    /// believed again.
    pub const HOLD: std::time::Duration = std::time::Duration::from_millis(1500);
    /// How often the window server is asked whether it agrees yet.
    pub const PROBE_EVERY: std::time::Duration = std::time::Duration::from_millis(200);
}

impl DragManager {
    pub fn reset(&mut self) { self.drag_swap_manager.reset(); }

    pub fn last_target(&self) -> Option<WindowId> { self.drag_swap_manager.last_target() }

    pub fn dragged(&self) -> Option<WindowId> { self.drag_swap_manager.dragged() }

    pub fn origin_frame(&self) -> Option<CGRect> { self.drag_swap_manager.origin_frame() }

    pub fn update_config(&mut self, config: WindowSnappingSettings) {
        self.drag_swap_manager.update_config(config);
    }
}

/// Manages window notifications
pub struct NotificationManager {
    pub last_sls_notification_ids: Vec<u32>,
    pub last_layout_modes_by_space: HashMap<SpaceId, crate::common::config::LayoutMode>,
    pub _window_notify_tx: Option<window_notify::Sender>,
}

/// Manages menu state and interactions
pub struct MenuManager {
    pub menu_state: super::MenuState,
    pub menu_tx: Option<menu_bar::Sender>,
}

/// Manages Mission Control state
pub struct MissionControlManager {
    pub mission_control_state: super::MissionControlState,
}

/// Owns ordering and coalescing for asynchronous AX window inventories.
pub struct WindowInventoryManager {
    pub topology_revision: u64,
    pub next_request_id: u64,
    pub in_flight: HashMap<pid_t, WindowInventoryToken>,
    pub pending: HashSet<pid_t>,
    pub refocus_after_refresh: HashMap<pid_t, WindowId>,
}

/// Manages workspace switching state
pub struct WorkspaceSwitchManager {
    pub workspace_switch_state: super::WorkspaceSwitchState,
    pub workspace_switch_generation: u64,
    pub active_workspace_switch: Option<u64>,
    pub pending_workspace_switch_origin: Option<WorkspaceSwitchOrigin>,
    pub pending_workspace_mouse_warp: Option<WindowId>,
}

impl WorkspaceSwitchManager {
    pub fn start_workspace_switch(&mut self, origin: WorkspaceSwitchOrigin) {
        self.workspace_switch_generation = self.workspace_switch_generation.wrapping_add(1);
        self.active_workspace_switch = Some(self.workspace_switch_generation);
        self.workspace_switch_state = WorkspaceSwitchState::Active;
        self.pending_workspace_switch_origin = Some(origin);
    }

    pub fn manual_switch_in_progress(&self) -> bool {
        self.workspace_switch_state == WorkspaceSwitchState::Active
            && self.pending_workspace_switch_origin == Some(WorkspaceSwitchOrigin::Manual)
    }

    pub fn mark_workspace_switch_inactive(&mut self) {
        self.workspace_switch_state = WorkspaceSwitchState::Inactive;
        self.pending_workspace_switch_origin = None;
    }
}

/// Manages refocus and cleanup state
pub struct RefocusManager {
    pub stale_cleanup_state: super::StaleCleanupState,
    pub refocus_state: super::RefocusState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshQuarantineState {
    Ready,
    Sleeping,
    SessionInactive,
    DisplayChurn,
}

pub struct RefreshQuarantineManager {
    pub sleeping: bool,
    pub session_inactive: bool,
    pub display_churn_active: bool,
    pub awaiting_post_wake_snapshot: bool,
    pub awaiting_post_session_snapshot: bool,
    pub pending_inventory_refresh: bool,
    /// LoginWindow/AppKit can replay application activations while restoring a
    /// session. Those activations are not user intent and must not drive a
    /// virtual-workspace switch. Explicit input clears this latch.
    pub suppress_auto_workspace_switch_until_input: bool,
}

impl RefreshQuarantineManager {
    pub fn state(&self) -> RefreshQuarantineState {
        if self.sleeping {
            RefreshQuarantineState::Sleeping
        } else if self.session_inactive {
            RefreshQuarantineState::SessionInactive
        } else if self.display_churn_active {
            RefreshQuarantineState::DisplayChurn
        } else {
            RefreshQuarantineState::Ready
        }
    }

    pub fn blocks_refreshes(&self) -> bool { self.state() != RefreshQuarantineState::Ready }
}

/// Manages communication channels to other actors
pub struct CommunicationManager {
    pub input_tx: Option<input::Sender>,
    pub stack_line_tx: Option<stack_line::Sender>,
    pub drop_overlay_tx: Option<crate::actor::drop_overlay::Sender>,
    pub tile_halo_tx: Option<crate::actor::tile_halo::Sender>,
    pub raise_manager_tx: raise_manager::Sender,
    pub event_broadcaster: BroadcastSender,
    pub wm_sender: Option<wm_controller::Sender>,
    pub events_tx: Option<actor::Sender<Event>>,
}

/// Manages recording state
pub struct RecordingManager {
    pub record: Record,
}

/// Manages layout engine state
pub struct LayoutManager {
    pub layout_engine: LayoutEngine,
}

pub type LayoutResult = Vec<(SpaceId, Vec<(WindowId, CGRect)>)>;

fn bound_frame_to_screen(frame: CGRect, screen: CGRect) -> CGRect {
    const WINDOW_HIDDEN_THRESHOLD: f64 = 10.0;

    let screen_left = screen.origin.x;
    let screen_top = screen.origin.y;
    let screen_right = screen.max().x;
    let screen_bottom = screen.max().y;
    let max_y = (screen_bottom - frame.size.height).max(screen_top);
    let x = if frame.max().x <= screen_left {
        screen_left - frame.size.width + WINDOW_HIDDEN_THRESHOLD
    } else if frame.origin.x >= screen_right {
        screen_right - WINDOW_HIDDEN_THRESHOLD
    } else {
        frame.origin.x
    };

    CGRect::new(
        CGPoint::new(x, frame.origin.y.clamp(screen_top, max_y)),
        frame.size,
    )
}

fn bound_scrolling_tiled_frames_to_screen(
    reactor: &Reactor,
    layout: &mut Vec<(WindowId, CGRect)>,
    screen: CGRect,
    active_workspace_windows: &HashSet<WindowId>,
) {
    for (wid, frame) in layout.iter_mut() {
        if !active_workspace_windows.contains(wid)
            || reactor.layout_manager.layout_engine.is_window_floating(*wid)
        {
            continue;
        }
        *frame = bound_frame_to_screen(*frame, screen);
    }
}

impl LayoutManager {
    pub fn update_layout(
        reactor: &mut Reactor,
        is_resize: bool,
        is_workspace_switch: bool,
        space_scope: Option<SpaceId>,
    ) -> Result<bool, crate::model::reactor::ReactorError> {
        let layout_result = Self::calculate_layout(reactor, space_scope);
        Self::apply_layout(reactor, layout_result, is_resize, is_workspace_switch)
    }

    /// Every desktop this pass arranges, each paired with the screen it is
    /// arranged for and whether that screen is the one showing it.
    ///
    /// In ordinary running that is the desktop each live display shows, and
    /// nothing else: a desktop nobody is looking at is left alone until the
    /// user switches to it, which exposes it and arranges it then.
    ///
    /// A departing display breaks that. macOS hands its desktops to a
    /// survivor with their trees intact — which is the whole of what
    /// `displaced_windows = "spaces"` promises — but the survivor goes on
    /// showing its own desktop, so the adopted ones are never arranged for
    /// the screen they now live on. Their windows keep the frames the
    /// geometry of a display that no longer exists gave them: side by side
    /// off the edge of the world, or piled on top of each other at
    /// byte-identical coordinates, and rift still counts them tiled.
    ///
    /// The engine's own display record is what tells an adopted desktop from
    /// one the survivor always had. `prune_display_state` drops the record
    /// for every display that left, so a desktop a live display owns now and
    /// the engine has no display for is exactly one that came across.
    /// Arranging it writes the record back, so each such desktop is caught up
    /// once and the pass narrows to the shown desktops again.
    fn spaces_to_arrange(
        reactor: &Reactor,
        screens: &[ScreenInfo],
    ) -> Vec<(SpaceId, ScreenInfo, bool)> {
        let mut arrange = Vec::new();
        for screen in screens {
            if let Some(space) = screen.space {
                arrange.push((space, screen.clone(), true));
            }
            let Some(uuid) = screen.display_uuid_opt() else {
                continue;
            };
            let Some(owned) = reactor.space_state.display_space_ids.get(uuid) else {
                continue;
            };
            for space in owned.iter().copied() {
                if screen.space == Some(space) {
                    continue;
                }
                let engine = &reactor.layout_manager.layout_engine;
                // A desktop the engine already places on a display was
                // arranged for that display's screen and needs nothing; one
                // it has never exposed has no tree to arrange.
                if engine.display_uuid_for_space(space).is_some()
                    || !engine.has_active_layout(space)
                {
                    continue;
                }
                arrange.push((space, screen.clone(), false));
            }
        }
        arrange
    }

    fn calculate_layout(reactor: &mut Reactor, space_scope: Option<SpaceId>) -> LayoutResult {
        if reactor.state.windows.tracked_window_count() == 0 {
            return LayoutResult::new();
        }
        let screens = reactor.space_state.screens.clone();
        let all_screen_frames: Vec<CGRect> = screens.iter().map(|s| s.frame).collect();
        let active_space_count = screens
            .iter()
            .filter_map(|screen| screen.space)
            .filter(|space| reactor.is_space_active(*space))
            .count();
        let mut layout_result = LayoutResult::new();

        for (space, screen, shown) in Self::spaces_to_arrange(reactor, &screens) {
            if space_scope.is_some_and(|scope| scope != space) {
                continue;
            }
            if shown && !reactor.is_space_active(space) {
                crate::sys::trace::act(
                    "arrange_calc",
                    &serde_json::json!({
                        "space": space, "skipped": "space is not active",
                    }),
                );
                continue;
            }
            let display_uuid_opt = screen.display_uuid_owned();
            let gaps = reactor
                .config
                .settings
                .layout
                .gaps
                .effective_for_display(display_uuid_opt.as_deref());
            reactor
                .layout_manager
                .layout_engine
                .update_space_display(space, display_uuid_opt.clone());
            // Floats are laid out where the window server says they are.
            // Rift's own record lags for apps that do not report their main
            // window moving (Lightroom), and laying a float out at a stale
            // record wrote it back to wherever rift last saw it.
            reactor.refresh_floating_frames_from_window_server();
            let mut layout =
                reactor.layout_manager.layout_engine.calculate_layout_with_virtual_workspaces(
                    &reactor.state.windows,
                    space,
                    screen.frame.clone(),
                    &gaps,
                    reactor.config.settings.ui.stack_line.thickness(),
                    reactor.config.settings.ui.stack_line.horiz_placement,
                    reactor.config.settings.ui.stack_line.vert_placement,
                    |wid| reactor.state.windows.window(wid).map(|w| w.frame_monotonic),
                    &all_screen_frames,
                );
            if active_space_count > 1
                && reactor.layout_manager.layout_engine.active_layout_mode_at(space)
                    == LayoutMode::Scrolling
            {
                let active_workspace_windows: HashSet<WindowId> = reactor
                    .layout_manager
                    .layout_engine
                    .windows_in_active_workspace(&reactor.state.windows, space)
                    .into_iter()
                    .collect();
                bound_scrolling_tiled_frames_to_screen(
                    reactor,
                    &mut layout,
                    screen.frame,
                    &active_workspace_windows,
                );
            }
            crate::sys::trace::act(
                "arrange_calc",
                &serde_json::json!({
                    "space": space,
                    "frames": layout.iter().map(|(wid, f)| {
                        serde_json::json!([wid.idx.get(), f.origin.x.round(), f.size.width.round()])
                    }).collect::<Vec<_>>(),
                }),
            );
            layout_result.push((space, layout));
        }

        layout_result
    }

    fn apply_layout(
        reactor: &mut Reactor,
        layout_result: LayoutResult,
        is_resize: bool,
        is_workspace_switch: bool,
    ) -> Result<bool, crate::model::reactor::ReactorError> {
        let main_window = reactor.main_window();
        trace!(?main_window);
        // The window in the user's hand is never laid out, for the whole
        // drag — not only in the one pass a frame report requested it for.
        // Whatever put it in a tree meanwhile, writing its slot under the
        // cursor is the one thing that must not happen; the drop lays it out.
        let skip_wid = reactor
            .window_in_drag()
            .or_else(|| reactor.drag_manager.skip_layout_for_window.take())
            .or(reactor.drag_manager.drag_swap_manager.dragged())
            // A window whose seam-finish is still settling: the system is
            // mid-relocation and its live frame reads as anything — half
            // off-screen looked "parked" and the float-restore wrote a stale
            // stored frame over the drop (the "pseudo-tile"). The finish
            // owns the window until it lands — but only while it is still a
            // float. Toggled back into the tree mid-settle, the arrange owns
            // its frame: skipping it here left the tree split around a
            // window that never moved (the toggle that "creates the space
            // but doesn't move the window").
            .or(reactor
                .drag_manager
                .seam_finish
                .filter(|finish| {
                    reactor.layout_manager.layout_engine.is_window_floating(finish.window)
                })
                .map(|finish| finish.window));
        let mut any_frame_changed = false;

        let active_space = reactor.workspace_command_space();
        for (space, layout) in layout_result {
            // A desktop a display owns but is not showing has no screen *by
            // space*; the display it belongs to is what says where to put it.
            // `calculate_layout` has already recorded that display, so asking
            // the engine finds the screen the frames were computed for.
            let screen = reactor.space_state.screen_by_space(space);
            if screen.is_none() {
                crate::sys::trace::act(
                    "arrange_apply",
                    &serde_json::json!({
                        "space": space, "skipped": "no screen holds this space",
                    }),
                );
            }
            if let Some(screen) = screen {
                let screen_frame = screen.frame;
                let display_uuid = screen.display_uuid_owned();
                let gaps = reactor
                    .config
                    .settings
                    .layout
                    .gaps
                    .effective_for_display(display_uuid.as_deref());
                let active_workspace_for_space_has_fullscreen = active_space == Some(space)
                    && reactor
                        .layout_manager
                        .layout_engine
                        .active_workspace_for_space_has_fullscreen(space);
                let group_infos = reactor.layout_manager.layout_engine.collect_group_containers(
                    space,
                    screen_frame,
                    &gaps,
                    reactor.config.settings.ui.stack_line.thickness(),
                    reactor.config.settings.ui.stack_line.horiz_placement,
                    reactor.config.settings.ui.stack_line.vert_placement,
                );

                // Keep internal stack-line UI actor fed from the same group snapshot.
                if reactor.config.settings.ui.stack_line.enabled
                    && let Some(tx) = &reactor.communication_manager.stack_line_tx
                {
                    let groups: Vec<crate::actor::stack_line::GroupInfo> = group_infos
                        .iter()
                        .map(|g| crate::actor::stack_line::GroupInfo {
                            node_id: g.node_id,
                            space_id: space,
                            container_kind: g.container_kind,
                            frame: g.frame,
                            total_count: g.total_count,
                            selected_index: g.selected_index,
                            window_ids: g.window_ids.clone(),
                        })
                        .collect();
                    let active_space_ids: Vec<crate::sys::screen::SpaceId> =
                        reactor.iter_active_spaces().collect();

                    if let Err(e) = tx.try_send(crate::actor::stack_line::Event::GroupsUpdated {
                        active_space_ids,
                        space_id: space,
                        groups,
                        active_workspace_for_space_has_fullscreen,
                    }) {
                        tracing::warn!("Failed to send groups update to stack_line: {}", e);
                    }
                }

                if let Some(workspace_id) =
                    reactor.layout_manager.layout_engine.active_workspace(space)
                {
                    let workspace_index =
                        reactor.layout_manager.layout_engine.active_workspace_idx(space);
                    let workspace_name = reactor
                        .layout_manager
                        .layout_engine
                        .workspace_name(space, workspace_id)
                        .unwrap_or_else(|| format!("Workspace {:?}", workspace_id));

                    let stacks: Vec<StackInfo> = group_infos
                        .iter()
                        .map(|g| StackInfo {
                            container_kind: match g.container_kind {
                                crate::layout_engine::LayoutKind::Horizontal => {
                                    rift_protocol::LayoutKind::Horizontal
                                }
                                crate::layout_engine::LayoutKind::Vertical => {
                                    rift_protocol::LayoutKind::Vertical
                                }
                                crate::layout_engine::LayoutKind::HorizontalStack => {
                                    rift_protocol::LayoutKind::HorizontalStack
                                }
                                crate::layout_engine::LayoutKind::VerticalStack => {
                                    rift_protocol::LayoutKind::VerticalStack
                                }
                            },
                            total_count: g.total_count,
                            selected_index: g.selected_index,
                            windows: g.window_ids.iter().map(WindowId::to_debug_string).collect(),
                        })
                        .collect();

                    if stacks.len() > 0 {
                        let event = BroadcastEvent::StacksChanged {
                            workspace_id: protocol_workspace_id(workspace_id),
                            workspace_index,
                            workspace_name,
                            stacks,
                            active_workspace_has_fullscreen:
                                active_workspace_for_space_has_fullscreen,
                            space_id: space.get(),
                            display_uuid,
                        };
                        let _ = reactor.communication_manager.event_broadcaster.send(event);
                    }
                }
            }

            if is_workspace_switch {
                any_frame_changed |=
                    AnimationManager::workspace_switch_layout(reactor, space, &layout, skip_wid);
            } else if reactor.workspace_switch_manager.active_workspace_switch.is_some() {
                any_frame_changed |=
                    AnimationManager::instant_layout(reactor, space, &layout, skip_wid);
            } else {
                any_frame_changed |=
                    AnimationManager::animate_layout(reactor, space, &layout, is_resize, skip_wid);
            }
        }

        Ok(any_frame_changed)
    }
}

/// Manages pending space changes
pub struct PendingSpaceChangeManager {
    pub pending_space_change: Option<ForwardedSpaceState>,
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::bound_frame_to_screen;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    #[test]
    fn bound_frame_to_screen_keeps_partial_overlap_for_strip_behavior() {
        let screen = rect(2000.0, 0.0, 1000.0, 800.0);
        let frame = rect(1500.0, 50.0, 700.0, 400.0);
        let bounded = bound_frame_to_screen(frame, screen);
        assert_eq!(bounded.origin.x, 1500.0);
        assert_eq!(bounded.size.width, 700.0);
    }

    #[test]
    fn bound_frame_to_screen_parks_fully_offscreen_windows_to_hidden_sliver() {
        let screen = rect(2000.0, 0.0, 1000.0, 800.0);
        let frame = rect(1200.0, 80.0, 600.0, 300.0);
        let bounded = bound_frame_to_screen(frame, screen);
        assert_eq!(bounded.origin.x, 1410.0);
        assert_eq!(bounded.size.width, 600.0);
    }

    #[test]
    fn bound_frame_to_screen_parks_right_offscreen_windows_to_hidden_sliver() {
        let screen = rect(2000.0, 0.0, 1000.0, 800.0);
        let frame = rect(3200.0, 80.0, 600.0, 300.0);
        let bounded = bound_frame_to_screen(frame, screen);
        assert_eq!(bounded.origin.x, 2990.0);
        assert_eq!(bounded.size.width, 600.0);
    }

    #[test]
    fn bound_frame_to_screen_does_not_park_partially_visible_right_windows() {
        let screen = rect(2000.0, 0.0, 1000.0, 800.0);
        let frame = rect(2998.0, 80.0, 600.0, 300.0);
        let bounded = bound_frame_to_screen(frame, screen);
        assert_eq!(bounded.origin.x, 2998.0);
        assert_eq!(bounded.size.width, 600.0);
    }
}
