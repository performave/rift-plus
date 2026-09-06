//! The Reactor's job is to maintain coherence between the system and model state.
//!
//! It takes events from the rest of the system and builds a coherent picture of
//! what is going on. It shares this with the layout actor, and reacts to layout
//! changes by sending requests out to the other actors in the system.

mod animation;
mod display_archive;
mod display_record;
mod events;
mod fullscreen_slots;
mod main_window;
mod managers;
mod query;
mod replay;
pub mod transaction_manager;
mod utils;

#[cfg(test)]
mod testing;

#[cfg(test)]
#[allow(non_snake_case)]
mod SpaceEventHandler {
    pub use super::events::space::WindowServerLifecyclePayload;

    pub fn handle_window_server_destroyed(
        reactor: &mut super::Reactor,
        payload: WindowServerLifecyclePayload,
    ) -> anyhow::Result<super::EventOutcome> {
        let wsid = payload.window_server_id;
        let tracked_window = reactor.state.windows.tracked_window_id(wsid);
        let assigned_space =
            tracked_window.and_then(|window| reactor.assigned_space_for_window_id(window));
        let observations = super::events::space::WindowServerDestroyedObservations {
            resolved_space: reactor.resolve_native_space(wsid, None),
            active_spaces: reactor.active_spaces.clone(),
            mission_control_active: reactor.is_mission_control_active(),
            ordered_in: crate::sys::window_server::window_ordered_in(wsid),
            still_known: crate::sys::window_server::get_window(wsid).is_some(),
            assigned_space,
            last_known_user_space: super::events::space::resolve_last_known_user_space(
                tracked_window.and_then(|window| reactor.best_space_for_window_id(window)),
                reactor.space_state.iter_known_spaces().next(),
            ),
        };
        let outcome = super::events::space::handle_window_server_destroyed(
            &mut reactor.state,
            &reactor.transaction_manager,
            &mut reactor.drag_manager,
            payload,
            observations,
        )?;
        reactor.apply_event_outcome(outcome);
        Ok(super::EventOutcome::default())
    }

    pub fn handle_window_server_appeared(
        reactor: &mut super::Reactor,
        window_server_id: crate::sys::window_server::WindowServerId,
        space: crate::sys::screen::SpaceId,
        kind: super::SpaceEventKind,
    ) {
        reactor.handle_event(super::Event::WindowServerAppeared(window_server_id, space, kind));
    }
}

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::rc::Rc;
use std::thread;

use animation::Sender as AnimationSender;
use events::{
    EventOutcome, app as application_workflow, command as command_workflow,
    drag as interaction_workflow, focus as focus_service, space as topology_workflow,
    system as system_workflow, window as window_workflow,
};
use main_window::MainWindowTracker;
use managers::LayoutManager;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
pub use replay::{Record, replay};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tracing::{debug, info, instrument, trace, warn};
use transaction_manager::TransactionId;

use super::{event_tap, gesture_tap};
use crate::actor::app::{
    AppInfo, AppThreadHandle, Quiet, Request, WindowId, WindowInfo, WindowInventoryToken, pid_t,
};
use crate::actor::raise_manager::{self, RaiseManager, RaiseRequest};
use crate::actor::reactor::events::window_discovery;
use crate::actor::spaces::{ForwardedSpaceState, TopologyWindowDelta};
use crate::actor::{self, menu_bar, stack_line};
use crate::common::collections::{BTreeMap, HashMap, HashSet};
use crate::common::config::Config;
use crate::layout_engine::{self as layout, Direction, LayoutEngine, LayoutEvent, ResolvedWindow};
use crate::model::broadcast::{
    BroadcastEvent, BroadcastSender, protocol_window_id, protocol_workspace_id,
};
use crate::model::space_activation::{SpaceActivationConfig, SpaceActivationPolicy};
use crate::model::tx_store::WindowTxStore;
use crate::model::{AppRuleResult, RiftState};
use crate::sys::event::MouseState;
use crate::sys::executor::Executor;
use crate::sys::geometry::{CGPointDef, CGRectDef, CGRectExt, SameAs};
pub use crate::sys::screen::ScreenInfo;
use crate::sys::screen::{SpaceId, order_visible_spaces_by_position};
use crate::sys::window_server::{
    self, WindowServerId, WindowServerInfo, window_level, window_sub_level,
};

pub type Sender = actor::Sender<Event>;
type Receiver = actor::Receiver<Event>;
use managers::RefreshQuarantineState;
pub use query::ReactorQueryHandle;

pub(crate) use crate::model::reactor::{AppState, WindowState};
pub use crate::model::reactor::{
    Command, DisplaySelector, DragSession, DragState, MenuState, MissionControlState,
    ReactorCommand, RefocusState, Requested, StaleCleanupState, WorkspaceSwitchOrigin,
    WorkspaceSwitchState,
};

#[derive(Clone)]
pub struct ReactorHandle {
    sender: Sender,
    queries: ReactorQueryHandle,
}

impl ReactorHandle {
    pub fn new(sender: Sender, queries: ReactorQueryHandle) -> Self { Self { sender, queries } }

    pub fn sender(&self) -> Sender { self.sender.clone() }

    pub fn send(&self, event: Event) { self.sender.send(event) }

    pub fn try_send(
        &self,
        event: Event,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<(tracing::Span, Event)>> {
        self.sender.try_send(event)
    }
}

impl std::ops::Deref for ReactorHandle {
    type Target = ReactorQueryHandle;

    fn deref(&self) -> &Self::Target { &self.queries }
}

use crate::model::server::RuntimeWindowData;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpaceEventKind {
    User,
    Fullscreen,
}

/// Floor for modifier-drag resizing, so a fast drag cannot collapse a window
/// to nothing (or invert it) before the user lets go.
const MIN_MODIFIER_DRAG_SIZE: f64 = 100.0;

/// The half of `frame` a window dropped on that side would occupy.
fn half_of(frame: CGRect, direction: Direction) -> CGRect {
    let half_w = frame.size.width / 2.0;
    let half_h = frame.size.height / 2.0;
    match direction {
        Direction::Left => CGRect::new(frame.origin, CGSize::new(half_w, frame.size.height)),
        Direction::Right => CGRect::new(
            CGPoint::new(frame.origin.x + half_w, frame.origin.y),
            CGSize::new(half_w, frame.size.height),
        ),
        Direction::Up => CGRect::new(frame.origin, CGSize::new(frame.size.width, half_h)),
        Direction::Down => CGRect::new(
            CGPoint::new(frame.origin.x, frame.origin.y + half_h),
            CGSize::new(frame.size.width, half_h),
        ),
    }
}

/// Which edges of a window a modifier resize moves.
///
/// Chosen from where the press landed, like grabbing a corner: the halves the
/// cursor is in are the ones that follow it.
#[derive(Clone, Copy, Debug, Default)]
struct ResizeEdges {
    left: bool,
    top: bool,
}

impl ResizeEdges {
    fn from_press(frame: CGRect, at: CGPoint) -> Self {
        Self {
            left: at.x < frame.mid().x,
            top: at.y < frame.mid().y,
        }
    }

    /// The frame after dragging `dx`/`dy` from where the press landed.
    ///
    /// A left-edge drag moves the origin and takes the width the other way, so
    /// the opposite edge stays put; a right-edge drag only changes the width.
    fn apply(self, frame: CGRect, dx: f64, dy: f64) -> CGRect {
        let mut out = frame;
        if self.left {
            out.origin.x = frame.origin.x + dx;
            out.size.width = frame.size.width - dx;
        } else {
            out.size.width = frame.size.width + dx;
        }
        if self.top {
            out.origin.y = frame.origin.y + dy;
            out.size.height = frame.size.height - dy;
        } else {
            out.size.height = frame.size.height + dy;
        }
        // Never let a fast drag invert the window through zero. Clamping the
        // size alone would let a left drag keep walking the origin, so the
        // origin is pinned to the edge that is not moving.
        if out.size.width < MIN_MODIFIER_DRAG_SIZE {
            out.size.width = MIN_MODIFIER_DRAG_SIZE;
            if self.left {
                out.origin.x = frame.origin.x + frame.size.width - MIN_MODIFIER_DRAG_SIZE;
            }
        }
        if out.size.height < MIN_MODIFIER_DRAG_SIZE {
            out.size.height = MIN_MODIFIER_DRAG_SIZE;
            if self.top {
                out.origin.y = frame.origin.y + frame.size.height - MIN_MODIFIER_DRAG_SIZE;
            }
        }
        out
    }
}

/// An in-flight modifier drag, as the reactor sees it.
#[derive(Clone, Copy)]
struct ModifierDragState {
    window: WindowId,
    action: crate::common::config::MouseAction,
    /// The window's frame when the drag began; every update is applied to this
    /// rather than to the previous frame, so nothing drifts.
    origin_frame: CGRect,
    edges: ResizeEdges,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub enum Event {
    SpaceStateChanged(ForwardedSpaceState),
    #[serde(skip)]
    ActiveDisplayChanged {
        menu_bar_space: Option<SpaceId>,
        command_space: Option<SpaceId>,
    },
    /// An application was launched. This event is also sent for every running
    /// application on startup.
    ///
    /// Both WindowInfo (accessibility) and WindowServerInfo are collected for
    /// any already-open windows when the launch event is sent. Since this
    /// event isn't ordered with respect to the Space events, it is possible to
    /// receive this event for a space we just switched off of.. FIXME. The same
    /// is true of WindowCreated events.
    ApplicationLaunched {
        pid: pid_t,
        info: AppInfo,
        #[serde(skip, default = "replay::deserialize_app_thread_handle")]
        handle: AppThreadHandle,
        is_frontmost: bool,
        main_window: Option<WindowId>,
        visible_windows: Vec<(WindowId, WindowInfo)>,
        window_server_info: Vec<WindowServerInfo>,
    },
    ApplicationTerminated(pid_t),
    ApplicationThreadTerminated(pid_t),
    ApplicationActivated(pid_t, Quiet),
    ApplicationDeactivated(pid_t),
    ApplicationGloballyActivated(pid_t),
    ApplicationGloballyDeactivated(pid_t),
    ApplicationMainWindowChanged(pid_t, Option<WindowId>, Quiet),
    /// Authoritative focus resolved from WindowServer's key-focus process and
    /// the z-ordered windows on the active native spaces.
    WindowServerFocusChanged(WindowId, SpaceId),

    WindowsDiscovered {
        pid: pid_t,
        token: WindowInventoryToken,
        successful: bool,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
    },
    #[serde(skip)]
    WindowInventoryRefreshRequested(pid_t),
    #[serde(skip)]
    RaiseTargetsMissing {
        windows: Vec<WindowId>,
        sequence_id: u64,
    },
    WindowCreated(
        WindowId,
        WindowInfo,
        Option<WindowServerInfo>,
        Option<MouseState>,
    ),
    WindowDestroyed(WindowId),
    /// Recorded, not skipped: a window's arrival on and departure from a space
    /// is where native fullscreen is decided, and leaving these two out of the
    /// flight recorder made every fullscreen bug invisible to a trace.
    WindowServerDestroyed(
        crate::sys::window_server::WindowServerId,
        SpaceId,
        SpaceEventKind,
    ),
    WindowServerAppeared(
        crate::sys::window_server::WindowServerId,
        SpaceId,
        SpaceEventKind,
    ),
    #[serde(skip)]
    SpaceCreated(SpaceId),
    /// A returned display's windows were given until now to arrive on its
    /// new space. See `display_archive`.
    #[serde(skip)]
    DisplayHomingDeadline(String),
    #[serde(skip)]
    SpaceDestroyed(SpaceId),
    WindowMinimized(WindowId),
    WindowDeminiaturized(WindowId),
    WindowFrameChanged(
        WindowId,
        #[serde(with = "CGRectDef")] CGRect,
        Option<TransactionId>,
        Requested,
        Option<MouseState>,
    ),
    WindowTitleChanged(WindowId, String),
    MenuOpened(pid_t),
    MenuClosed(pid_t),

    /// Left mouse button was released.
    ///
    /// Layout changes are suppressed while the button is down so that they
    /// don't interfere with drags. This event is used to update the layout in
    /// case updates were supressed while the button was down.
    ///
    /// FIXME: This can be interleaved incorrectly with the MouseState in app
    /// actor events.
    MouseUp,
    /// A modifier-held drag has started over `window`, at `at` in window-server
    /// coordinates.
    ///
    /// The tap swallows the whole gesture so the application never sees it. The
    /// press is reported separately from the movement because which edges a
    /// resize moves is decided by where in the window it began — the same rule
    /// as dragging a window's corner — and that has to be captured before the
    /// window starts changing shape.
    MouseModifierDragBegin {
        window: WindowServerId,
        #[serde(with = "CGPointDef")]
        at: CGPoint,
        action: crate::common::config::MouseAction,
    },
    /// The window server ordered a window in or out.
    ///
    /// Closing the window of an app that keeps running orders the window out
    /// rather than destroying it, so no destroy notification ever arrives and
    /// the window keeps its place in the layout.
    WindowServerVisibilityChanged(WindowServerId, bool),
    /// Movement during a modifier drag, measured from where the drag began
    /// rather than from the previous event, so the window cannot drift away
    /// from the cursor over a long drag.
    MouseModifierDrag {
        dx: f64,
        dy: f64,
    },
    /// Sent by the event tap only when the cursor enters a different window.
    /// Window resolution and transition deduplication stay on the input
    /// thread; the reactor only applies the model-dependent focus/raise work.
    MouseMoved(WindowServerId),
    /// Sent by the event tap while a mouse button is held and the pointer
    /// moves. A window being dragged reports its frame at whatever cadence
    /// its app chooses; the pointer is what decides where a drop lands, so
    /// the drop target is re-evaluated on every sample of it.
    MouseDragged {
        x: f64,
        y: f64,
    },
    /// Forwarded by the spaces actor after wake has been observed.
    ///
    /// The spaces actor is the authority for sleep/lock/display lifecycle.
    /// The reactor uses this only to reopen refresh gating and resubscribe
    /// WindowServer notifications once the topology authority says wake
    /// processing has advanced.
    SystemWoke,
    #[serde(skip)]
    SystemWillSleep,
    #[serde(skip)]
    SessionDidResignActive,
    #[serde(skip)]
    SessionDidBecomeActive,

    #[serde(skip)]
    DisplayChurnBegin,
    #[serde(skip)]
    DisplayChurnEnd,

    #[serde(skip)]
    MissionControlNativeEntered,
    #[serde(skip)]
    MissionControlNativeExited,

    /// A raise request completed. Used by the raise manager to track when
    /// all raise requests in a sequence have finished.
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },

    /// A raise sequence timed out. Used by the raise manager to clean up
    /// pending raises that took too long.
    RaiseTimeout {
        sequence_id: u64,
    },

    #[serde(skip)]
    Query(query::QueryRequest),

    #[serde(skip)]
    InstallIpc(crate::ipc::InstallRequest),

    Command(Command),

    #[serde(skip)]
    RegisterWmSender(crate::actor::wm_controller::Sender),

    #[serde(skip)]
    ConfigUpdated(Config),
}

/// What to say when the scripting addition is not answering. The addition is
/// the only route macOS 26 leaves for these three, so naming it — and the
/// command that loads it — is the whole of the useful advice.
const SA_REQUIRED_CREATE: &str = "creating a space needs the scripting addition; run `sudo rift sa load` (`rift sa status` to check)";
const SA_REQUIRED_DESTROY: &str = "destroying a space needs the scripting addition; run `sudo rift sa load` (`rift sa status` to check)";
const SA_REQUIRED_MOVE: &str = "moving a window to a space needs the scripting addition; run `sudo rift sa load` (`rift sa status` to check)";

pub struct Reactor {
    pub config: Config,
    pub one_space: bool,
    app_manager: managers::AppManager,
    layout_manager: managers::LayoutManager,
    pub(crate) state: RiftState,
    space_state: ForwardedSpaceState,
    space_activation_policy: SpaceActivationPolicy,
    main_window_tracker: MainWindowTracker,
    drag_manager: managers::DragManager,
    workspace_switch_manager: managers::WorkspaceSwitchManager,
    recording_manager: managers::RecordingManager,
    communication_manager: managers::CommunicationManager,
    notification_manager: managers::NotificationManager,
    transaction_manager: transaction_manager::TransactionManager,
    /// The modifier drag in flight, if any. See `ModifierDragState`.
    modifier_drag: Option<ModifierDragState>,
    /// The float grab strips last pushed to the event tap, to push only
    /// changes. See `Request::SetFloatDragStrips` (event tap).
    last_float_strips: Vec<(u32, i32, CGRect)>,
    /// When the mouse button last came up. A focus change that follows a
    /// click is the pointer's doing; the pointer is not moved for it.
    last_mouse_up: Option<std::time::Instant>,
    /// The window that had focus when focus went somewhere rift cannot name
    /// a window for — a status-item popover, a menu, a windowless app. Focus
    /// coming straight back to it is the user finishing with that, not
    /// choosing the window again, and does not move the pointer.
    focus_left_from: Option<WindowId>,
    menu_manager: managers::MenuManager,
    mission_control_manager: managers::MissionControlManager,
    window_inventory_manager: managers::WindowInventoryManager,
    refocus_manager: managers::RefocusManager,
    refresh_quarantine_manager: managers::RefreshQuarantineManager,
    pending_space_change_manager: managers::PendingSpaceChangeManager,
    active_spaces: HashSet<SpaceId>,
    display_archive: display_archive::DisplayArchive,
    fullscreen_slots: fullscreen_slots::FullscreenSlots,
    pub animation_tx: Option<AnimationSender>,
    /// Why the command now being handled could not be carried out, if it
    /// could not. Set by `fail_command` and drained by `handle_ipc_command`,
    /// so a command that does nothing says so instead of reporting success.
    command_error: Option<String>,
    #[cfg(test)]
    pub(crate) test_mouse_warps: Vec<CGPoint>,
}

/// Layout commands that act on the focused window rather than on the
/// workspace or the tree as a whole.
fn targets_focused_window(command: &layout::LayoutCommand) -> bool {
    use layout::LayoutCommand as L;
    matches!(
        command,
        L::MoveNode(_)
            | L::JoinWindow(_)
            | L::ConsumeOrExpelWindow(_)
            | L::UnjoinWindows
            | L::ToggleWindowFloating
            | L::ToggleWindowFloatingWithOptions(_)
            | L::ToggleFullscreen
            | L::ToggleFullscreenWithinGaps
            | L::ResizeWindowGrow(_)
            | L::ResizeWindowShrink(_)
            | L::ResizeWindowBy { .. }
            | L::CenterSelection
            | L::MoveWindowToWorkspace { .. }
            | L::PromoteToMaster
    )
}

impl Reactor {
    /// How long after a click a focus change is still taken to be its
    /// consequence: the app activates on mouse-down, and the reports of it
    /// reach rift up to a few hundred milliseconds later.
    const CLICK_FOCUS_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

    pub fn spawn(
        config: Config,
        layout_engine: LayoutEngine,
        record: Record,
        event_tap_tx: event_tap::Sender,
        broadcast_tx: BroadcastSender,
        menu_tx: menu_bar::Sender,
        stack_line_tx: stack_line::Sender,
        drop_overlay_tx: crate::actor::drop_overlay::Sender,
        window_notify: Option<(crate::actor::window_notify::Sender, WindowTxStore)>,
        gesture_tap_tx: Option<gesture_tap::Sender>,
        one_space: bool,
    ) -> ReactorHandle {
        let (events_tx, events) = actor::channel();
        let events_tx_clone = events_tx.clone();
        let mut reactor = Reactor::new(
            config,
            layout_engine,
            record,
            broadcast_tx,
            window_notify,
            one_space,
        );
        reactor.communication_manager.event_tap_tx = Some(event_tap_tx);
        reactor.menu_manager.menu_tx = Some(menu_tx);
        reactor.communication_manager.stack_line_tx = Some(stack_line_tx);
        reactor.communication_manager.drop_overlay_tx = Some(drop_overlay_tx);
        reactor.communication_manager.gesture_tap_tx = gesture_tap_tx;
        reactor.communication_manager.events_tx = Some(events_tx_clone.clone());
        let query_handle = ReactorQueryHandle::new(events_tx_clone.clone());
        thread::Builder::new()
            .name("reactor".to_string())
            .spawn(move || {
                Executor::run(Reactor::run(reactor, events, events_tx_clone));
            })
            .unwrap();
        ReactorHandle::new(events_tx, query_handle)
    }

    pub fn new(
        config: Config,
        layout_engine: LayoutEngine,
        mut record: Record,
        broadcast_tx: BroadcastSender,
        window_notify: Option<(crate::actor::window_notify::Sender, WindowTxStore)>,
        one_space: bool,
    ) -> Reactor {
        // FIXME: Remove apps that are no longer running from restored state.
        record.start(&config, &layout_engine);
        let (raise_manager_tx, _rx) = actor::channel();
        let (window_notify_tx, window_tx_store) = match window_notify {
            Some((tx, store)) => (Some(tx), store),
            None => (None, WindowTxStore::new()),
        };
        let reactor = Reactor {
            config: config.clone(),
            one_space,
            command_error: None,
            app_manager: managers::AppManager::new(),
            layout_manager: managers::LayoutManager { layout_engine },
            state: RiftState::default(),
            space_state: ForwardedSpaceState::default(),
            space_activation_policy: SpaceActivationPolicy::new(),
            main_window_tracker: MainWindowTracker::default(),
            drag_manager: managers::DragManager {
                drag_state: DragState::Inactive,
                drop_overlay_shown: false,
                notifications_silenced: None,
                space_sync_at: None,
                seam_finish: None,
                zone_candidate: None,
                drop_preview_cache: None,
                drag_swap_manager: crate::actor::drag_swap::DragManager::new(
                    config.settings.window_snapping,
                ),
                skip_layout_for_window: None,
                drop_pin: None,
                held_window: None,
            },
            workspace_switch_manager: managers::WorkspaceSwitchManager {
                workspace_switch_state: WorkspaceSwitchState::Inactive,
                workspace_switch_generation: 0,
                active_workspace_switch: None,
                pending_workspace_switch_origin: None,
                pending_workspace_mouse_warp: None,
            },
            recording_manager: managers::RecordingManager { record },
            communication_manager: managers::CommunicationManager {
                event_tap_tx: None,
                gesture_tap_tx: None,
                stack_line_tx: None,
                drop_overlay_tx: None,
                raise_manager_tx,
                event_broadcaster: broadcast_tx,
                wm_sender: None,
                events_tx: None,
            },
            notification_manager: managers::NotificationManager {
                last_sls_notification_ids: Vec::new(),
                last_layout_modes_by_space: HashMap::default(),
                _window_notify_tx: window_notify_tx,
            },
            transaction_manager: transaction_manager::TransactionManager::new(window_tx_store),
            modifier_drag: None,
            last_float_strips: Vec::new(),
            last_mouse_up: None,
            focus_left_from: None,
            menu_manager: managers::MenuManager {
                menu_state: MenuState::Closed,
                menu_tx: None,
            },
            mission_control_manager: managers::MissionControlManager {
                mission_control_state: MissionControlState::Inactive,
            },
            window_inventory_manager: managers::WindowInventoryManager {
                topology_revision: 0,
                next_request_id: 0,
                in_flight: HashMap::default(),
                pending: HashSet::default(),
                refocus_after_refresh: HashMap::default(),
            },
            refocus_manager: managers::RefocusManager {
                stale_cleanup_state: StaleCleanupState::Enabled,
                refocus_state: RefocusState::None,
            },
            refresh_quarantine_manager: managers::RefreshQuarantineManager {
                sleeping: false,
                session_inactive: false,
                display_churn_active: false,
                awaiting_post_wake_snapshot: false,
                awaiting_post_session_snapshot: false,
                pending_inventory_refresh: false,
                suppress_auto_workspace_switch_until_input: false,
            },
            pending_space_change_manager: managers::PendingSpaceChangeManager {
                pending_space_change: None,
            },
            active_spaces: HashSet::default(),
            display_archive: Default::default(),
            fullscreen_slots: Default::default(),
            #[cfg(test)]
            test_mouse_warps: Vec::new(),
            animation_tx: None,
        };
        reactor
    }

    fn set_active_spaces(&mut self, spaces: &[Option<SpaceId>]) {
        self.active_spaces.clear();
        for space in spaces.iter().flatten().copied() {
            self.active_spaces.insert(space);
        }
    }

    fn is_space_active(&self, space: SpaceId) -> bool { self.active_spaces.contains(&space) }

    fn iter_active_spaces(&self) -> impl Iterator<Item = SpaceId> + '_ {
        self.active_spaces.iter().copied()
    }

    fn advance_window_inventory_revision_if_needed(&mut self, incoming: &ForwardedSpaceState) {
        let topology_changed = self.space_state.screens != incoming.screens
            || self.space_state.active_spaces != incoming.active_spaces
            || self.space_state.display_space_ids != incoming.display_space_ids
            || self.space_state.active_window_spaces != incoming.active_window_spaces;
        if !topology_changed {
            return;
        }

        self.window_inventory_manager.topology_revision =
            self.window_inventory_manager.topology_revision.wrapping_add(1);
        self.window_inventory_manager
            .pending
            .extend(self.window_inventory_manager.in_flight.keys().copied());
    }

    fn abandon_window_inventories_from_instability(&mut self) {
        // AX requests issued before sleep or display teardown are not guaranteed to
        // reply. Keeping them in `in_flight` prevents the authoritative recovery
        // snapshot from requesting replacement AX elements, leaving every affected
        // window unusable until its application is activated manually.
        let abandoned = self
            .window_inventory_manager
            .in_flight
            .drain()
            .map(|(pid, _)| pid)
            .collect::<Vec<_>>();
        self.window_inventory_manager.pending.extend(abandoned);
    }

    fn request_window_inventory(&mut self, pid: pid_t) {
        if self.refreshes_blocked() || self.window_inventory_manager.in_flight.contains_key(&pid) {
            self.window_inventory_manager.pending.insert(pid);
            return;
        }

        let Some(app) = self.app_manager.apps.get(&pid) else {
            self.window_inventory_manager.pending.remove(&pid);
            return;
        };
        self.window_inventory_manager.next_request_id =
            self.window_inventory_manager.next_request_id.wrapping_add(1);
        let token = WindowInventoryToken {
            request_id: self.window_inventory_manager.next_request_id,
            topology_revision: self.window_inventory_manager.topology_revision,
        };
        if app.handle.send(Request::RefreshWindowInventory(token)).is_ok() {
            self.window_inventory_manager.in_flight.insert(pid, token);
            self.window_inventory_manager.pending.remove(&pid);
        }
    }

    fn forget_window_inventory(&mut self, pid: pid_t) {
        self.window_inventory_manager.in_flight.remove(&pid);
        self.window_inventory_manager.pending.remove(&pid);
        self.window_inventory_manager.refocus_after_refresh.remove(&pid);
    }

    fn finish_window_inventory(
        &mut self,
        pid: pid_t,
        token: WindowInventoryToken,
        successful: bool,
    ) -> bool {
        if self.window_inventory_manager.in_flight.get(&pid).copied() != Some(token) {
            return false;
        }
        self.window_inventory_manager.in_flight.remove(&pid);

        let accepted = successful
            && !self.refreshes_blocked()
            && token.topology_revision == self.window_inventory_manager.topology_revision;
        if !accepted && successful {
            self.window_inventory_manager.pending.insert(pid);
        }
        if self.window_inventory_manager.pending.remove(&pid) {
            self.request_window_inventory(pid);
        }
        accepted
    }

    fn active_space_ids(&self) -> Vec<u64> {
        self.active_spaces.iter().map(|space| space.get()).collect()
    }

    fn is_window_on_active_space(&self, wid: WindowId) -> bool {
        self.best_space_for_window_id(wid)
            .is_some_and(|space| self.is_space_active(space))
    }

    fn activation_cfg(&self) -> SpaceActivationConfig {
        SpaceActivationConfig {
            default_disable: self.config.settings.default_disable,
            one_space: self.one_space,
        }
    }

    fn screens_for_current_spaces(&self) -> Vec<ScreenInfo> { self.space_state.screens.clone() }

    fn display_uuids_for_current_screens(&self) -> Vec<Option<String>> {
        self.space_state
            .screens
            .iter()
            .map(|screen| screen.display_uuid_owned())
            .collect()
    }

    #[cfg(test)]
    fn raw_spaces_for_current_screens(&self) -> Vec<Option<SpaceId>> {
        self.space_state.screens.iter().map(|s| s.space).collect()
    }

    fn display_uuid_for_space(&self, space: SpaceId) -> Option<String> {
        self.space_state
            .screen_by_space(space)
            .and_then(|screen| screen.display_uuid_owned())
    }

    fn expose_space_if_known(&mut self, space: SpaceId) {
        let Some(screen) = self.space_state.screen_by_space(space) else {
            return;
        };
        self.layout_manager
            .layout_engine
            .virtual_workspace_manager_mut()
            .list_workspaces(space);
        self.send_layout_event(LayoutEvent::SpaceExposed(space, screen.frame.size));
    }

    fn recompute_and_set_active_spaces(&mut self, spaces: &[Option<SpaceId>]) {
        let cfg = self.activation_cfg();
        let display_uuids = self.display_uuids_for_current_screens();
        let active_spaces =
            self.space_activation_policy.compute_active_spaces(cfg, spaces, &display_uuids);
        let previous_active = self.active_spaces.clone();
        self.set_active_spaces(&active_spaces);
        self.handle_active_space_change(previous_active);
    }

    fn recompute_and_set_active_spaces_from_current_screens(&mut self) {
        let raw_spaces = self.authoritative_spaces_for_current_screens();
        self.recompute_and_set_active_spaces(&raw_spaces);
    }

    fn authoritative_spaces_for_current_screens(&self) -> Vec<Option<SpaceId>> {
        self.space_state
            .screens
            .iter()
            .map(|screen| {
                screen.space.filter(|space| self.space_state.active_spaces.contains(space))
            })
            .collect()
    }

    fn handle_active_space_change(&mut self, previous_active: HashSet<SpaceId>) {
        if previous_active == self.active_spaces {
            return;
        }

        let deactivated: Vec<SpaceId> =
            previous_active.difference(&self.active_spaces).copied().collect();
        let activated: Vec<SpaceId> =
            self.active_spaces.difference(&previous_active).copied().collect();

        // Do not remove windows when a space is merely deactivated (e.g. macOS Space
        // switches). Removing them clears workspace assignments and causes windows
        // without app rules to be re-assigned to the current workspace.

        if !activated.is_empty() {
            for space in &activated {
                self.expose_space_if_known(*space);
            }
        }

        if !activated.is_empty() || !deactivated.is_empty() {
            self.refresh_window_server_snapshot_for_active_spaces();
            self.request_window_inventories();
        }

        if !activated.is_empty() {
            self.apply_app_rules_for_activated_spaces(&activated);
        }
    }

    fn apply_app_rules_for_activated_spaces(&mut self, activated: &[SpaceId]) {
        let activated_set: HashSet<SpaceId> = activated.iter().copied().collect();
        let mut windows_by_pid: HashMap<pid_t, Vec<WindowId>> = HashMap::default();

        for (&wsid, &space) in &self.space_state.active_window_spaces {
            if !activated_set.contains(&space) {
                continue;
            }
            let Some(wid) = self.state.windows.tracked_window_id(wsid) else {
                continue;
            };
            let Some(state) = self.state.windows.window(wid) else {
                continue;
            };
            if !state.can_reconcile_admission() {
                continue;
            }
            windows_by_pid.entry(wid.pid).or_default().push(wid);
        }

        for (pid, window_ids) in windows_by_pid {
            let Some(app_state) = self.app_manager.apps.get(&pid) else {
                continue;
            };

            self.process_windows_for_app_rules(window_ids, app_state.info.clone(), false);
        }
    }

    fn refresh_window_server_snapshot_for_active_spaces(&mut self) {
        let active_windows = self.authoritative_active_space_windows();
        self.reconcile_authoritative_active_window_snapshot(active_windows, true);
    }

    fn authoritative_active_space_windows(&self) -> Vec<(WindowServerId, Option<SpaceId>)> {
        let mut queried = HashMap::default();
        for space in self.iter_active_spaces() {
            for wsid in window_server::space_window_list_for_connection(&[space.get()], 0, false)
                .into_iter()
                .map(WindowServerId::new)
            {
                queried.entry(wsid).or_insert(space);
            }
        }

        // A refresh can be partial while WindowServer is waking. Keep the last
        // forwarded per-space sample in that case, but never use the global
        // visible-window union as a substitute for querying each active space.
        let membership = if queried.is_empty() {
            self.space_state.active_window_spaces.clone()
        } else {
            queried
        };

        let mut membership: Vec<_> = membership
            .into_iter()
            .map(|(wsid, space)| (wsid, self.resolve_native_space(wsid, Some(space))))
            .collect();
        membership.sort_by_key(|(wsid, _)| *wsid);
        membership
    }

    fn has_known_windows_for_active_spaces(&self) -> bool {
        self.state.windows.iter_windows().any(|(wid, _)| {
            self.authoritative_space_for_window_id(wid)
                .is_some_and(|space| self.is_space_active(space))
        })
    }

    fn refresh_active_space_window_membership(
        &mut self,
        active_windows: Vec<(WindowServerId, Option<SpaceId>)>,
    ) {
        let active_wsids: HashSet<WindowServerId> =
            active_windows.iter().map(|(wsid, _)| *wsid).collect();

        // An empty active-space list is valid, but an empty WS-id result while we
        // already know about windows assigned to the active space is typically the
        // transient post-wake race on same-display space switches. Preserve the
        // existing visibility basis in that case and let the follow-up AX refresh
        // reconcile instead of blanking the workspace immediately.
        if active_wsids.is_empty() && self.has_known_windows_for_active_spaces() {
            return;
        }

        let previously_visible_wsids: Vec<_> =
            self.state.windows.iter_visible_window_server_ids().collect();
        for wsid in previously_visible_wsids {
            if !active_wsids.contains(&wsid) {
                self.state.windows.mark_window_hidden(wsid);
            }
        }

        for (wsid, space) in active_windows {
            let space = self.resolve_native_space(wsid, space);
            if let Some(space) = space {
                self.state.windows.set_window_server_space(wsid, Some(space));
                self.clear_pending_target_if_confirmed_space(wsid, space);
            }
            self.state.windows.mark_window_visible(wsid);
            self.state.windows.clear_window_server_observed(wsid);
        }
    }

    fn remove_windows_missing_from_active_space_snapshot(
        &mut self,
        previously_visible_wsids: Vec<WindowServerId>,
        preserve_assignments: bool,
    ) {
        for wsid in previously_visible_wsids {
            if self.state.windows.is_window_visible(wsid) {
                continue;
            }
            let Some(wid) = self.state.windows.tracked_window_id(wsid) else {
                continue;
            };
            let Some(space) = self.assigned_space_for_window_id(wid) else {
                continue;
            };
            if !self.is_space_active(space) {
                continue;
            }

            let inactive_target = self
                .resolve_native_space(wsid, None)
                .filter(|current_space| *current_space != space)
                .filter(|current_space| {
                    #[cfg(test)]
                    {
                        let _ = current_space;
                        true
                    }
                    #[cfg(not(test))]
                    {
                        window_server::space_is_user(current_space.get())
                    }
                })
                .filter(|current_space| !self.is_space_active(*current_space));
            if let Some(current_space) = inactive_target {
                self.state.windows.set_window_server_space(wsid, Some(current_space));
                let _ = self.reassign_window_to_authoritative_space(wid, current_space);
                continue;
            }

            if preserve_assignments {
                debug!(
                    ?wid,
                    ?wsid,
                    "Preserving workspace assignment omitted from partial authoritative snapshot"
                );
                continue;
            }

            // If the authoritative active-space snapshot no longer includes a
            // previously visible window and WindowServer cannot confirm a new
            // native space for it, drop the stale origin-space ownership. Keeping
            // the old assignment lets later discovery/MC refresh rebuild the
            // origin layout from stale workspace state.
            self.state.windows.set_window_server_space(wsid, None);
            self.send_layout_event(LayoutEvent::WindowRemoved(wid));
        }
    }

    fn reconcile_authoritative_active_window_snapshot(
        &mut self,
        active_windows: Vec<(WindowServerId, Option<SpaceId>)>,
        preserve_missing_assignments: bool,
    ) {
        let observed_windows = active_windows.clone();
        let previously_visible_wsids: Vec<_> =
            self.state.windows.iter_visible_window_server_ids().collect();
        self.refresh_active_space_window_membership(active_windows);
        self.remove_windows_missing_from_active_space_snapshot(
            previously_visible_wsids,
            preserve_missing_assignments,
        );
        self.reconcile_windows_in_authoritative_active_snapshot(&observed_windows);
    }

    fn is_login_window_pid(&self, pid: pid_t) -> bool {
        self.app_manager.apps.get(&pid).and_then(|a| a.info.bundle_id.as_deref())
            == Some("com.apple.loginwindow")
    }

    // fn store_txid(&self, wsid: Option<WindowServerId>, txid: TransactionId, target: CGRect) {
    //     self.transaction_manager.store_txid(wsid, txid, target);
    // }
    //
    // fn update_txid_entries<I>(&self, entries: I)
    // where
    //     I: IntoIterator<Item = (WindowServerId, TransactionId, CGRect)>,
    // {
    //     self.transaction_manager.update_entries(entries);
    // }
    //
    // fn remove_txid_for_window(&self, wsid: Option<WindowServerId>) {
    //     self.transaction_manager.remove_for_window(wsid);
    // }

    fn clear_pending_hidden_window_targets(&self) {
        for (wid, window) in self.state.windows.iter_windows() {
            if self.hidden_assigned_space_for_window_id(wid).is_none() {
                continue;
            }
            if let Some(wsid) = window.info.sys_id {
                self.transaction_manager.clear_target_for_window(wsid);
            }
        }
    }

    fn clear_pending_target_if_confirmed_space(
        &self,
        wsid: WindowServerId,
        confirmed_space: SpaceId,
    ) {
        if self.pending_target_space_for_window_server_id(wsid) == Some(confirmed_space) {
            self.transaction_manager.clear_target_for_window(wsid);
        }
    }

    /// Rift has sent the window to another space itself (scripting
    /// addition). A frame write still pending for it was for the tree it is
    /// leaving; it must not be taken, when the window server reports the
    /// move, as a write still in flight to be believed over the report.
    fn note_window_sent_to_space(&self, wsid: WindowServerId) {
        self.transaction_manager.clear_target_for_window(wsid);
    }

    fn is_in_drag(&self) -> bool {
        matches!(
            self.drag_manager.drag_state,
            DragState::Active { .. } | DragState::PendingSwap { .. }
        )
    }

    fn is_mission_control_active(&self) -> bool {
        matches!(
            self.mission_control_manager.mission_control_state,
            MissionControlState::Active
        )
    }

    fn get_pending_drag_swap(&self) -> Option<(WindowId, WindowId)> {
        if let DragState::PendingSwap { session, target } = &self.drag_manager.drag_state {
            Some((session.window, *target))
        } else {
            None
        }
    }

    /// The window the user is dragging, while the drag lasts. Its layout
    /// membership is frozen: the window server hands a dragged window to the
    /// other display the moment its centre crosses the border, and an
    /// active-display change follows as the pointer crosses — both while the
    /// button is still down. Acting on either re-tiled the window on the
    /// destination under the cursor. The drop (`MouseUp`) resolves where it
    /// belongs, once.
    /// Flight-recorder note for a frame report the user may be dragging:
    /// which branch handled it and what the reactor believed about the
    /// window's spaces at the time. Drags that silently produce no drop are
    /// invisible in the ring otherwise.
    fn note_frame_change_for_flight_recorder(
        &self,
        wid: WindowId,
        mouse_state: Option<crate::sys::event::MouseState>,
        branch: &str,
        spaces: Option<(Option<SpaceId>, Option<SpaceId>, bool, bool)>,
    ) {
        if !crate::sys::trace::is_recording()
            || (mouse_state != Some(crate::sys::event::MouseState::Down)
                && self.window_in_drag().is_none())
        {
            return;
        }
        let wsid = self.state.windows.window(wid).and_then(|window| window.info.sys_id);
        crate::sys::trace::act(
            "frame_change",
            &serde_json::json!({
                "wid": wid.idx.get(),
                "branch": branch,
                "mouse": mouse_state,
                "spaces": spaces,
                "assigned": self.assigned_space_for_window_id(wid),
                "fullscreen_suspended": wsid.is_some_and(|wsid| self.is_known_fullscreen_window(wsid)),
                "mission_control": self.is_mission_control_active(),
                "modifier_drag": self.modifier_drag.is_some(),
                "in_drag": self.window_in_drag().map(|w| w.idx.get()),
                "held": self.drag_manager.held_window.map(|w| w.idx.get()),
            }),
        );
    }

    pub(crate) fn window_in_drag(&self) -> Option<WindowId> {
        match &self.drag_manager.drag_state {
            DragState::Active { session } | DragState::PendingSwap { session, .. } => {
                Some(session.window)
            }
            DragState::Inactive => self.drag_manager.held_window,
        }
    }

    /// The window server hands a dragged window to the display under the
    /// pointer before the app reports the first frame with the button down
    /// — the space change is the first rift hears of the drag. A window
    /// that changes space while the button is down is, for that reason,
    /// taken to be in the user's hand: re-tiling it on the new display now
    /// yanks it out from under the cursor, and the drop would then start
    /// from the wrong tree. Hold it instead; the drop decides where it
    /// belongs (`settle_held_window` when the app never reports the drag).
    fn hold_if_dragged_across_spaces(&mut self, window: WindowId, target_space: SpaceId) {
        if self.window_in_drag().is_some() {
            return;
        }
        if crate::sys::event::get_mouse_state() != Some(crate::sys::event::MouseState::Down) {
            return;
        }
        let Some(assigned) = self.assigned_space_for_window_id(window) else {
            return;
        };
        if assigned == target_space || !self.is_space_active(assigned) {
            return;
        }
        debug!(
            ?window,
            ?assigned,
            ?target_space,
            "window changed space with the button down; holding it until the drop"
        );
        self.drag_manager.held_window = Some(window);
    }

    /// A held window whose drag the app never reported has no session to
    /// resolve the drop; the button is up, so where the window server has
    /// it now is where it belongs.
    fn settle_held_window(&mut self, window: WindowId) -> EventOutcome {
        let outcome = EventOutcome::default();
        let Some(wsid) = self.state.windows.window(window).and_then(|state| state.info.sys_id)
        else {
            return outcome;
        };
        let Some(space) = self.resolve_native_space(wsid, None) else {
            return outcome;
        };
        if self.assigned_space_for_window_id(window) == Some(space) || !self.is_space_active(space)
        {
            return outcome;
        }
        debug!(
            ?window,
            ?space,
            "settling a held window where the window server has it"
        );
        self.state.windows.set_window_server_space(wsid, Some(space));
        self.state.windows.mark_window_visible(wsid);
        if self.reassign_window_to_authoritative_space(window, space) {
            outcome.with_arrange_passes(1)
        } else {
            outcome
        }
    }

    fn get_active_drag_session(&self) -> Option<&DragSession> {
        if let DragState::Active { session } = &self.drag_manager.drag_state {
            Some(session)
        } else {
            None
        }
    }

    /// The drag session in whichever state carries one. `Active` and
    /// `PendingSwap` alternate on every evaluation during a drag over a
    /// target, so anything consulted per pointer move has to see the session
    /// in both — reading it through `get_active_drag_session` made the
    /// origin hint vanish on exactly every other sample, and the overlay
    /// blinked at report rate.
    fn current_drag_session(&self) -> Option<&DragSession> {
        match &self.drag_manager.drag_state {
            DragState::Active { session } | DragState::PendingSwap { session, .. } => Some(session),
            DragState::Inactive => None,
        }
    }

    fn take_active_drag_session(&mut self) -> Option<DragSession> {
        match std::mem::replace(&mut self.drag_manager.drag_state, DragState::Inactive) {
            DragState::Active { session } => Some(session),
            DragState::PendingSwap { session, .. } => Some(session),
            _ => None,
        }
    }

    async fn run(reactor: Reactor, events: Receiver, events_tx: Sender) {
        let (raise_manager_tx, raise_manager_rx) = actor::channel();
        let (animation_tx, animation_rx) = tokio::sync::mpsc::unbounded_channel();
        let reactor = Rc::new(RefCell::new(reactor));
        let event_tap_tx = {
            let mut reactor = reactor.borrow_mut();
            reactor.communication_manager.raise_manager_tx = raise_manager_tx.clone();
            reactor.animation_tx = Some(animation_tx);
            reactor.communication_manager.event_tap_tx.clone()
        };
        let reactor_task = Self::run_reactor_loop(reactor, events);
        let raise_manager_task = RaiseManager::run(raise_manager_rx, events_tx, event_tap_tx);
        let animation_task = animation::AnimationManager::run(animation_rx);
        let _ = tokio::join!(reactor_task, raise_manager_task, animation_task);
    }

    async fn run_reactor_loop(reactor: Rc<RefCell<Reactor>>, mut events: Receiver) {
        const MAX_EVENT_BATCH: usize = 64;

        while let Some((span, event)) = events.recv().await {
            let _guard = span.enter();
            Self::handle_thread_event(&reactor, event);
            // Drain a bounded batch to reduce recv/select overhead.
            for _ in 1..MAX_EVENT_BATCH {
                let Ok((span, event)) = events.try_recv() else {
                    break;
                };
                let _guard = span.enter();
                Self::handle_thread_event(&reactor, event);
            }
        }
    }

    fn handle_thread_event(reactor: &Rc<RefCell<Reactor>>, event: Event) {
        match event {
            Event::InstallIpc(request) => crate::ipc::install_mach_server(reactor.clone(), request),
            event => reactor.borrow_mut().handle_loop_event(event),
        }
    }

    fn handle_loop_event(&mut self, event: Event) {
        if let Event::Query(req) = event {
            self.handle_query_request(req);
            return;
        }
        if self.should_quarantine_space_lifecycle_event(&event) {
            trace!(?event, state = ?self.refresh_quarantine_state(), "quarantined space lifecycle event");
            return;
        }
        if self.should_quarantine_during_display_churn(&event) {
            trace!(?event, "quarantined during display churn");
            return;
        }
        Self::note_windowserver_activity(&event);
        self.handle_event(event);
        #[cfg(any(test, debug_assertions))]
        self.state.windows.debug_assert_invariants();
    }

    /// Runs one command from the CLI, reporting why it did nothing if it did.
    ///
    /// Most commands cannot fail in a way the caller can act on. The ones that
    /// can are the three that need the scripting addition: without it there is
    /// no way to move a window between spaces or to create or destroy one, and
    /// a CLI that answers "Command executed successfully" to a command that
    /// did nothing is the worst of the failure modes -- it sends you looking
    /// anywhere but at the addition.
    pub(crate) fn handle_ipc_command(&mut self, command: Command) -> Result<(), String> {
        self.command_error = None;
        self.handle_loop_event(Event::Command(command));
        match self.command_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Records why the command in flight could not be carried out.
    fn fail_command(&mut self, message: impl Into<String>) {
        let message = message.into();
        warn!("{message}");
        self.command_error = Some(message);
    }

    fn note_windowserver_activity(event: &Event) {
        let wsid = match event {
            Event::WindowFrameChanged(wid, ..) => Some(wid.idx.get()),
            Event::WindowCreated(wid, ..) => Some(wid.idx.get()),
            Event::WindowDestroyed(wid) => Some(wid.idx.get()),
            Event::WindowMinimized(wid) => Some(wid.idx.get()),
            Event::WindowDeminiaturized(wid) => Some(wid.idx.get()),
            Event::MouseMoved(..) => None,
            Event::WindowServerDestroyed(wsid, ..) => Some(wsid.as_u32()),
            Event::WindowServerAppeared(wsid, ..) => Some(wsid.as_u32()),
            _ => None,
        };
        if let Some(wsid) = wsid {
            window_server::note_windowserver_activity(wsid);
        }
    }

    fn log_event(&self, event: &Event) {
        match event {
            Event::WindowFrameChanged(..)
            | Event::MouseUp
            | Event::MouseMoved(..)
            | Event::MouseDragged { .. } => {
                trace!(?event, "Event")
            }
            _ => debug!(?event, "Event"),
        }
    }

    fn should_update_notifications(event: &Event) -> bool {
        matches!(
            event,
            Event::WindowCreated(..)
                | Event::WindowDestroyed(..)
                | Event::WindowServerDestroyed(..)
                | Event::WindowServerAppeared(..)
                | Event::WindowsDiscovered { .. }
                | Event::ApplicationLaunched { .. }
                | Event::ApplicationTerminated(..)
                | Event::ApplicationThreadTerminated(..)
                | Event::SpaceStateChanged(..)
        )
    }

    fn should_quarantine_during_display_churn(&self, event: &Event) -> bool {
        if !crate::sys::display_churn::is_active() {
            return false;
        }

        matches!(
            event,
            Event::WindowCreated(..)
                | Event::WindowDestroyed(..)
                | Event::WindowServerDestroyed(..)
                | Event::WindowServerAppeared(..)
                | Event::WindowFrameChanged(..)
                | Event::WindowMinimized(..)
                | Event::WindowDeminiaturized(..)
                | Event::WindowTitleChanged(..)
                | Event::WindowsDiscovered { .. }
                | Event::SpaceCreated(..)
                | Event::SpaceDestroyed(..)
        )
    }

    fn should_quarantine_space_lifecycle_event(&self, event: &Event) -> bool {
        self.refreshes_blocked()
            && matches!(event, Event::SpaceCreated(..) | Event::SpaceDestroyed(..))
    }

    fn refresh_quarantine_state(&self) -> RefreshQuarantineState {
        self.refresh_quarantine_manager.state()
    }

    fn refreshes_blocked(&self) -> bool { self.refresh_quarantine_manager.blocks_refreshes() }

    fn defer_window_inventory_refresh(&mut self) {
        self.refresh_quarantine_manager.pending_inventory_refresh = true;
    }

    fn flush_deferred_window_inventory_refresh(&mut self) {
        if self.refreshes_blocked() {
            return;
        }

        if self.refresh_quarantine_manager.pending_inventory_refresh {
            self.refresh_quarantine_manager.pending_inventory_refresh = false;
            self.request_window_inventories();
        }
    }

    // All lifecycle churn is upstreamed through the spaces actor. The reactor
    // only remembers that one visibility refresh is owed, then flushes it once
    // every upstream gate is open again.
    fn request_refresh_when_spaces_actor_stabilizes(&mut self) {
        self.defer_window_inventory_refresh();
        self.flush_deferred_window_inventory_refresh();
    }

    fn release_post_instability_quarantine_after_authoritative_snapshot(&mut self) {
        let released_wake = self.refresh_quarantine_manager.awaiting_post_wake_snapshot;
        let released_session = self.refresh_quarantine_manager.awaiting_post_session_snapshot;

        if !released_wake && !released_session {
            return;
        }

        self.refresh_quarantine_manager.awaiting_post_wake_snapshot = false;
        self.refresh_quarantine_manager.awaiting_post_session_snapshot = false;
        if released_wake {
            self.refresh_quarantine_manager.sleeping = false;
        }
        if released_session {
            self.refresh_quarantine_manager.session_inactive = false;
        }
        self.flush_deferred_window_inventory_refresh();
    }

    #[instrument(name = "reactor::handle_event", skip(self), fields(event=?event))]
    fn handle_event(&mut self, event: Event) {
        let previously_focused_window = self.main_window();
        let was_mouse_up = matches!(event, Event::MouseUp);
        match self.dispatch_workflow(event) {
            Ok(mut outcome) => {
                let focused_window = self.main_window();
                if focused_window != previously_focused_window {
                    match focused_window {
                        Some(focused_window) => {
                            outcome = outcome.with_focused_window_broadcast(focused_window);
                            let returning = previously_focused_window.is_none()
                                && self.focus_left_from == Some(focused_window);
                            self.focus_left_from = None;
                            if !returning {
                                self.follow_focus_with_mouse(focused_window, &mut outcome);
                            }
                            // A window the user brings to the front that rift
                            // turned away is looked at again: what disqualified
                            // it may have been the state it opened in.
                            self.readmit_rejected_window(focused_window);
                        }
                        None => self.focus_left_from = previously_focused_window,
                    }
                } else if was_mouse_up && let Some(focused_window) = focused_window {
                    self.follow_dock_click_with_mouse(focused_window, &mut outcome);
                }
                if let Some(homing) = self.advance_display_homing() {
                    outcome.absorb(homing);
                }
                self.release_drop_pin_if_landed();
                self.assert_seam_finish();
                self.apply_event_outcome(outcome);
            }
            Err(error) => warn!(%error, "reactor workflow failed"),
        }
        // The overlay is a statement about a pending drop, so it lives exactly
        // as long as one does. Every way a drag can end other than a drop —
        // a participant destroyed, Mission Control opening, the window
        // crossing to another space, a lost session — resets the drag state
        // somewhere in a workflow, and none of them know about the overlay.
        // Reconciling here means none of them can strand it on screen.
        if self.drag_manager.drop_overlay_shown
            && !matches!(self.drag_manager.drag_state, DragState::PendingSwap { .. })
        {
            self.hide_drop_region();
        }
    }

    fn dispatch_workflow(&mut self, event: Event) -> anyhow::Result<EventOutcome> {
        crate::sys::trace::mark_reactor_thread();
        self.log_event(&event);
        self.recording_manager.record.on_event(&event);

        // Wake/unlock produces synthetic activation notifications as loginwindow
        // yields focus back to the pre-sleep application. Only real input makes
        // a subsequent activation a trustworthy request to follow an app to a
        // different virtual workspace.
        if matches!(event, Event::MouseUp | Event::MouseMoved(_) | Event::Command(_)) {
            self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input = false;
        }

        match event {
            Event::SystemWillSleep => {
                self.refresh_quarantine_manager.sleeping = true;
                self.refresh_quarantine_manager.awaiting_post_wake_snapshot = false;
                return Ok(EventOutcome::default());
            }
            Event::SystemWoke => {
                self.refresh_quarantine_manager.sleeping = true;
                self.refresh_quarantine_manager.awaiting_post_wake_snapshot = true;
                self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input = true;
                let outcome = system_workflow::handle_system_woke()?;
                self.defer_window_inventory_refresh();
                return Ok(outcome);
            }
            Event::SessionDidResignActive => {
                self.refresh_quarantine_manager.session_inactive = true;
                self.refresh_quarantine_manager.awaiting_post_session_snapshot = false;
                return Ok(EventOutcome::default());
            }
            Event::SessionDidBecomeActive => {
                self.refresh_quarantine_manager.session_inactive = true;
                self.refresh_quarantine_manager.awaiting_post_session_snapshot = true;
                self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input = true;
                self.defer_window_inventory_refresh();
                return Ok(EventOutcome::default());
            }
            Event::DisplayChurnBegin => {
                self.refresh_quarantine_manager.display_churn_active = true;
                return Ok(EventOutcome::default());
            }
            Event::DisplayChurnEnd => {
                self.refresh_quarantine_manager.display_churn_active = false;
                self.request_refresh_when_spaces_actor_stabilizes();
                return Ok(EventOutcome::default());
            }
            Event::DisplayHomingDeadline(uuid) => {
                return Ok(self.handle_display_homing_deadline(&uuid));
            }
            _ => {}
        }
        self.note_explicit_window_intent(&event);
        self.sync_drag_notification_silence();
        self.sync_float_drag_strips();
        // Focus reported on a window that is not admitted — an app's child
        // window, such as Lightroom's filmstrip, which the window server and
        // AX both happily report as focused — is focus on the top-level
        // window around it, as far as anything downstream is concerned:
        // layout focus, the main-window tracker, commands, the pointer.
        let event = match event {
            Event::WindowServerFocusChanged(window, space) => {
                Event::WindowServerFocusChanged(self.admitted_root_for(window), space)
            }
            Event::ApplicationMainWindowChanged(pid, Some(window), quiet) => {
                Event::ApplicationMainWindowChanged(
                    pid,
                    Some(self.admitted_root_for(window)),
                    quiet,
                )
            }
            other => other,
        };

        let should_update_notifications = Self::should_update_notifications(&event);
        let duplicate_global_activation = matches!(
            &event,
            Event::ApplicationGloballyActivated(pid)
                if self.main_window_tracker.is_globally_frontmost(*pid)
        );

        let raised_window = self.main_window_tracker.handle_event(&event);
        match event {
            Event::ApplicationLaunched {
                pid,
                info,
                handle,
                visible_windows,
                window_server_info,
                is_frontmost,
                main_window,
            } => {
                let _ = (is_frontmost, main_window);
                self.forget_window_inventory(pid);
                let mut outcome = application_workflow::handle_application_launched(
                    &mut self.app_manager,
                    application_workflow::ApplicationLaunchedPayload {
                        pid,
                        info,
                        handle,
                        visible_windows,
                        window_server_info,
                    },
                )?;
                if self.main_window_tracker.is_globally_frontmost(pid) {
                    outcome.app_requests.push((pid, Request::ApplicationGloballyActivated(pid)));
                }
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::ApplicationTerminated(pid) => {
                return application_workflow::handle_application_terminated(pid);
            }
            Event::ApplicationThreadTerminated(pid) => {
                self.forget_window_inventory(pid);
                self.clear_menu_state_for_pid(pid);
                return application_workflow::handle_application_thread_terminated(
                    &mut self.app_manager,
                    pid,
                );
            }
            Event::ApplicationActivated(pid, quiet) => {
                self.clear_menu_state_for_non_owner(pid);
                let mut outcome = application_workflow::handle_application_activated(
                    application_workflow::ApplicationActivatedPayload { pid, quiet },
                )?;
                if quiet == Quiet::No {
                    outcome.absorb(self.handle_app_activation_workspace_switch(pid));
                }
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::ApplicationDeactivated(pid) => {
                self.clear_menu_state_for_pid(pid);
            }
            Event::ApplicationGloballyDeactivated(pid) => {
                self.clear_menu_state_for_pid(pid);
            }
            Event::ApplicationGloballyActivated(pid) => {
                if duplicate_global_activation {
                    trace!(pid, "Ignoring duplicate global application activation");
                    return Ok(EventOutcome::focus_changed(None, should_update_notifications));
                }
                self.clear_menu_state_for_non_owner(pid);
                if !self.is_login_window_pid(pid) {
                    if let Some(app) = self.app_manager.apps.get(&pid) {
                        let _ = app.handle.send(Request::ApplicationGloballyActivated(pid));
                    }
                }
                // The app thread will resolve the current AX main window and
                // emit ApplicationActivated. Do not replay cached focus here.
                return Ok(EventOutcome::focus_changed(None, should_update_notifications));
            }
            Event::WindowServerFocusChanged(window, reported_space) => {
                if self.layout_manager.layout_engine.focused_window() == Some(window) {
                    if let Some(event_tap_tx) = &self.communication_manager.event_tap_tx {
                        _ = event_tap_tx.send(crate::actor::event_tap::Request::EnforceHidden);
                    }
                    return Ok(EventOutcome::default());
                }
                if !self.state.windows.contains_window(window) {
                    self.request_window_inventory(window.pid);
                    return Ok(EventOutcome::default());
                }
                return Ok(if self.is_space_active(reported_space) {
                    EventOutcome::default()
                        .with_layout_event(LayoutEvent::WindowFocused(reported_space, window))
                } else {
                    EventOutcome::default()
                });
            }
            Event::RegisterWmSender(sender) => {
                return Ok(system_workflow::handle_register_wm_sender(
                    &mut self.communication_manager,
                    sender,
                )?);
            }
            Event::WindowInventoryRefreshRequested(pid) => {
                self.request_window_inventory(pid);
                return Ok(EventOutcome::default());
            }
            Event::RaiseTargetsMissing { windows, sequence_id } => {
                let Some(first) = windows.first() else {
                    return Ok(EventOutcome::default());
                };
                self.window_inventory_manager.refocus_after_refresh.insert(first.pid, *first);
                self.request_window_inventory(first.pid);
                let mut outcome = EventOutcome::default();
                for window_id in windows {
                    outcome
                        .raise_requests
                        .push(raise_manager::Event::RaiseCompleted { window_id, sequence_id });
                }
                return Ok(outcome);
            }
            Event::WindowsDiscovered {
                pid,
                token,
                successful,
                new,
                known_visible,
            } => {
                if !self.finish_window_inventory(pid, token, successful) {
                    debug!(pid, ?token, successful, "Discarding stale AX window inventory");
                    return Ok(EventOutcome::default());
                }
                let refocus = self
                    .window_inventory_manager
                    .refocus_after_refresh
                    .remove(&pid)
                    .is_some_and(|target| new.iter().any(|(wid, _)| *wid == target));
                let mut outcome = application_workflow::handle_windows_discovered(
                    application_workflow::WindowsDiscoveredPayload { pid, new, known_visible },
                )?;
                if refocus {
                    outcome = outcome.with_arrange_passes(1);
                }
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::WindowCreated(wid, window, ws_info, mouse_state) => {
                let _ = mouse_state;
                let mut outcome = window_workflow::handle_window_created(
                    &mut self.state,
                    &self.transaction_manager,
                    window_workflow::WindowCreatedPayload {
                        window_id: wid,
                        window,
                        window_server_info: ws_info,
                    },
                )?;
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::WindowDestroyed(wid) => {
                // macOS can replace AXUIElements during lifecycle/display churn while the
                // native window remains alive. Recovery already schedules a stable refresh,
                // so preserve topology until then. Outside churn, retain the original AX
                // destruction behavior and remove the window immediately.
                if self.refreshes_blocked() {
                    return Ok(EventOutcome::default());
                }

                let mut outcome = window_workflow::handle_window_destroyed(
                    &mut self.state,
                    &self.transaction_manager,
                    &mut self.drag_manager,
                    window_workflow::WindowDestroyedPayload { window: wid },
                )?;
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::WindowServerDestroyed(wsid, sid, kind) => {
                let tracked_window = self.state.windows.tracked_window_id(wsid);
                let assigned_space =
                    tracked_window.and_then(|window| self.assigned_space_for_window_id(window));
                let last_known_user_space = topology_workflow::resolve_last_known_user_space(
                    tracked_window.and_then(|window| self.best_space_for_window_id(window)),
                    self.space_state.iter_known_spaces().next(),
                );
                let observations = topology_workflow::WindowServerDestroyedObservations {
                    resolved_space: self.resolve_native_space(wsid, None),
                    active_spaces: self.active_spaces.clone(),
                    mission_control_active: self.is_mission_control_active(),
                    ordered_in: window_server::window_ordered_in(wsid),
                    still_known: window_server::get_window(wsid).is_some(),
                    assigned_space,
                    last_known_user_space,
                };
                return topology_workflow::handle_window_server_destroyed(
                    &mut self.state,
                    &self.transaction_manager,
                    &mut self.drag_manager,
                    topology_workflow::WindowServerLifecyclePayload {
                        window_server_id: wsid,
                        space: sid,
                        kind,
                    },
                    observations,
                );
            }
            Event::WindowServerAppeared(wsid, sid, kind) => {
                let tracked_window = self.state.windows.tracked_window_id(wsid);
                let assigned_space =
                    tracked_window.and_then(|window| self.assigned_space_for_window_id(window));
                let last_known_user_space = topology_workflow::resolve_last_known_user_space(
                    tracked_window.and_then(|window| self.best_space_for_window_id(window)),
                    self.space_state.iter_known_spaces().next(),
                );
                let window_server_info = window_server::get_window(wsid);
                let owner_pid = window_server_info.as_ref().map(|info| info.pid);
                let app_known =
                    owner_pid.is_some_and(|pid| self.app_manager.apps.contains_key(&pid));
                let running_app_info = owner_pid.filter(|_| !app_known).and_then(|pid| {
                    objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(
                        pid,
                    )
                    .map(|app| AppInfo::from(&*app))
                });
                let observations = topology_workflow::WindowServerAppearedObservations {
                    resolved_space: self.resolve_native_space(wsid, Some(sid)),
                    active_spaces: self.active_spaces.clone(),
                    mission_control_active: self.is_mission_control_active(),
                    assigned_space,
                    last_known_user_space,
                    window_server_info,
                    app_known,
                    running_app_info,
                };
                if matches!(kind, SpaceEventKind::User)
                    && let Some(wid) = tracked_window
                {
                    self.note_window_appeared_while_away(wid, sid);
                }
                return topology_workflow::handle_window_server_appeared(
                    &mut self.state,
                    topology_workflow::WindowServerLifecyclePayload {
                        window_server_id: wsid,
                        space: sid,
                        kind,
                    },
                    observations,
                );
            }
            Event::SpaceCreated(space) => {
                return topology_workflow::handle_space_lifecycle(
                    &mut self.space_activation_policy,
                    topology_workflow::SpaceLifecyclePayload { space, created: true },
                );
            }
            Event::SpaceDestroyed(space) => {
                return topology_workflow::handle_space_lifecycle(
                    &mut self.space_activation_policy,
                    topology_workflow::SpaceLifecyclePayload { space, created: false },
                );
            }
            Event::WindowMinimized(wid) => {
                return window_workflow::handle_window_minimized(&mut self.state, wid);
            }
            Event::WindowDeminiaturized(wid) => {
                let active_space = self.state.windows.window(wid).and_then(|window| {
                    self.best_space_for_window(&window.frame_monotonic, window.info.sys_id)
                        .filter(|space| self.is_space_active(*space))
                        .or_else(|| {
                            window
                                .info
                                .sys_id
                                .is_none()
                                .then(|| self.workspace_command_space())
                                .flatten()
                        })
                });
                return window_workflow::handle_window_deminiaturized(
                    &mut self.state,
                    window_workflow::WindowDeminiaturizedPayload { window: wid, active_space },
                );
            }
            Event::WindowFrameChanged(wid, new_frame, last_seen, requested, mouse_state) => {
                let mission_control_active = self.is_mission_control_active();
                let mut effective_mouse_state = mouse_state;
                // A modifier drag is rift resizing a window from the pointer:
                // the button is down, but every report — the window's and
                // its neighbours', which the same arranges write — is the
                // echo of rift's own write, not the user moving a window.
                // Read as a drag it held the reporting window, and a held
                // window is not laid out: the neighbour stopped following
                // while the resized window went on, leaving a gap.
                if self.modifier_drag.is_some() {
                    effective_mouse_state = Some(crate::sys::event::MouseState::Up);
                }
                // Noted before the transaction gate below can discard the
                // report: the button is down and this window is moving.
                if effective_mouse_state == Some(crate::sys::event::MouseState::Down)
                    && self.state.windows.contains_window(wid)
                {
                    self.drag_manager.held_window = Some(wid);
                }
                // Whatever else this report means, the window is this size
                // now, so no recorded minimum may claim it cannot be.
                self.layout_manager.layout_engine.relax_observed_min_size(wid, new_frame.size);
                let disposition = window_workflow::classify_window_frame_change(
                    &mut self.state,
                    &self.transaction_manager,
                    &mut self.drag_manager,
                    wid,
                    new_frame,
                    last_seen,
                    requested.0,
                    &mut effective_mouse_state,
                    mission_control_active,
                );
                if !matches!(
                    disposition,
                    window_workflow::FrameChangeDisposition::NeedsGeometryAnalysis
                ) {
                    let mut outcome = EventOutcome::no_change();
                    // A window that would not shrink to its slot is sitting
                    // over its neighbour. Remember the size it insisted on
                    // and lay out again, so the slot grows to fit it and the
                    // neighbour gives way instead.
                    // A "refusal" larger than the display the window is on
                    // is not a minimum the app insists on: it is the frame
                    // the window still had when the app reported halfway
                    // through applying a move (origin first, size after —
                    // a window just sent from a larger display). Recording
                    // it squeezed the neighbour to nothing.
                    let fits_display = |observed: CGSize| {
                        self.best_space_for_window_id(wid)
                            .and_then(|space| self.space_state.screen_by_space(space))
                            .is_none_or(|screen| {
                                observed.width <= screen.frame.size.width
                                    && observed.height <= screen.frame.size.height
                            })
                    };
                    if let window_workflow::FrameChangeDisposition::HandledRefusedSize {
                        requested,
                        observed,
                    } = disposition
                        && fits_display(observed)
                        && self
                            .layout_manager
                            .layout_engine
                            .note_observed_min_size(wid, requested, observed)
                    {
                        debug!(
                            ?wid,
                            ?requested,
                            ?observed,
                            "window refused its size; treating it as a minimum"
                        );
                        outcome = outcome.with_arrange_passes(1);
                    }
                    outcome.dispatch_mouse_up = effective_mouse_state
                        == Some(crate::sys::event::MouseState::Up)
                        && matches!(
                            self.drag_manager.drag_state,
                            DragState::Active { .. } | DragState::PendingSwap { .. }
                        );
                    outcome.focused_window = raised_window;
                    self.note_frame_change_for_flight_recorder(
                        wid,
                        effective_mouse_state,
                        "gate",
                        None,
                    );
                    return Ok(outcome);
                }
                let (server_id, old_frame) = self
                    .state
                    .windows
                    .window(wid)
                    .map(|window| (window.info.sys_id, window.frame_monotonic))
                    .unwrap_or((None, new_frame));
                let mut old_space = self.geometry_space_for_window(&old_frame, server_id);
                let mut new_space = self.geometry_space_for_window(&new_frame, server_id);
                // A window the window server has on a desktop that is not
                // being shown has not changed desktops by moving: nothing on
                // a hidden desktop is dragged, and a change of desktop is
                // reported by the server, not by a frame. Its frame only
                // says which display it will be on when shown, and after a
                // display change that can differ from where the frame was
                // before; read as a cross-space move, that pulled the window
                // into the visible tree of the display it landed on, beside
                // the windows really there, until discovery put it back.
                let hidden_desktop = server_id
                    .and_then(window_server::window_space)
                    .filter(|space| !self.is_space_active(*space))
                    .filter(|space| {
                        self.space_state.display_space_ids.values().flatten().any(|s| s == space)
                    });
                if let Some(space) = hidden_desktop {
                    old_space = Some(space);
                    new_space = Some(space);
                }
                let old_space_active = old_space.is_some_and(|space| self.is_space_active(space));
                let new_space_active = new_space.is_some_and(|space| self.is_space_active(space));
                let best_resize_space = self.best_space_for_window(&new_frame, server_id);
                let active_resize_space =
                    best_resize_space.filter(|space| self.is_space_active(*space)).or_else(|| {
                        server_id.is_none().then(|| self.workspace_command_space()).flatten()
                    });
                let pending_target_space = server_id
                    .and_then(|server| self.pending_target_space_for_window_server_id(server));
                let assigned_space = self.assigned_space_for_window_id(wid);
                let keep_assigned_for_scrolling = old_space.is_some_and(|space| {
                    self.layout_manager.layout_engine.active_layout_mode_at(space)
                        == crate::common::config::LayoutMode::Scrolling
                        && !self.layout_manager.layout_engine.is_window_floating(wid)
                        && self
                            .layout_manager
                            .layout_engine
                            .virtual_workspace_manager()
                            .workspace_for_window(&self.state.windows, space, wid)
                            .is_some()
                });
                let screens = self
                    .space_state
                    .screens
                    .iter()
                    .filter_map(|screen| {
                        Some((screen.space?, screen.frame, screen.display_uuid_owned()))
                    })
                    .collect();
                let mut outcome = window_workflow::handle_window_frame_changed(
                    &mut self.state,
                    &mut self.layout_manager,
                    &mut self.drag_manager,
                    window_workflow::WindowFrameChangedPayload {
                        window: wid,
                        new_frame,
                        mouse_state: effective_mouse_state,
                        old_space,
                        new_space,
                        old_space_active,
                        new_space_active,
                        active_resize_space,
                        pending_target_space,
                        assigned_space,
                        keep_assigned_for_scrolling,
                        screens,
                    },
                )?;
                // Frame acknowledgements and no-op geometry changes can return
                // early from the reducer. Mouse release still has to terminate
                // an existing drag session in those cases.
                if effective_mouse_state == Some(crate::sys::event::MouseState::Up)
                    && matches!(
                        self.drag_manager.drag_state,
                        DragState::Active { .. } | DragState::PendingSwap { .. }
                    )
                {
                    outcome.dispatch_mouse_up = true;
                }
                outcome.focused_window = raised_window;
                self.note_frame_change_for_flight_recorder(
                    wid,
                    effective_mouse_state,
                    "geometry",
                    Some((old_space, new_space, old_space_active, new_space_active)),
                );
                return Ok(outcome);
            }
            Event::WindowTitleChanged(wid, new_title) => {
                let mut outcome = window_workflow::handle_window_title_changed(
                    &mut self.state,
                    window_workflow::WindowTitleChangedPayload { window: wid, title: new_title },
                )?;
                outcome.focused_window = raised_window;
                return Ok(outcome);
            }
            Event::SpaceStateChanged(space_state) => {
                self.advance_window_inventory_revision_if_needed(&space_state);
                let releases_lifecycle_refresh_quarantine =
                    space_state.releases_lifecycle_refresh_quarantine;
                let releases_display_churn_refresh_quarantine =
                    space_state.releases_display_churn_refresh_quarantine;
                let releases_instability = (releases_lifecycle_refresh_quarantine
                    && (self.refresh_quarantine_manager.awaiting_post_wake_snapshot
                        || self.refresh_quarantine_manager.awaiting_post_session_snapshot))
                    || (releases_display_churn_refresh_quarantine
                        && self.refresh_quarantine_manager.display_churn_active);
                if releases_instability {
                    self.abandon_window_inventories_from_instability();
                }
                let mut outcome = self.handle_authoritative_space_snapshot(space_state)?;
                if releases_lifecycle_refresh_quarantine {
                    self.release_post_instability_quarantine_after_authoritative_snapshot();
                }
                if releases_display_churn_refresh_quarantine {
                    self.refresh_quarantine_manager.display_churn_active = false;
                    self.request_refresh_when_spaces_actor_stabilizes();
                }
                if releases_instability {
                    // Releasing either recovery gate already flushes the deferred
                    // all-app refresh. A topology-changing snapshot requests the
                    // same refresh through its outcome; leave only one request per
                    // app instead of immediately scheduling a redundant follow-up.
                    outcome.refresh_window_inventories = false;
                }
                return Ok(outcome);
            }
            Event::ActiveDisplayChanged { menu_bar_space, command_space } => {
                self.space_state.menu_bar_space = menu_bar_space;
                self.space_state.command_space = command_space;
                return Ok(EventOutcome::default());
            }
            Event::MouseUp => {
                // A modifier drag ends with the button. Left set, it kept
                // reading the window's later reports as echoes and kept
                // mouse-follows-focus off for good.
                let ended_modifier_drag = self.modifier_drag.take();
                // A rift-driven float move (a takeover, or an alt-drag) that
                // ends straddling the display seam needs the same finish a
                // reported drag's drop gets: the system relocates straddling
                // programmatic placements to a display of its own choosing.
                if let Some(drag) = ended_modifier_drag
                    && matches!(drag.action, crate::common::config::MouseAction::Move)
                    && self.layout_manager.layout_engine.is_window_floating(drag.window)
                    && let Some(state) = self.state.windows.window(drag.window)
                {
                    let screens: Vec<CGRect> =
                        self.space_state.screens.iter().map(|screen| screen.frame).collect();
                    let pointer = window_server::current_cursor_location().ok();
                    let dropped_at = state.frame_monotonic;
                    if crate::actor::reactor::events::drag::seam_fitted(
                        &screens, pointer, dropped_at,
                    )
                    .is_some()
                    {
                        self.drag_manager.seam_finish = Some(managers::SeamFinish {
                            window: drag.window,
                            dropped_at,
                            pointer,
                            fitted: None,
                            at: crate::sys::trace::now(),
                            attempts: 0,
                        });
                    }
                }
                self.last_mouse_up = Some(crate::sys::trace::now());
                let held = self.drag_manager.held_window.take();
                let pending_swap = self.get_pending_drag_swap();
                let (visible_spaces, visible_space_centers) = self.visible_spaces_for_layout(true);
                // A drop on a target is a statement about that target's
                // space, whatever the dragged window's own frame says. Near
                // the edge of a display the frame hangs over onto the next
                // one, and the window server may already have handed the
                // window to that display's space; resolving the drop from
                // the dragged window then tried the insert in a tree the
                // target is not in, fell back to a no-op swap, and moved the
                // window to the other display — while the arrange for the
                // display it was dropped on pulled it back. That is the
                // flicker between displays that ended with the window tiled
                // on the wrong one.
                let target_space =
                    pending_swap.and_then(|(_, target)| self.best_space_for_window_id(target));
                // And that statement has to outlive the drop by a beat: the
                // window server's own report that the window moved to the
                // display it hung over arrives after this, while the arrange
                // is still moving it back. See `managers::DropPin`.
                if let (Some((dragged, _)), Some(space)) = (pending_swap, target_space)
                    && let Some(wsid) =
                        self.state.windows.window(dragged).and_then(|window| window.info.sys_id)
                {
                    let now = crate::sys::trace::now();
                    self.drag_manager.drop_pin = Some(managers::DropPin {
                        window: wsid,
                        space,
                        until: now + managers::DropPin::HOLD,
                        next_probe: now + managers::DropPin::PROBE_EVERY,
                    });
                }
                let swap_space = target_space
                    .or_else(|| {
                        pending_swap.and_then(|(dragged, _)| {
                            self.state.windows.window(dragged).and_then(|window| {
                                self.best_space_for_window(
                                    &window.frame_monotonic,
                                    window.info.sys_id,
                                )
                            })
                        })
                    })
                    .or_else(|| {
                        self.drag_manager
                            .drag_swap_manager
                            .origin_frame()
                            .and_then(|frame| self.best_space_for_frame(&frame))
                    })
                    .or_else(|| self.space_state.screens.iter().find_map(|screen| screen.space));
                let session = match &self.drag_manager.drag_state {
                    DragState::Active { session } | DragState::PendingSwap { session, .. } => {
                        Some(session.clone())
                    }
                    DragState::Inactive => None,
                };
                // A dragged window goes where the pointer lets go of it. The
                // window's own frame is a poor witness: it is only ever as
                // far onto the next display as the user got it, and deciding
                // by its centre sent a window the user had pulled a fifth of
                // the way onto the other display back to where it came from.
                let pointer = window_server::current_cursor_location().ok();
                let pointer_space = session.as_ref().and_then(|_| {
                    let cursor = pointer?;
                    self.screen_for_point(cursor).and_then(|screen| screen.space)
                });
                let final_space = target_space.or(pointer_space).or_else(|| {
                    session.as_ref().and_then(|session| {
                        session
                            .settled_space
                            .or_else(|| self.best_space_for_frame(&session.last_frame))
                            .or_else(|| self.best_space_for_window_id(session.window))
                    })
                });
                // Where the pointer sits inside the target decides whether the
                // drop swaps or splits, so it has to be read before the drag
                // state is torn down.
                // The same resolution the preview used, so the drop does what
                // the overlay showed.
                let drop_action = pending_swap.and_then(|(dragged, target)| {
                    // What the overlay last showed is the promise the drop
                    // keeps; the cursor is only consulted when there was no
                    // preview (the overlay disabled, or no region to draw).
                    if let Some(cached) = self
                        .drag_manager
                        .drop_preview_cache
                        .filter(|cached| cached.dragged == dragged && cached.target == target)
                    {
                        return Some(cached.action);
                    }
                    let cursor = window_server::current_cursor_location().ok()?;
                    self.drop_action_for(dragged, target, cursor)
                });
                self.hide_drop_region();
                let focused = self.window_id_under_cursor().and_then(|window| {
                    self.best_space_for_window_id(window).map(|space| (space, window))
                });
                let mut outcome = interaction_workflow::handle_mouse_up(
                    &mut self.state,
                    &mut self.layout_manager,
                    &mut self.drag_manager,
                    interaction_workflow::MouseUpPayload {
                        pending_swap,
                        drop_action,
                        swap_space,
                        final_space,
                        visible_spaces,
                        visible_space_centers,
                        screens: self
                            .space_state
                            .screens
                            .iter()
                            .map(|screen| screen.frame)
                            .collect(),
                        pointer,
                    },
                )?;
                if let Some(window) = held
                    && session.as_ref().map(|session| session.window) != Some(window)
                {
                    outcome.absorb(self.settle_held_window(window));
                }
                if let Some((space, window)) = focused {
                    outcome = outcome.with_layout_event(LayoutEvent::WindowFocused(space, window));
                }
                if crate::sys::trace::is_recording() {
                    crate::sys::trace::act(
                        "drop",
                        &serde_json::json!({
                            "held": held.map(|w| w.idx.get()),
                            "session": session.as_ref().map(|s| (s.window.idx.get(), s.origin_space, s.settled_space, s.layout_dirty)),
                            "pending_swap": pending_swap.map(|(a, b)| (a.idx.get(), b.idx.get())),
                            "final_space": final_space,
                            "passes": outcome.arrange.passes,
                        }),
                    );
                }
                // The drag is over; a refresh sweep deferred while it ran is
                // due now, as one census instead of one per space flip.
                self.flush_deferred_window_inventory_refresh();
                return Ok(outcome);
            }
            Event::MenuOpened(pid) => {
                return Ok(system_workflow::handle_menu_opened(&mut self.menu_manager, pid)?);
            }
            Event::MenuClosed(pid) => {
                return Ok(system_workflow::handle_menu_closed(&mut self.menu_manager, pid)?);
            }
            Event::WindowServerVisibilityChanged(wsid, visible) => {
                let mut outcome = EventOutcome::no_change();
                if let Some(wid) = self.state.windows.tracked_window_id(wsid) {
                    if visible {
                        self.state.windows.mark_window_visible(wsid);
                    } else {
                        self.state.windows.mark_window_hidden(wsid);
                        // Being ordered out is not proof of anything on its own:
                        // it is also what every window on a space does when you
                        // switch away from it. Ask the app whether the window's
                        // accessibility element is still valid, which only a
                        // genuinely destroyed window fails. If it is gone the
                        // app emits WindowDestroyed and the ordinary teardown
                        // runs, so nothing here has to guess.
                        outcome =
                            outcome.with_app_request(wid.pid, Request::VerifyWindowAlive(wid));
                    }
                }
                return Ok(outcome);
            }
            Event::MouseModifierDragBegin { window, at, action } => {
                self.begin_mouse_modifier_drag(window, at, action);
                return Ok(EventOutcome::default());
            }
            Event::MouseModifierDrag { dx, dy } => {
                let outcome = EventOutcome::default();
                return Ok(match self.handle_mouse_modifier_drag(dx, dy) {
                    Some(event) => outcome.with_layout_event(event).with_arrange_passes(1),
                    None => outcome,
                });
            }
            Event::MouseDragged { x, y } => {
                self.sync_dragged_float_space();
                let session = match &self.drag_manager.drag_state {
                    DragState::Active { session } | DragState::PendingSwap { session, .. } => {
                        Some((session.window, session.last_frame))
                    }
                    DragState::Inactive => None,
                };
                if let Some((wid, frame)) = session {
                    self.evaluate_drop_target(wid, frame, Some(CGPoint::new(x, y)));
                }
                return Ok(EventOutcome::no_change());
            }
            Event::MouseMoved(wsid) => {
                let window = self.state.windows.tracked_window_id(wsid);
                let active_space = window.and_then(|window| {
                    self.state.windows.window(window).and_then(|state| {
                        self.best_space_for_window(&state.frame_monotonic, state.info.sys_id)
                            .filter(|space| self.is_space_active(*space))
                            .or_else(|| {
                                state
                                    .info
                                    .sys_id
                                    .is_none()
                                    .then(|| self.workspace_command_space())
                                    .flatten()
                            })
                    })
                });
                let needs_layout_sync = window.is_some_and(|window| {
                    self.layout_manager.layout_engine.focused_window() != Some(window)
                });
                return window_workflow::handle_mouse_moved_over_window(
                    &self.app_manager,
                    window_workflow::MouseMovedPayload {
                        window,
                        should_sync: window
                            .is_some_and(|window| self.should_raise_on_mouse_over(window)),
                        is_main: window.is_some_and(|window| self.main_window() == Some(window)),
                        needs_layout_sync,
                        active_space,
                    },
                );
            }
            Event::MissionControlNativeEntered => {
                return topology_workflow::handle_mission_control_native_entered(
                    &mut self.mission_control_manager,
                    &mut self.drag_manager,
                );
            }
            Event::MissionControlNativeExited => {
                return topology_workflow::handle_mission_control_native_exited(
                    &mut self.mission_control_manager,
                );
            }
            Event::RaiseCompleted { window_id, sequence_id } => {
                return Ok(system_workflow::handle_raise_completed(
                    system_workflow::RaiseCompletedPayload {
                        window: window_id,
                        sequence: sequence_id,
                    },
                )?);
            }
            Event::RaiseTimeout { sequence_id } => {
                return Ok(system_workflow::handle_raise_timeout(sequence_id)?);
            }
            Event::ConfigUpdated(new_cfg) => {
                return command_workflow::handle_config_updated(
                    &mut self.config,
                    &mut self.layout_manager,
                    &self.state,
                    &mut self.drag_manager,
                    new_cfg,
                );
            }
            Event::Command(Command::Metrics(cmd)) => {
                return command_workflow::handle_command_metrics(cmd);
            }
            Event::Command(Command::Reactor(ReactorCommand::RecordTrace { path })) => {
                match path {
                    Some(path) => {
                        info!(?path, "Recording a trace");
                        let mut record = Record::new(Some(&path));
                        record.start_with_state(
                            &self.config,
                            &self.layout_manager.layout_engine,
                            Some(&self.state.windows),
                            Some(&self.transaction_manager),
                        );
                        // A recording starts mid-session. Everything the
                        // reactor already knows — the displays and spaces,
                        // the running apps and their windows — goes in first,
                        // as the events a restart would deliver, so a replay
                        // starts from the same state.
                        record.on_event(&Event::SpaceStateChanged(self.space_state.clone()));
                        let mut pids: Vec<pid_t> = self.app_manager.apps.keys().copied().collect();
                        pids.sort_unstable();
                        for pid in pids {
                            let Some(app) = self.app_manager.apps.get(&pid) else {
                                continue;
                            };
                            let visible_windows: Vec<(WindowId, WindowInfo)> = self
                                .state
                                .windows
                                .iter_windows()
                                .filter(|(wid, _)| wid.pid == pid)
                                .map(|(wid, state)| (wid, state.info.clone()))
                                .collect();
                            let window_server_info: Vec<WindowServerInfo> = visible_windows
                                .iter()
                                .filter_map(|(_, info)| info.sys_id)
                                .filter_map(|wsid| self.state.windows.get_window_server_info(wsid))
                                .collect();
                            record.on_event(&Event::ApplicationLaunched {
                                pid,
                                info: app.info.clone(),
                                handle: app.handle.clone(),
                                is_frontmost: self.main_window_tracker.is_globally_frontmost(pid),
                                main_window: self.main_window().filter(|wid| wid.pid == pid),
                                visible_windows,
                                window_server_info,
                            });
                        }
                        // Focus: which app is in front, and which window has
                        // the window server's focus. Neither is derivable
                        // from the launches.
                        if let Some(pid) = self.main_window_tracker.global_frontmost() {
                            record.on_event(&Event::ApplicationGloballyActivated(pid));
                        }
                        if let Some(wid) = self.main_window()
                            && let Some(space) = self.best_space_for_window_id(wid)
                        {
                            record.on_event(&Event::WindowServerFocusChanged(wid, space));
                        }
                        self.recording_manager.record = record;
                        // The replayed launches ask the window server about
                        // every window and space; the live session did not,
                        // at this moment. Ask now so the answers are on the
                        // record (see `sys::trace`).
                        let wsids: Vec<WindowServerId> = self
                            .state
                            .windows
                            .iter_windows()
                            .filter_map(|(_, state)| state.info.sys_id)
                            .collect();
                        for wsid in wsids {
                            let _ = window_server::get_window(wsid);
                            let _ = window_server::window_spaces(wsid);
                            let _ = window_server::window_ordered_in(wsid);
                            let _ = window_server::window_parent(wsid);
                            let _ = window_server::window_is_sticky(wsid);
                            let _ = window_server::window_level(wsid.0);
                            let _ = window_server::live_window_frame(wsid);
                            let _ = window_server::app_window_suitability(wsid);
                        }
                        let spaces: Vec<SpaceId> = self.iter_active_spaces().collect();
                        for space in spaces {
                            let _ = window_server::space_window_list_for_connection(
                                &[space.get()],
                                0,
                                false,
                            );
                            let _ = window_server::key_focused_window(space);
                            let _ = window_server::space_is_user(space.get());
                        }
                        let _ = window_server::active_space();
                        let _ = window_server::current_cursor_location();
                        let _ = crate::sys::event::get_mouse_state();
                    }
                    None => {
                        info!("Stopped recording the trace");
                        self.recording_manager.record = Record::new(None);
                        crate::sys::trace::stop_recording();
                    }
                }
                return Ok(EventOutcome::no_change());
            }
            Event::Command(Command::Reactor(ReactorCommand::DumpTrace { path })) => {
                match crate::sys::trace::dump_ring(&path) {
                    Ok(lines) => info!(?path, lines, "Dumped the flight recorder"),
                    Err(err) => warn!(?path, %err, "Failed to dump the flight recorder"),
                }
                return Ok(EventOutcome::no_change());
            }
            Event::Command(Command::Reactor(ReactorCommand::Debug)) => {
                return command_workflow::handle_command_reactor_debug(
                    &self.layout_manager,
                    &self.space_state,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)) => {
                let active_space = self.active_display_space();
                return command_workflow::handle_command_reactor_save_and_exit(
                    &self.state,
                    &mut self.layout_manager,
                    active_space,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::SaveLayout { path })) => {
                let active_space = self.active_display_space();
                return command_workflow::handle_command_reactor_save_layout(
                    &self.state,
                    &mut self.layout_manager,
                    path,
                    active_space,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::AutosaveLayout { path })) => {
                let active_space = self.active_display_space();
                return command_workflow::handle_command_reactor_autosave_layout(
                    &self.state,
                    &mut self.layout_manager,
                    path,
                    active_space,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::RestoreLayout {
                path,
                scope,
                source,
            })) => {
                let Some(active_space) = self.active_display_space() else {
                    return Ok(EventOutcome::no_change().with_stdout_line(
                        "Could not restore saved layout: no active macOS space is available".into(),
                    ));
                };
                let request = layout::RestoreRequest {
                    scope,
                    active_space,
                    source,
                    from_space: None,
                };
                let outcome = EventOutcome::window_membership_changed(false, true);
                let report = self.layout_manager.layout_engine.restore_layout(
                    path,
                    request,
                    &mut self.state.windows,
                    &self.config.virtual_workspaces,
                    &self.config.settings.layout,
                );
                return Ok(match report {
                    Ok(report) => outcome.with_stdout_line(report.summary()),
                    Err(error) => {
                        tracing::error!(?scope, %error, "Could not restore saved layout");
                        outcome.with_stdout_line(format!("Could not restore saved layout: {error}"))
                    }
                });
            }
            Event::Command(Command::Reactor(ReactorCommand::Serialize)) => {
                let serialized = self.serialize_state();
                return command_workflow::handle_command_reactor_serialize(serialized);
            }
            Event::Command(Command::Reactor(ReactorCommand::SwitchSpace(direction))) => {
                return command_workflow::handle_switch_native_space(direction);
            }
            Event::Command(Command::Reactor(ReactorCommand::SwitchToSpace(index))) => {
                unsafe {
                    crate::sys::space_switch::switch_to_space_index(
                        index,
                        self.config.settings.space_switch_method,
                    )
                };
                return Ok(EventOutcome::default());
            }
            Event::Command(Command::Reactor(ReactorCommand::MoveWindowToSpace {
                index,
                follow,
            })) => {
                self.move_focused_window_to_space(index, follow);
                return Ok(EventOutcome::default());
            }
            Event::Command(Command::Reactor(ReactorCommand::CreateSpace)) => {
                self.create_space_after_active();
                return Ok(EventOutcome::default());
            }
            Event::Command(Command::Reactor(ReactorCommand::RestoreDepartureLayout)) => {
                return Ok(self.restore_departure_layout());
            }
            Event::Command(Command::Reactor(ReactorCommand::DestroySpace)) => {
                let active = crate::sys::space_switch::active_space();
                if !crate::sys::scripting_addition::destroy_space(active.get()) {
                    self.fail_command(SA_REQUIRED_DESTROY);
                }
                return Ok(EventOutcome::default());
            }
            Event::Command(Command::Reactor(ReactorCommand::ToggleSpaceActivated)) => {
                let space = self.active_display_space();
                let display_uuid = space.and_then(|space| {
                    self.space_state
                        .screen_by_space(space)
                        .and_then(|screen| screen.display_uuid_owned())
                });
                let config = self.activation_cfg();
                return command_workflow::handle_command_reactor_toggle_space_activated(
                    &mut self.space_activation_policy,
                    command_workflow::ToggleSpacePayload { config, space, display_uuid },
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::ShowMissionControlAll)) => {
                return command_workflow::handle_mission_control_command(
                    crate::actor::wm_controller::WmCmd::ShowMissionControlAll,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::ShowMissionControlCurrent)) => {
                return command_workflow::handle_mission_control_command(
                    crate::actor::wm_controller::WmCmd::ShowMissionControlCurrent,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::DismissMissionControl)) => {
                return command_workflow::handle_mission_control_command(
                    crate::actor::wm_controller::WmCmd::DismissMissionControl,
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::CloseWindow { window_server_id })) => {
                return command_workflow::handle_close_window(
                    window_server_id.map(WindowServerId::new),
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::FocusWindow {
                window_id,
                window_server_id,
            })) => {
                let window_id = WindowId::new(window_id.pid, window_id.idx);
                let window_server_id = window_server_id.map(WindowServerId::new);
                let resolved_space = self.best_space_for_window_id(window_id).or_else(|| {
                    self.state.windows.window(window_id).and_then(|window| {
                        self.best_space_for_window(&window.frame_monotonic, window.info.sys_id)
                    })
                });
                return command_workflow::handle_command_reactor_focus_window(
                    &self.state,
                    &self.app_manager,
                    command_workflow::FocusWindowPayload {
                        window_id,
                        window_server_id,
                        resolved_space,
                        space_is_active: resolved_space
                            .is_some_and(|space| self.is_space_active(space)),
                    },
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::MoveMouseToDisplay(selector))) => {
                let screen = self.screen_for_selector(&selector, None).cloned();
                let focus_window = screen.as_ref().and_then(|screen| {
                    let space = screen.space?;
                    self.last_focused_window_in_space(space).or_else(|| {
                        self.layout_manager
                            .layout_engine
                            .windows_in_active_workspace(&self.state.windows, space)
                            .into_iter()
                            .next()
                    })
                });
                let target_is_active = screen
                    .as_ref()
                    .and_then(|screen| screen.space)
                    .is_none_or(|space| self.is_space_active(space));
                return command_workflow::handle_move_mouse_to_display(
                    &self.app_manager,
                    command_workflow::DisplayFocusPayload {
                        screen,
                        target_is_active,
                        focus_window,
                        focus_window_center: None,
                    },
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::FocusDisplay(selector))) => {
                let screen = self.screen_for_selector(&selector, None).cloned();
                let focus_window = screen.as_ref().and_then(|screen| {
                    let space = screen.space?;
                    self.last_focused_window_in_space(space).or_else(|| {
                        self.layout_manager
                            .layout_engine
                            .windows_in_active_workspace(&self.state.windows, space)
                            .into_iter()
                            .next()
                    })
                });
                let target_is_active = screen
                    .as_ref()
                    .and_then(|screen| screen.space)
                    .is_none_or(|space| self.is_space_active(space));
                let focus_window_center = focus_window
                    .and_then(|wid| self.state.windows.window(wid))
                    .map(|window| window.frame_monotonic.mid());
                return command_workflow::handle_focus_display(
                    &self.app_manager,
                    command_workflow::DisplayFocusPayload {
                        screen,
                        target_is_active,
                        focus_window,
                        focus_window_center,
                    },
                );
            }
            Event::Command(Command::Layout(command)) => {
                if self.focused_window_is_unmanaged(&command) {
                    return Ok(EventOutcome::no_change());
                }
                self.sync_layout_focus_for_command(&command);
                // Moving a window doesn't change focus, so mouse_follows_focus
                // never fires and the cursor gets left behind on the window's
                // old position. The command handler warps to the moved window's
                // post-arrange frame for MoveNode; it goes through the same
                // per-app check as every other warp, so an app on
                // mouse_follows_focus_blacklist is not dragged onto by this
                // path either.
                let post_arrange_mouse_warp = self
                    .main_window()
                    .filter(|wid| self.mouse_follows_focus_permitted_for_app(*wid));
                let command_space = self.command_context_space();
                let (visible_spaces, visible_space_centers) = self.visible_spaces_for_layout(false);
                return command_workflow::handle_command_layout(
                    &mut self.state,
                    &mut self.layout_manager,
                    &mut self.workspace_switch_manager,
                    command_workflow::LayoutCommandPayload {
                        command,
                        command_space,
                        visible_spaces,
                        visible_space_centers,
                        post_arrange_mouse_warp,
                    },
                );
            }
            Event::Command(Command::Reactor(ReactorCommand::MoveWindowToDisplay {
                selector,
                window_id,
            })) => {
                if self.is_in_drag() {
                    warn!("Ignoring move-window-to-display while a drag is active");
                    return Ok(EventOutcome::no_change());
                }
                let command_space = self.workspace_command_space();
                let resolved_window = {
                    let workspaces = self.layout_manager.layout_engine.virtual_workspace_manager();
                    match window_id {
                        Some(index) => command_space
                            .and_then(|space| {
                                workspaces.find_window_by_idx(&self.state.windows, space, index)
                            })
                            .or_else(|| {
                                self.iter_active_spaces().find_map(|space| {
                                    workspaces.find_window_by_idx(&self.state.windows, space, index)
                                })
                            }),
                        None => self
                            .main_window()
                            .or_else(|| self.window_id_under_cursor())
                            .or_else(|| {
                                command_space.and_then(|space| {
                                    workspaces.find_window_by_idx(&self.state.windows, space, 0)
                                })
                            }),
                    }
                };
                let Some(window) = resolved_window else {
                    warn!("Move window to display ignored because no target window was resolved");
                    return Ok(EventOutcome::no_change());
                };
                let Some(window_state) = self.state.windows.window(window) else {
                    warn!(?window, "Move window to display ignored: unknown window");
                    return Ok(EventOutcome::no_change());
                };
                let window_server_id = window_state.info.sys_id;
                let window_frame = window_state.frame_monotonic;
                let source_space = self
                    .assigned_space_for_window_id(window)
                    .or_else(|| self.best_space_for_window_id(window))
                    .or_else(|| self.best_space_for_window(&window_frame, window_server_id));
                let Some(source_space) = source_space.filter(|space| self.is_space_active(*space))
                else {
                    warn!(
                        ?window,
                        "Move window to display ignored: source space unavailable"
                    );
                    return Ok(EventOutcome::no_change());
                };
                let origin = self
                    .space_state
                    .screen_by_space(source_space)
                    .map(|screen| screen.frame.mid())
                    .or_else(|| self.current_screen_center());
                let Some(target_screen) = self.screen_for_selector(&selector, origin).cloned()
                else {
                    warn!(
                        ?selector,
                        "Move window to display ignored: target display not found"
                    );
                    return Ok(EventOutcome::no_change());
                };
                let Some(target_space) =
                    target_screen.space.filter(|space| self.is_space_active(*space))
                else {
                    warn!(
                        ?selector,
                        "Move window to display ignored: target space unavailable"
                    );
                    return Ok(EventOutcome::no_change());
                };
                if source_space == target_space {
                    return Ok(EventOutcome::no_change());
                }
                let mut target_frame = window_frame;
                let mut origin = target_screen.frame.mid();
                origin.x -= window_frame.size.width / 2.0;
                origin.y -= window_frame.size.height / 2.0;
                let min = target_screen.frame.min();
                let max = target_screen.frame.max();
                origin.x = origin.x.max(min.x).min(max.x - window_frame.size.width);
                origin.y = origin.y.max(min.y).min(max.y - window_frame.size.height);
                target_frame.origin = origin;
                return command_workflow::handle_command_reactor_move_window_to_display(
                    &mut self.state,
                    &mut self.layout_manager,
                    command_workflow::MoveWindowToDisplayPayload {
                        window,
                        window_server_id,
                        source_space,
                        target_space,
                        target_screen: target_screen.frame,
                        target_frame,
                    },
                );
            }
            _ => (),
        }

        Ok(EventOutcome::focus_changed(
            raised_window,
            should_update_notifications,
        ))
    }

    /// Applies workflow follow-up requests in one stable order.
    ///
    /// Explicit transition frames are written before layout calculation so the
    /// resulting layout remains authoritative. Focus selection follows layout
    /// writes, then UI/platform presentation state is refreshed. Broadcast and
    /// discovery requests made directly by a workflow are consequently observed
    /// only after its model mutation is complete.
    fn apply_event_outcome(&mut self, outcome: EventOutcome) {
        if !outcome.window_server_updates.is_empty() {
            self.update_partial_window_server_info(outcome.window_server_updates);
        }
        if outcome.recompute_active_spaces {
            self.recompute_and_set_active_spaces_from_current_screens();
        }
        if outcome.repair_spaces_after_mission_control {
            self.repair_spaces_after_mission_control();
        }
        if outcome.refresh_after_mission_control {
            self.refresh_windows_after_mission_control();
        }
        if outcome.refresh_window_inventories {
            self.request_window_inventories();
        }
        // Discovery responses reconcile model state before layout. Requests
        // which schedule new discovery are deferred to the final phase below.
        for discovery in outcome.discoveries {
            self.on_windows_discovered_with_app_info(
                discovery.pid,
                discovery.new,
                discovery.known_visible,
                discovery.app_info,
            );
        }
        for window in outcome.reapply_app_rules {
            self.maybe_reapply_app_rules_for_window(window);
        }
        for window in outcome.finalize_created_windows {
            let active_space = self.state.windows.window(window).and_then(|state| {
                self.best_space_for_window(&state.frame_monotonic, state.info.sys_id)
                    .filter(|space| self.is_space_active(*space))
                    .or_else(|| {
                        state
                            .info
                            .sys_id
                            .is_none()
                            .then(|| self.workspace_command_space())
                            .flatten()
                    })
            });
            if let Some(space) = active_space {
                if let Some(app_info) =
                    self.app_manager.apps.get(&window.pid).map(|app| app.info.clone())
                {
                    self.process_windows_for_app_rules(vec![window], app_info, false);
                }
                if self.state.windows.window(window).is_some_and(WindowState::is_admitted) {
                    self.send_layout_event(LayoutEvent::WindowAdded(space, window));
                }
            }
        }

        for (window_server_id, space) in outcome.confirmed_window_spaces {
            self.clear_pending_target_if_confirmed_space(window_server_id, space);
        }
        for (window_server_id, space, window) in outcome.fullscreen_restorations {
            let mut nested = EventOutcome::default();
            if self
                .restore_fullscreen_window_to_user_space(
                    window_server_id,
                    space,
                    window,
                    &mut nested,
                )
                .is_none()
            {
                self.reassign_window_to_authoritative_space(window, space);
            }
            self.apply_event_outcome(nested);
        }
        for reassignment in outcome.topology_reassignments {
            if reassignment.preserve_workspace_ordinal {
                self.reassign_window_to_authoritative_space_preserving_workspace_ordinal(
                    reassignment.window,
                    reassignment.space,
                );
            } else {
                self.reassign_window_to_authoritative_space(
                    reassignment.window,
                    reassignment.space,
                );
            }
        }

        // Some transitions need to place a window on its destination display
        // before arranging that display. Keep these writes ahead of both layout
        // responses and the arrange pass so tiling always supplies the final frame.
        for write in outcome.pre_layout_window_frame_writes {
            let window_server_id =
                self.state.windows.window(write.window).and_then(|window| window.info.sys_id);
            let transaction = if let Some(window_server_id) = window_server_id {
                let transaction = self.transaction_manager.generate_next_txid(window_server_id);
                self.transaction_manager.store_txid(window_server_id, transaction, write.frame);
                transaction
            } else {
                TransactionId::default()
            };
            if let Some(app) = self.app_manager.apps.get(&write.window.pid)
                && let Err(error) = app.handle.send(Request::SetWindowFrame(
                    write.window,
                    write.frame,
                    transaction,
                    write.requested,
                ))
            {
                warn!(window = ?write.window, %error, "failed to write requested window frame");
            }
        }

        for event in outcome.layout_events {
            self.send_layout_event(event);
        }
        for (response, workspace_switch_space) in outcome.layout_responses {
            self.handle_layout_response(response, workspace_switch_space);
        }
        for (window, frame) in outcome.drag_swap_evaluations {
            self.maybe_swap_on_drag(window, frame);
        }
        if outcome.dispatch_mouse_up {
            self.handle_event(Event::MouseUp);
        }

        let mut layout_changed = false;
        if outcome.arrange.requested && (!self.is_in_drag() || outcome.arrange.window_was_destroyed)
        {
            for _ in 0..outcome.arrange.passes.max(1) {
                layout_changed |= self.update_layout_or_warn(
                    outcome.arrange.is_resize,
                    matches!(
                        self.workspace_switch_manager.workspace_switch_state,
                        WorkspaceSwitchState::Active
                    ),
                    outcome.arrange.space_scope,
                );
            }
            // Publish the menu state once after all arrange passes have completed.
            self.maybe_send_menu_update();
        }
        if outcome.broadcast_layout_changed && layout_changed {
            self.broadcast_layout_changed(
                outcome.arrange.space_scope.or_else(|| self.workspace_command_space()),
            );
        }
        if outcome.broadcast_selection_changed {
            self.broadcast_selection_changed(
                outcome.arrange.space_scope.or_else(|| self.workspace_command_space()),
            );
        }

        if layout_changed
            && let Some(window) = outcome.post_arrange_mouse_warp
            && let Some(center) = self.window_center_on_known_screen(window)
        {
            self.warp_mouse(center);
        }

        for request in outcome.raise_requests {
            if let Err(error) = self.communication_manager.raise_manager_tx.try_send(request) {
                warn!(%error, "failed to send raise request");
            }
        }

        if let Some((space, window)) =
            focus_service::resolve(outcome.focused_window, |wid| self.best_space_for_window_id(wid))
        {
            self.send_layout_event(LayoutEvent::WindowFocused(space, window));
        }

        if let Some(direction) = outcome.switch_native_space {
            unsafe {
                window_server::switch_space(direction, self.config.settings.space_switch_method)
            };
        }

        for (pid, window) in outcome.make_key_windows {
            if let Err(error) = window_server::make_key_window(pid, window) {
                warn!(?error, "failed to make key window");
            }
        }
        for point in outcome.mouse_warps {
            self.warp_mouse(point);
        }

        for command in outcome.wm_commands {
            let is_dismiss = matches!(
                command,
                crate::actor::wm_controller::WmCmd::DismissMissionControl
            );
            if let Some(wm) = self.communication_manager.wm_sender.as_ref() {
                wm.send(crate::actor::wm_controller::WmEvent::Command(
                    crate::actor::wm_controller::WmCommand::Wm(command),
                ));
            } else if is_dismiss {
                self.set_mission_control_active(false);
            }
        }
        for event in outcome.wm_events {
            if let Some(wm) = self.communication_manager.wm_sender.as_ref() {
                wm.send(event);
            }
        }

        if let Some(window_server_id) = outcome.close_window {
            let target = match window_server_id {
                Some(wsid) => self.state.windows.tracked_window_id(wsid),
                None => self.main_window(),
            };
            if let Some(window) = target {
                self.request_close_window(window.pid, window_server_id);
            } else {
                warn!(?window_server_id, "Close target not found");
            }
        }

        if let Some(config) = outcome.service_config_update {
            if let Some(tx) = &self.communication_manager.stack_line_tx
                && let Err(error) = tx.try_send(stack_line::Event::ConfigUpdated(config.clone()))
            {
                warn!(%error, "failed to update stack line config");
            }
            if let Some(tx) = &self.communication_manager.drop_overlay_tx {
                tx.send(crate::actor::drop_overlay::Event::ConfigUpdated(config.clone()));
            }
            if let Some(tx) = &self.menu_manager.menu_tx
                && let Err(error) = tx.try_send(menu_bar::Event::ConfigUpdated(config.clone()))
            {
                warn!(%error, "failed to update menu bar config");
            }
            crate::sys::scripting_addition::apply_space_switch_animation(
                &config.settings.space_switch_animation,
            );
            if let Some(wm) = &self.communication_manager.wm_sender {
                wm.send(crate::actor::wm_controller::WmEvent::ConfigUpdated(config));
            }
        }
        for line in outcome.stdout_lines {
            println!("{line}");
        }
        self.workspace_switch_manager.mark_workspace_switch_inactive();
        if self.workspace_switch_manager.active_workspace_switch.is_some() && !layout_changed {
            self.workspace_switch_manager.active_workspace_switch = None;
            trace!("Workspace switch stabilized with no further frame changes");
        }

        // Execute deferred mouse warp after workspace switch completes
        if let Some(wid) = self.workspace_switch_manager.pending_workspace_mouse_warp.take() {
            if let Some(window_center) = self.window_center_on_known_screen(wid) {
                self.warp_mouse(window_center);
            }
        }

        if outcome.refresh_window_notifications {
            let mut ids: Vec<u32> = self
                .state
                .windows
                .iter_tracked_window_server_ids()
                .map(|wsid| wsid.as_u32())
                .collect();
            ids.sort_unstable();

            if ids != self.notification_manager.last_sls_notification_ids {
                crate::sys::window_notify::update_window_notifications(&ids);

                self.notification_manager.last_sls_notification_ids = ids;
            }
        }
        if outcome.refresh_focus_follows_mouse {
            self.update_focus_follows_mouse_state();
        }
        if outcome.refresh_layout_mode {
            self.update_event_tap_layout_mode();
        }
        for broadcast in outcome.window_title_broadcasts {
            self.broadcast_window_title_changed(
                broadcast.window,
                broadcast.previous_title,
                broadcast.new_title,
            );
        }
        if let Some(window) = outcome.focused_window_broadcast {
            self.broadcast_focused_window_changed(window);
        }
        // Requests which schedule fresh discovery are last so observers see
        // the fully reconciled model, layout, UI, and broadcasts.
        for (pid, request) in outcome.app_requests {
            if let Some(app) = self.app_manager.apps.get(&pid)
                && let Err(error) = app.handle.send(request)
            {
                warn!(pid, %error, "failed to send deferred application request");
            }
        }
        for pid in outcome.window_inventory_requests {
            self.request_window_inventory(pid);
        }
    }

    fn create_window_data(&self, window_id: WindowId) -> Option<RuntimeWindowData> {
        let window_state = self.state.windows.window(window_id)?;
        if !window_state.is_admitted() {
            return None;
        }
        let app = self.app_manager.apps.get(&window_id.pid)?;

        let app_name = app.info.localized_name.clone();
        let bundle_id = app.info.bundle_id.clone();

        Some(RuntimeWindowData {
            id: window_id,
            is_floating: self.layout_manager.layout_engine.is_window_floating(window_id),
            is_focused: self.main_window() == Some(window_id),
            layout_position: None,
            app_name,
            info: WindowInfo {
                title: window_state.info.title.clone(),
                frame: window_state.frame_monotonic,
                bundle_id,
                ..window_state.info.clone()
            },
        })
    }

    fn update_complete_window_server_info(&mut self, ws_info: Vec<WindowServerInfo>) {
        self.state.windows.clear_visible_windows();
        self.update_partial_window_server_info(ws_info);
    }

    fn update_partial_window_server_info(&mut self, ws_info: Vec<WindowServerInfo>) {
        // Mark visible windows and remove any corresponding observed WSID markers
        // for ids we now have server info for.
        self.state.windows.set_visible_windows(ws_info.iter().map(|info| info.id));
        for info in ws_info.iter() {
            // If we've been observing this server id from SLS callbacks, clear it.
            self.state.windows.clear_window_server_observed(info.id);
            self.state.windows.track_window_server_info(*info);

            if let Some(wid) = self.state.windows.tracked_window_id(info.id) {
                if let Some(window) = self.state.windows.window_mut(wid) {
                    if info.layer == 0 {
                        window.frame_monotonic = info.frame;
                    }
                } else {
                    continue;
                }
                if utils::refresh_heuristic(&mut self.state, wid)
                    .is_some_and(|transition| transition.was_admitted && !transition.is_admitted)
                {
                    self.send_layout_event(LayoutEvent::WindowRemoved(wid));
                }
            }
        }
    }

    fn request_window_inventories(&mut self) {
        // AX discovery remains the source of truth for enumerating app windows.
        // Native-space membership/visibility is supplied separately by the spaces
        // actor; do not replace this with the global CG on-screen window list.
        // A drag in flight defers the sweep too: the window server flips the
        // dragged window between spaces as it crosses a display seam, and
        // answering every one of those SpaceStateChanged snapshots with a
        // full every-app AX census (twice over) was ~11 discovery events a
        // second for the whole drag — with get_window/live_frame queries per
        // window riding on each. Nothing in the sweep is urgent mid-drag:
        // the dragged window is frozen in its tree, and the drop flushes the
        // one refresh that is owed.
        if self.refreshes_blocked() || self.window_in_drag().is_some() {
            self.defer_window_inventory_refresh();
            return;
        }

        let pids: Vec<_> = self.app_manager.apps.keys().copied().collect();
        for pid in pids {
            self.request_window_inventory(pid);
        }
    }

    fn restore_windows_after_fullscreen_exit(&mut self, spaces: &[Option<SpaceId>]) {
        let refresh_spaces: Vec<SpaceId> = spaces
            .iter()
            .copied()
            .flatten()
            .filter(|space| !self.is_fullscreen_space(*space))
            .collect();

        for space in refresh_spaces {
            let records: Vec<_> = self
                .state
                .windows
                .iter_native_fullscreen_records()
                .filter(|record| {
                    record.last_known_user_space == Some(space)
                        || record.workspace.is_some_and(|workspace| workspace.space == space)
                })
                .collect();

            if records.is_empty() {
                continue;
            }

            for record in records {
                let _ = self
                    .state
                    .windows
                    .restore_window_from_native_fullscreen(record.current_window_id);

                self.request_window_inventory(record.current_window_id.pid);

                let live_window_id = record
                    .window_server_id
                    .and_then(|wsid| self.state.windows.tracked_window_id(wsid))
                    .or_else(|| {
                        self.state
                            .windows
                            .contains_window(record.current_window_id)
                            .then_some(record.current_window_id)
                    });

                let target_space = record
                    .workspace
                    .map(|workspace| workspace.space)
                    .or(record.last_known_user_space);

                if let (Some(window_id), Some(target_space)) = (live_window_id, target_space)
                    && let Some(source_space) =
                        self.best_space_for_window_id(window_id).or(Some(target_space))
                    && source_space != target_space
                {
                    let target_screen_size = self
                        .space_state
                        .screen_by_space(target_space)
                        .map(|screen| screen.frame.size)
                        .unwrap_or_else(|| CGSize::new(0.0, 0.0));

                    let response = self.layout_manager.layout_engine.move_window_to_space(
                        &mut self.state.windows,
                        source_space,
                        target_space,
                        target_screen_size,
                        window_id,
                    );
                    self.handle_layout_response(response, None);
                }
            }

            self.refocus_manager.refocus_state = RefocusState::Pending(space);
            self.update_layout_or_warn(false, false, None);
            self.update_focus_follows_mouse_state();
        }
    }

    fn is_fullscreen_space(&self, space: SpaceId) -> bool {
        self.space_state.fullscreen_spaces.contains(&space)
    }

    fn finalize_space_change(
        &mut self,
        spaces: &[Option<SpaceId>],
        active_windows: Vec<(WindowServerId, Option<SpaceId>)>,
        preserve_missing_assignments: bool,
    ) {
        self.refocus_manager.stale_cleanup_state = if spaces.iter().all(|space| space.is_none()) {
            StaleCleanupState::Suppressed
        } else {
            StaleCleanupState::Enabled
        };
        self.expose_all_spaces();
        if let Some(main_window) = self.main_window() {
            if let Some(space) = self.main_window_space() {
                self.send_layout_event(LayoutEvent::WindowFocused(space, main_window));
            }
        }
        self.reconcile_authoritative_active_window_snapshot(
            active_windows,
            preserve_missing_assignments,
        );
        self.request_window_inventories();

        if let Some(space) = self.workspace_command_space() {
            self.focus_desktop_if_active_workspace_empty(space);
        }

        if let Some(space) = self
            .workspace_command_space()
            .or_else(|| spaces.iter().copied().flatten().find(|space| self.is_space_active(*space)))
        {
            if let Some((workspace_id, workspace_name)) =
                self.layout_manager.layout_engine.ensure_active_workspace_info(space)
            {
                let display_uuid = self.display_uuid_for_space(space);
                let broadcast_event = BroadcastEvent::WorkspaceChanged {
                    workspace_id: protocol_workspace_id(workspace_id),
                    workspace_name,
                    space_id: space.get(),
                    display_uuid,
                };
                _ = self.communication_manager.event_broadcaster.send(broadcast_event);
            }
        }
    }

    fn broadcast_window_title_changed(
        &mut self,
        window_id: WindowId,
        previous_title: String,
        new_title: String,
    ) {
        if previous_title != new_title
            && let Some(space) = self.best_space_for_window_id(window_id)
            && self.is_space_active(space)
            && let Some(workspace_id) = self.layout_manager.layout_engine.active_workspace(space)
        {
            let workspace_index = self.layout_manager.layout_engine.active_workspace_idx(space);

            let workspace_name = self
                .layout_manager
                .layout_engine
                .workspace_name(space, workspace_id)
                .unwrap_or_else(|| format!("Workspace {:?}", workspace_id));

            let display_uuid = self.display_uuid_for_space(space);

            let event = BroadcastEvent::WindowTitleChanged {
                window_id: protocol_window_id(window_id),
                workspace_id: protocol_workspace_id(workspace_id),
                workspace_index,
                workspace_name,
                previous_title,
                new_title,
                space_id: space.get(),
                display_uuid,
            };
            let _ = self.communication_manager.event_broadcaster.send(event);
        }
    }

    fn broadcast_focused_window_changed(&self, window_id: WindowId) {
        if let Some(space) = self.best_space_for_window_id(window_id)
            && self.is_space_active(space)
            && let Some(workspace_id) = self.layout_manager.layout_engine.active_workspace(space)
        {
            let workspace_index = self.layout_manager.layout_engine.active_workspace_idx(space);
            let workspace_name = self
                .layout_manager
                .layout_engine
                .workspace_name(space, workspace_id)
                .unwrap_or_else(|| format!("Workspace {:?}", workspace_id));
            let display_uuid = self.display_uuid_for_space(space);

            let event = BroadcastEvent::FocusedWindowChanged {
                window_id: protocol_window_id(window_id),
                workspace_id: protocol_workspace_id(workspace_id),
                workspace_index,
                workspace_name,
                space_id: space.get(),
                display_uuid,
            };
            let _ = self.communication_manager.event_broadcaster.send(event);
        }
    }

    fn broadcast_layout_changed(&self, space: Option<SpaceId>) {
        self.broadcast_layout_state_changed(space, rift_protocol::EventKind::LayoutChanged);
    }

    fn broadcast_selection_changed(&self, space: Option<SpaceId>) {
        self.broadcast_layout_state_changed(space, rift_protocol::EventKind::SelectionChanged);
    }

    fn broadcast_layout_state_changed(
        &self,
        space: Option<SpaceId>,
        kind: rift_protocol::EventKind,
    ) {
        if let Some(space) = space
            && self.is_space_active(space)
            && let Some(workspace_id) = self.layout_manager.layout_engine.active_workspace(space)
            && let Some(layout) = self.query_layout_state(Some(space.get()), None)
        {
            let workspace_index = self.layout_manager.layout_engine.active_workspace_idx(space);
            let workspace_name = self
                .layout_manager
                .layout_engine
                .workspace_name(space, workspace_id)
                .unwrap_or_else(|| format!("Workspace {:?}", workspace_id));
            let workspace_id = protocol_workspace_id(workspace_id);
            let space_id = space.get();
            let display_uuid = self.display_uuid_for_space(space);
            let event = match kind {
                rift_protocol::EventKind::LayoutChanged => BroadcastEvent::LayoutChanged {
                    workspace_id,
                    workspace_index,
                    workspace_name,
                    space_id,
                    display_uuid,
                    layout,
                },
                rift_protocol::EventKind::SelectionChanged => BroadcastEvent::SelectionChanged {
                    workspace_id,
                    workspace_index,
                    workspace_name,
                    space_id,
                    display_uuid,
                    layout,
                },
                _ => return,
            };
            let _ = self.communication_manager.event_broadcaster.send(event);
        }
    }

    fn maybe_reapply_app_rules_for_window(&mut self, window_id: WindowId) {
        if !self.config.virtual_workspaces.reapply_app_rules_on_title_change {
            return;
        }

        let Some(space) = self.best_space_for_window_id(window_id) else {
            return;
        };
        if !self.is_space_active(space) {
            return;
        }

        let is_rule_candidate = match self.state.windows.window(window_id) {
            Some(window_state) => window_state.can_reconcile_admission(),
            None => return,
        };

        if !is_rule_candidate {
            return;
        }

        let app_info = match self.app_manager.apps.get(&window_id.pid) {
            Some(app_state) => app_state.info.clone(),
            None => return,
        };

        self.process_windows_for_app_rules(vec![window_id], app_info, true);
    }

    fn handle_authoritative_space_snapshot(
        &mut self,
        space_state: ForwardedSpaceState,
    ) -> anyhow::Result<EventOutcome> {
        let mut outcome = EventOutcome::window_membership_changed(false, true);
        let analysis = topology_workflow::analyze_space_snapshot(
            &self.space_state,
            &self.active_spaces,
            &self.space_activation_policy,
            self.activation_cfg(),
            &space_state,
        );
        let pending_space_state = space_state.clone();
        let spaces_by_display_before = self.spaces_by_display();
        let ForwardedSpaceState {
            screens,
            fullscreen_spaces,
            has_seen_display_set,
            active_spaces,
            menu_bar_space,
            command_space,
            display_space_ids,
            last_user_space_by_display,
            space_remaps,
            display_set_changed,
            should_force_refresh_layout,
            releases_lifecycle_refresh_quarantine,
            resized_spaces,
            topology_window_delta,
            active_window_spaces,
            ..
        } = space_state;
        self.space_state.active_window_spaces = active_window_spaces;
        let activation_config = self.activation_cfg();
        let topology_workflow::SpaceSnapshotAnalysis {
            spaces,
            authoritative_spaces,
            command_space_only_update,
            invalidates_pending_targets,
        } = analysis;

        let current_display_spaces = screens
            .iter()
            .filter_map(|screen| screen.space.map(|space| (space, screen.display_uuid.clone())))
            .collect::<Vec<_>>();
        self.layout_manager.layout_engine.reconcile_startup_spaces(
            &mut self.state.windows,
            &current_display_spaces,
            screens.len(),
        );

        self.space_state.has_seen_display_set = has_seen_display_set;
        self.space_state.fullscreen_spaces = fullscreen_spaces;
        self.space_state.active_spaces = active_spaces;
        if command_space_only_update {
            self.space_state.menu_bar_space = menu_bar_space;
            self.space_state.command_space = command_space;
            return Ok(outcome);
        }
        if display_set_changed {
            let active_displays: Vec<String> =
                screens.iter().map(|screen| screen.display_uuid.clone()).collect();
            outcome.absorb(self.archive_departed_displays(
                &active_displays,
                &screens,
                &display_space_ids,
            ));
            self.layout_manager.layout_engine.prune_display_state(&active_displays);
        }
        self.note_display_set(&screens, &display_space_ids);
        self.space_state.menu_bar_space = menu_bar_space;
        self.space_state.command_space = command_space;
        self.space_state.display_space_ids = display_space_ids;
        self.space_state.last_user_space_by_display = last_user_space_by_display;

        if screens.is_empty() {
            self.refocus_manager.stale_cleanup_state = StaleCleanupState::Suppressed;
            if !self.space_state.screens.is_empty() {
                self.space_state.screens.clear();
                self.expose_all_spaces();
            }
            self.recompute_and_set_active_spaces(&[]);
            self.update_complete_window_server_info(Vec::new());
            self.try_apply_pending_space_change();
            return Ok(outcome);
        }

        self.refocus_manager.stale_cleanup_state = StaleCleanupState::Enabled;
        self.space_state.screens = screens;
        if invalidates_pending_targets {
            self.clear_pending_hidden_window_targets();
        }
        if self.is_mission_control_active() {
            self.pending_space_change_manager.pending_space_change = Some(pending_space_state);
            return Ok(outcome);
        }
        for (previous_space, space) in space_remaps {
            self.layout_manager.layout_engine.remap_space(
                &mut self.state.windows,
                previous_space,
                space,
            );
        }
        for screen in &self.space_state.screens {
            let (Some(space), Some(display_uuid)) = (screen.space, screen.display_uuid_opt())
            else {
                continue;
            };
            self.layout_manager
                .layout_engine
                .update_space_display(space, Some(display_uuid.to_string()));
        }
        outcome.absorb(self.begin_display_homing());
        let current_screens = self.screens_for_current_spaces();
        self.space_activation_policy
            .on_spaces_updated(activation_config, &current_screens);
        self.recompute_and_set_active_spaces(&authoritative_spaces);
        self.restore_windows_after_fullscreen_exit(&spaces);

        for (space, size) in resized_spaces {
            if !self.is_space_active(space) {
                continue;
            }
            self.layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .list_workspaces(space);
            outcome = outcome.with_layout_event(LayoutEvent::SpaceExposed(space, size));
        }
        // A shown space with no layout is exposed too, whatever its size did.
        // The resize list only covers spaces whose geometry changed; a space
        // that comes back the same size after its workspaces were moved to
        // another id (a display's other desktop, after a remap) would
        // otherwise never get a tree again.
        let unexposed: Vec<(SpaceId, CGSize)> = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| Some((screen.space?, screen.frame.size)))
            .filter(|(space, _)| {
                self.is_space_active(*space)
                    && !self.layout_manager.layout_engine.has_active_layout(*space)
            })
            .collect();
        for (space, size) in unexposed {
            debug!(space = space.get(), "Shown space has no layout; exposing it");
            self.layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .list_workspaces(space);
            outcome = outcome.with_layout_event(LayoutEvent::SpaceExposed(space, size));
        }
        if let Some(delta) = topology_window_delta {
            outcome.absorb(self.apply_topology_window_delta(delta));
        }
        let active_windows = self.authoritative_active_space_windows();
        self.finalize_space_change(&spaces, active_windows, releases_lifecycle_refresh_quarantine);
        self.settle_displaced_windows();
        if !releases_lifecycle_refresh_quarantine {
            self.follow_space_switch_across_displays(&spaces_by_display_before, &mut outcome);
        }
        self.try_apply_pending_space_change();
        if should_force_refresh_layout {
            outcome = outcome.with_window_inventory_refresh().with_arrange_passes(1);
        }
        Ok(outcome)
    }

    /// The space each display currently shows, keyed by display uuid.
    fn spaces_by_display(&self) -> HashMap<String, SpaceId> {
        self.space_state
            .screens
            .iter()
            .filter_map(|screen| Some((screen.display_uuid_owned()?, screen.space?)))
            .collect()
    }

    /// A space switch that lands on another display moves nothing the
    /// pointer could follow. macOS activates a window on the new space only
    /// when the switch is on the active display, and that activation is what
    /// `mouse_follows_focus` follows. `switch-to-space` aimed at a space of
    /// the other display, or `move-window-to-space --follow`, switches a
    /// display the pointer is not on, so nothing was activated and the
    /// pointer stayed behind. Finish the switch the way one on the active
    /// display ends: focus the window last used on the new space, which pulls
    /// the pointer to it like any focus does, or, when the space has nothing
    /// rift knows of, focus its desktop and put the pointer in the middle of
    /// the display.
    ///
    /// Only for a switch, as opposed to a display coming or going or a wake
    /// replaying the topology: exactly one display changed its space and the
    /// rest kept theirs. And only when neither the pointer nor the key window
    /// is on that display — otherwise the pointer is there already, or
    /// macOS's own activation is on its way and will be followed as usual.
    fn follow_space_switch_across_displays(
        &mut self,
        before: &HashMap<String, SpaceId>,
        outcome: &mut EventOutcome,
    ) {
        if !self.config.settings.mouse_follows_focus
            || self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input
            || self.is_mission_control_active()
            || self.is_in_drag()
            || self.modifier_drag.is_some()
        {
            return;
        }
        let mut switched = self.space_state.screens.iter().filter_map(|screen| {
            let space = screen.space?;
            let previous = *before.get(screen.display_uuid_opt()?)?;
            (previous != space).then_some((previous, space, screen.frame))
        });
        let (Some((previous, space, frame)), None) = (switched.next(), switched.next()) else {
            return;
        };
        if window_server::current_cursor_location().is_ok_and(|point| frame.contains(point)) {
            return;
        }
        let key_window_space =
            self.main_window().and_then(|wid| self.best_space_for_window_id(wid));
        if key_window_space.is_some_and(|key| key == space || key == previous) {
            return;
        }

        if let Some(wid) = self.visible_focus_candidate_in_active_workspace(space, None) {
            debug!(
                ?wid,
                space = space.get(),
                "Following a space switch onto another display"
            );
            self.handle_layout_response(
                layout::EventResponse {
                    focus_window: Some(wid),
                    ..Default::default()
                },
                None,
            );
            return;
        }
        debug!(
            space = space.get(),
            "Following a space switch onto another display's empty space"
        );
        self.focus_desktop_if_active_workspace_empty(space);
        *outcome = std::mem::take(outcome).with_mouse_warp(frame.mid());
    }

    fn try_apply_pending_space_change(&mut self) {
        if let Some(pending) = self.pending_space_change_manager.pending_space_change.take() {
            if pending.screens.len() == self.space_state.screens.len() {
                // During native Mission Control we must preserve the full forwarded snapshot,
                // not just the raw spaces vector, otherwise command-space and per-display space
                // metadata can remain stale after exit.
                if let Ok(outcome) = self.handle_authoritative_space_snapshot(pending) {
                    self.apply_event_outcome(outcome);
                }
            } else {
                self.pending_space_change_manager.pending_space_change = Some(pending);
            }
        }
    }

    fn repair_spaces_after_mission_control(&mut self) {
        // First, apply any SpaceChanged that arrived while MC was active.
        self.try_apply_pending_space_change();
    }

    fn on_windows_discovered_with_app_info(
        &mut self,
        pid: pid_t,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
        app_info: Option<AppInfo>,
    ) {
        let app_info =
            app_info.or_else(|| self.app_manager.apps.get(&pid).map(|app| app.info.clone()));
        let inactive_windows = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, _)| {
                (wid.pid == pid && self.is_window_on_known_inactive_space(wid)).then_some(wid)
            })
            .collect();
        // AX can replace a window's process-local identity while preserving its
        // WindowServer id. Treat the currently tracked identity as visible for
        // stale cleanup so the state survives long enough to be rekeyed below.
        let mut cleanup_visible = known_visible.clone();
        cleanup_visible.extend(new.iter().filter_map(|(_, info)| {
            info.sys_id.and_then(|wsid| self.state.windows.tracked_window_id(wsid))
        }));
        // Only a window this sweep did not see can be retired, so only those are
        // worth querying — and for those the live answer is worth the query, both
        // as the liveness signal and because the cached one may be old.
        let cleanup_visible_set: HashSet<WindowId> = cleanup_visible.iter().copied().collect();
        let server_observations = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, window)| {
                (wid.pid == pid && !cleanup_visible_set.contains(&wid))
                    .then_some(window.info.sys_id)
                    .flatten()
            })
            .map(|wsid| {
                let live = window_server::get_window(wsid);
                let still_known = live.is_some();
                let info = live.or_else(|| self.state.windows.get_window_server_info(wsid));
                (wsid, window_discovery::StaleWindowObservation {
                    info,
                    suitable: window_server::app_window_suitability(wsid),
                    ordered_in: window_server::window_ordered_in(wsid),
                    still_known,
                })
            })
            .collect();
        let stale_snapshot = window_discovery::StaleCleanupSnapshot {
            suppressed: matches!(
                self.refocus_manager.stale_cleanup_state,
                StaleCleanupState::Suppressed
            ),
            mission_control_active: self.is_mission_control_active(),
            drag_active: self.is_in_drag(),
            inactive_windows,
            server_observations,
        };
        // AX can replace a window's process-local identity while preserving its
        // WindowServer id. Treat the currently tracked identity as visible for
        // stale cleanup so the state survives long enough to be rekeyed below.
        let mut cleanup_visible = known_visible.clone();
        cleanup_visible.extend(new.iter().filter_map(|(_, info)| {
            info.sys_id.and_then(|wsid| self.state.windows.tracked_window_id(wsid))
        }));
        let stale_windows = window_discovery::identify_stale_windows(
            &self.state,
            pid,
            &cleanup_visible,
            &stale_snapshot,
        );
        let mut outcome = match window_discovery::cleanup_stale_windows(
            &mut self.state,
            &self.transaction_manager,
            &mut self.drag_manager,
            stale_windows,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                warn!(%error, pid, "window discovery cleanup failed");
                return;
            }
        };
        let observed_windows = new
            .into_iter()
            .map(|(wid, info)| {
                let current_native_space =
                    info.sys_id.and_then(|wsid| self.resolve_native_space(wsid, None));
                let active_space = self
                    .best_space_for_window(&info.frame, info.sys_id)
                    .filter(|space| self.is_space_active(*space))
                    .or_else(|| {
                        info.sys_id.is_none().then(|| self.workspace_command_space()).flatten()
                    });
                window_discovery::ObservedWindow {
                    wid,
                    info,
                    current_native_space,
                    active_space,
                }
            })
            .collect();
        let (new_windows, process_outcome) = window_discovery::process_window_list(
            &mut self.state,
            &mut self.layout_manager,
            observed_windows,
        );
        outcome.absorb(process_outcome);
        window_discovery::update_window_states(&mut self.state, new_windows);

        let candidate_windows: HashSet<WindowId> = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, _)| (wid.pid == pid).then_some(wid))
            .chain(known_visible.iter().copied().filter(|wid| wid.pid == pid))
            .collect();
        let discovery_spaces = candidate_windows
            .iter()
            .filter_map(|wid| self.discovery_space_for_window_id(*wid).map(|space| (*wid, space)))
            .collect();
        let authoritative_spaces = candidate_windows
            .iter()
            .filter_map(|wid| {
                self.authoritative_space_for_window_id(*wid).map(|space| (*wid, space))
            })
            .collect();
        let active_spaces = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| screen.space)
            .filter(|space| self.is_space_active(*space))
            .collect();
        let focused_window = self.focused_window_for_discovery(pid);
        let frozen_window = self.window_in_drag();
        self.layout_manager.layout_engine.set_frozen_window(frozen_window);
        outcome.absorb(window_discovery::emit_layout_events(
            &mut self.state,
            &mut self.layout_manager,
            window_discovery::EmitLayoutPayload {
                frozen_window,
                pid,
                known_visible: &known_visible,
                app_info: &app_info,
                discovery_spaces,
                authoritative_spaces,
                active_spaces,
                focused_window,
            },
        ));
        self.apply_event_outcome(outcome);
    }

    fn best_space_for_window(
        &self,
        frame: &CGRect,
        window_server_id: Option<WindowServerId>,
    ) -> Option<SpaceId> {
        if let Some(wsid) = window_server_id
            && self.is_known_fullscreen_window(wsid)
        {
            return None;
        }

        if let Some(wsid) = window_server_id {
            if let Some(space) = self.resolve_native_space(wsid, None) {
                return Some(space);
            }
        }

        if let Some(space) = self.hidden_assigned_space_for_frame(window_server_id, frame) {
            return Some(space);
        }

        self.best_space_for_frame(frame)
    }

    fn best_space_for_frame(&self, frame: &CGRect) -> Option<SpaceId> {
        let center = frame.mid();
        self.screen_for_point(center).and_then(|screen| screen.space).or_else(|| {
            self.space_state
                .screens
                .iter()
                .filter_map(|screen| {
                    let space = screen.space?;
                    let area = screen.frame.intersection(frame).area() as i64;
                    if area > 0 { Some((area, space)) } else { None }
                })
                .max_by_key(|(area, _)| *area)
                .map(|(_, space)| space)
        })
    }

    /// yabai warps the pointer on *every* focus change (`WINDOW_FOCUSED`),
    /// however it came about — a hotkey, cmd-tab, a click in another app, the
    /// app raising a window of its own — and whatever the window's state.
    /// Rift only did so for the focus changes it caused itself. This is the
    /// rest: whenever the main window changes hands, warp to it, unless the
    /// pointer is already in it (a click), a drag or workspace switch is in
    /// progress, Mission Control is up, or the change is loginwindow replaying
    /// activations after a wake.
    fn follow_focus_with_mouse(&mut self, window: WindowId, outcome: &mut EventOutcome) {
        // The window server can report focus on a child window — Lightroom's
        // filmstrip, a sheet — which is not what the user sees as "the
        // window". Aim at the app's admitted top-level window around it.
        let window = self.admitted_root_for(window);
        if !self.mouse_follows_focus_allowed_for(window)
            || self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input
            || self.is_mission_control_active()
            || !matches!(self.drag_manager.drag_state, DragState::Inactive)
            || self.modifier_drag.is_some()
        {
            return;
        }
        if self.workspace_switch_manager.workspace_switch_state == WorkspaceSwitchState::Active {
            self.workspace_switch_manager.pending_workspace_mouse_warp = Some(window);
            return;
        }
        if let Some(center) = self.window_center_on_known_screen(window)
            && !outcome.mouse_warps.contains(&center)
        {
            *outcome = std::mem::take(outcome).with_mouse_warp(center);
        }
    }

    /// A click on the Dock tile of the app that is already in front changes
    /// no focus, so nothing above moves the pointer — but it is the same
    /// request every other Dock click makes: take me to that window.
    ///
    /// Only the left button, and only an application's tile. The right button
    /// opens a menu, a stack or the Trash opens a fan, and both are places the
    /// pointer has to stay.
    fn follow_dock_click_with_mouse(&mut self, window: WindowId, outcome: &mut EventOutcome) {
        if !crate::sys::event::last_mouse_up_was_left() {
            return;
        }
        let Ok(cursor) = window_server::current_cursor_location() else {
            return;
        };
        // The cheap question first: the Dock is asked about a tile only for a
        // click that landed on it.
        if !window_server::point_is_on_dock(cursor) {
            return;
        }
        let Some(tile) = crate::sys::axuielement::dock_application_tile_at(cursor) else {
            return;
        };
        // Any other app's tile is about to activate that app, and the focus
        // change it makes carries the pointer over on its own.
        let front = self
            .app_manager
            .apps
            .get(&window.pid)
            .and_then(|app| app.info.localized_name.as_deref());
        if front != Some(tile.as_str()) {
            return;
        }
        self.follow_focus_with_mouse(window, outcome);
    }

    /// A command aimed at the focused window acts on the layout engine's
    /// focused window, which only ever tracks admitted windows. When the
    /// window actually in front is one rift does not manage (Premiere's
    /// `AXLayoutArea` main window, a panel rejected by the heuristic), the
    /// engine's record is stale — the last managed window — and the command
    /// would hit that instead. Such a command does nothing.
    ///
    /// Focus reported on an admitted window's child (Lightroom's filmstrip)
    /// still stands for the real window, as elsewhere.
    /// A tracked window rift turned away is judged again as it is now. If
    /// it passes, it enters the layout as a window just created would.
    /// Returns whether it was admitted here.
    fn readmit_rejected_window(&mut self, wid: WindowId) -> bool {
        if self.state.windows.window(wid).is_none_or(WindowState::is_admitted) {
            return false;
        }
        if !utils::refresh_heuristic(&mut self.state, wid)
            .is_some_and(|transition| !transition.was_admitted && transition.is_admitted)
        {
            return false;
        }
        info!(?wid, "window previously turned away now qualifies; admitting it");
        let outcome = EventOutcome::window_membership_changed(false, true)
            .with_created_window_finalization(wid);
        self.apply_event_outcome(outcome);
        true
    }

    fn focused_window_is_unmanaged(&mut self, command: &layout::LayoutCommand) -> bool {
        if !targets_focused_window(command) {
            return false;
        }
        // A command aimed at a window rift turned away is the moment to
        // look at it again. Admitted now, it takes its place in the layout
        // and the command has done what the user wanted of it.
        let front = self.main_window().map(|front| self.admitted_root_for(front));
        let candidates: Vec<WindowId> = match front {
            Some(front) => vec![front],
            None => self
                .main_window_tracker
                .global_frontmost()
                .map(|pid| {
                    self.state
                        .windows
                        .iter_windows()
                        .filter(|(wid, state)| wid.pid == pid && !state.is_admitted())
                        .map(|(wid, _)| wid)
                        .collect()
                })
                .unwrap_or_default(),
        };
        for candidate in candidates {
            if self.readmit_rejected_window(candidate) {
                return true;
            }
        }
        let engine_focus = self.layout_manager.layout_engine.focused_window();
        let unmanaged = match self.main_window() {
            Some(front) => {
                let target = self.admitted_root_for(front);
                !self.state.windows.window(target).is_some_and(WindowState::is_admitted)
            }
            // No window in front rift can name: only block when the frontmost
            // app is not the one the engine's focus belongs to, so a cold-start
            // fallback for the same app keeps working.
            None => match self.main_window_tracker.global_frontmost() {
                Some(pid) => engine_focus.is_none_or(|wid| wid.pid != pid),
                None => false,
            },
        };
        if unmanaged {
            info!(
                ?command,
                front = ?self.main_window(),
                ?engine_focus,
                "Ignoring layout command: the focused window is not managed"
            );
        }
        unmanaged
    }

    /// The layout engine keeps its own idea of the focused window, fed only
    /// by `WindowFocused` events. A Dock activation of an app whose window
    /// the window server never names (Premiere) leaves that record on the
    /// previously focused window even though the tracker has moved on, and a
    /// focused-window command then acts on the wrong window. Before such a
    /// command runs, bring the engine's focus in line with the admitted
    /// window that is actually in front.
    fn sync_layout_focus_for_command(&mut self, command: &layout::LayoutCommand) {
        if !targets_focused_window(command) {
            return;
        }
        let Some(front) = self.main_window() else {
            return;
        };
        let target = self.admitted_root_for(front);
        if !self.state.windows.window(target).is_some_and(WindowState::is_admitted) {
            return;
        }
        let engine_focus = self.layout_manager.layout_engine.focused_window();
        if engine_focus == Some(target) {
            return;
        }
        let Some(space) = self.best_space_for_window_id(target) else {
            return;
        };
        if !self.is_space_active(space) {
            return;
        }
        info!(
            ?command,
            ?target,
            ?engine_focus,
            "Focused-window command: aligning layout focus with the window in front"
        );
        self.send_layout_event(LayoutEvent::WindowFocused(space, target));
    }

    /// Commands and drops that are unambiguously about a particular window.
    /// See `display_archive::adopt_displaced_window`.
    fn note_explicit_window_intent(&mut self, event: &Event) {
        let mut windows: Vec<WindowId> = Vec::new();
        match event {
            Event::Command(Command::Layout(command)) => {
                if targets_focused_window(command)
                    && let Some(wid) = self.main_window()
                {
                    windows.push(wid);
                }
                if let layout::LayoutCommand::SwapWindows(a, b) = command {
                    windows.push(WindowId::new(a.pid, a.idx));
                    windows.push(WindowId::new(b.pid, b.idx));
                }
            }
            Event::MouseUp => {
                if let Some((dragged, _)) = self.get_pending_drag_swap() {
                    windows.push(dragged);
                }
            }
            Event::MouseModifierDragBegin { window, .. } => {
                if let Some(wid) = self.state.windows.tracked_window_id(*window) {
                    windows.push(wid);
                }
            }
            _ => {}
        }
        for wid in windows {
            self.adopt_displaced_window(wid);
        }
    }

    /// While the user drags a floating window, the window's own move/resize
    /// notifications are silenced at the source. An app that animates the
    /// drag itself (Warp's tab-bar drag) posts one per animation frame, and
    /// running its accessibility bridge mid-gesture glitched the drag — the
    /// window snapped back to where the drag began. Evaluated on every
    /// event, so however the drag ends, the next event restores them.
    fn sync_drag_notification_silence(&mut self) {
        let target = self
            .window_in_drag()
            .filter(|wid| self.layout_manager.layout_engine.is_window_floating(*wid));
        let current = self.drag_manager.notifications_silenced;
        if target == current {
            return;
        }
        if let Some(prev) = current
            && let Some(app) = self.app_manager.apps.get(&prev.pid)
        {
            _ = app.handle.send(Request::SetDragNotificationSilence(prev, false));
        }
        if let Some(wid) = target
            && let Some(app) = self.app_manager.apps.get(&wid.pid)
        {
            _ = app.handle.send(Request::SetDragNotificationSilence(wid, true));
        }
        self.drag_manager.notifications_silenced = target;
    }

    /// Keeps the event tap's picture of the floating windows' grab strips
    /// (title/tab bars) current, for the plain-drag takeover. Pushed only
    /// when it changes.
    fn sync_float_drag_strips(&mut self) {
        const STRIP_HEIGHT: f64 = 44.0;
        let strips: Vec<(u32, i32, CGRect)> = if self.config.settings.mouse.takeover_float_drags {
            self.state
                .windows
                .iter_windows()
                .filter(|(wid, _)| self.layout_manager.layout_engine.is_window_floating(*wid))
                .filter_map(|(wid, state)| {
                    let wsid = state.info.sys_id?;
                    let frame = state.frame_monotonic;
                    Some((
                        wsid.as_u32(),
                        wid.pid,
                        CGRect::new(
                            frame.origin,
                            CGSize::new(frame.size.width, STRIP_HEIGHT.min(frame.size.height)),
                        ),
                    ))
                })
                .collect()
        } else {
            Vec::new()
        };
        if strips == self.last_float_strips {
            return;
        }
        if let Some(event_tap_tx) = &self.communication_manager.event_tap_tx {
            _ = event_tap_tx.send(crate::actor::event_tap::Request::SetFloatDragStrips(
                strips.clone(),
            ));
        }
        self.last_float_strips = strips;
    }

    /// While the user drags a floating window across the display seam, keep
    /// its space membership matched to the display actually holding it.
    /// macOS relocates a window whose position has left its space's display;
    /// for an app that animates its own drag (Warp's tab bar) that is the
    /// vibrating fight over the seam — position bounced, app re-asserts,
    /// repeat. A bare space-membership move has no position component, so
    /// matching membership as the window crosses removes the trigger without
    /// touching the drag itself.
    fn sync_dragged_float_space(&mut self) {
        const EVERY: std::time::Duration = std::time::Duration::from_millis(100);
        let Some(wid) = self.window_in_drag() else {
            return;
        };
        if !self.layout_manager.layout_engine.is_window_floating(wid) {
            return;
        }
        let now = crate::sys::trace::now();
        if self
            .drag_manager
            .space_sync_at
            .is_some_and(|at| now.saturating_duration_since(at) < EVERY)
        {
            return;
        }
        self.drag_manager.space_sync_at = Some(now);
        let Some(wsid) = self.state.windows.window(wid).and_then(|window| window.info.sys_id)
        else {
            return;
        };
        let Some(live) = window_server::live_window_frame(wsid) else {
            return;
        };
        let majority_space = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| {
                let space = screen.space?;
                let i = screen.frame.intersection(&live);
                (i.size.width > 1.0 && i.size.height > 1.0)
                    .then_some((space, i.size.width * i.size.height))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(space, _)| space);
        let Some(space) = majority_space.filter(|space| self.is_space_active(*space)) else {
            return;
        };
        if window_server::window_space(wsid) == Some(space) {
            return;
        }
        if crate::sys::scripting_addition::move_window_to_space(wsid.as_u32(), space.get()) {
            debug!(
                ?wid,
                ?space,
                "syncing dragged float's space to the display under it"
            );
            self.note_window_sent_to_space(wsid);
            crate::sys::trace::act("drag_space_sync", &(wsid.as_u32(), space.get()));
        }
    }

    /// A seam-straddling drop's finish write races the system's relocation
    /// of the straddling window; the loser of the race is overwritten. Once
    /// the dust has settled, re-assert the finish — a frame fully on one
    /// display is never relocated, so the second write is final.
    fn assert_seam_finish(&mut self) {
        let Some(finish) = self.drag_manager.seam_finish else {
            return;
        };
        // A finish belongs to a float. Toggled back into the tree
        // mid-settle, the arrange owns the window's frame; a surviving
        // watch would read the tile write as the system relocating the
        // drop and re-assert the drop frame over the tiling.
        if !self.layout_manager.layout_engine.is_window_floating(finish.window) {
            self.drag_manager.seam_finish = None;
            return;
        }
        // A new drag supersedes the finish outright: re-asserting an old
        // drop's frame while the user is dragging yanked the window a
        // thousand pixels out of their hand.
        if self.window_in_drag().is_some()
            || crate::sys::event::get_mouse_state() == Some(crate::sys::event::MouseState::Down)
        {
            self.drag_manager.seam_finish = None;
            return;
        }
        let now = crate::sys::trace::now();
        if now.saturating_duration_since(finish.at) < managers::SeamFinish::SETTLE {
            return;
        }
        let Some(window) = self.state.windows.window(finish.window) else {
            self.drag_manager.seam_finish = None;
            return;
        };
        // The server, not rift's own record: the system's relocation report
        // trails rift's last write's transaction and the gate discards it,
        // so `frame_monotonic` still shows the write that lost the race.
        let observed = window
            .info
            .sys_id
            .and_then(window_server::live_window_frame)
            .unwrap_or(window.frame_monotonic);
        let target = match finish.fitted {
            None => {
                // The watch: if the drop was left where the user put it —
                // overhanging the seam, clipped — macOS allowed it, and
                // there is nothing to do. Only a relocation asks for the
                // deterministic placement.
                let moved = (observed.origin.x - finish.dropped_at.origin.x).abs()
                    + (observed.origin.y - finish.dropped_at.origin.y).abs()
                    > managers::SeamFinish::TOLERANCE;
                if !moved {
                    self.drag_manager.seam_finish = None;
                    return;
                }
                let screens: Vec<CGRect> =
                    self.space_state.screens.iter().map(|screen| screen.frame).collect();
                let Some(fitted) = crate::actor::reactor::events::drag::seam_fitted(
                    &screens,
                    finish.pointer,
                    finish.dropped_at,
                ) else {
                    self.drag_manager.seam_finish = None;
                    return;
                };
                debug!(
                    window = ?finish.window,
                    dropped_at = ?finish.dropped_at,
                    ?observed,
                    ?fitted,
                    "seam drop was relocated; placing it deterministically"
                );
                fitted
            }
            Some(fitted) => {
                // Landed means: resting on the landing display and no other
                // — not pixel equality; the system likes to adjust a
                // placement by a pixel or two, and chasing that would burn
                // the attempts for nothing.
                let display_of = |point: CGPoint| {
                    self.space_state.screens.iter().position(|screen| screen.frame.contains(point))
                };
                let foreign_overlap = self.space_state.screens.iter().any(|screen| {
                    if screen.frame.contains(fitted.mid()) {
                        return false;
                    }
                    let i = screen.frame.intersection(&observed);
                    i.size.width > 1.0 && i.size.height > 1.0
                });
                let landed =
                    !foreign_overlap && display_of(observed.mid()) == display_of(fitted.mid());
                if landed || finish.attempts >= 2 {
                    self.drag_manager.seam_finish = None;
                    return;
                }
                debug!(
                    window = ?finish.window,
                    ?fitted,
                    ?observed,
                    "re-asserting a seam-finish the system relocated over"
                );
                fitted
            }
        };
        // The relocation may have handed the window to the other display's
        // space (even an inactive one); a frame write alone cannot carry it
        // back across. Put it on the landing display's space first.
        let landing_space = self
            .screen_for_point(target.mid())
            .and_then(|screen| screen.space)
            .filter(|space| self.is_space_active(*space));
        if let (Some(wsid), Some(space)) = (window.info.sys_id, landing_space)
            && window_server::window_space(wsid) != Some(space)
            && crate::sys::scripting_addition::move_window_to_space(wsid.as_u32(), space.get())
        {
            self.note_window_sent_to_space(wsid);
        }
        let transaction = match window.info.sys_id {
            Some(wsid) => {
                let transaction = self.transaction_manager.generate_next_txid(wsid);
                self.transaction_manager.store_txid(wsid, transaction, target);
                transaction
            }
            None => TransactionId::default(),
        };
        if let Some(app) = self.app_manager.apps.get(&finish.window.pid) {
            _ = app
                .handle
                .send(Request::SetWindowFrame(finish.window, target, transaction, true));
        }
        self.drag_manager.seam_finish = Some(managers::SeamFinish {
            fitted: Some(target),
            at: now,
            attempts: finish.attempts + 1,
            ..finish
        });
    }

    /// The pin is released as soon as the window server agrees with it, or
    /// when the hold runs out — whichever comes first — so it never outlives
    /// the frame write it was covering.
    fn release_drop_pin_if_landed(&mut self) {
        let Some(pin) = self.drag_manager.drop_pin else {
            return;
        };
        let now = crate::sys::trace::now();
        if now >= pin.until {
            self.drag_manager.drop_pin = None;
            return;
        }
        // A server report that already reached the store is proof enough —
        // no live query needed to notice the landing.
        if self.state.windows.window_server_space(pin.window) == Some(pin.space) {
            self.drag_manager.drop_pin = None;
            return;
        }
        // Live probes are throttled: this runs after every event, and asking
        // on each of the ~40 pointer moves a second kept a query storm
        // running for the whole hold after every drop.
        if now < pin.next_probe {
            return;
        }
        if let Some(pin) = self.drag_manager.drop_pin.as_mut() {
            pin.next_probe = now + managers::DropPin::PROBE_EVERY;
        }
        if window_server::window_space(pin.window) == Some(pin.space) {
            self.drag_manager.drop_pin = None;
        }
    }

    #[cfg(test)]
    fn ensure_active_drag(&mut self, wid: WindowId, frame: &CGRect) {
        let needs_new_session =
            self.get_active_drag_session().is_none_or(|session| session.window != wid);
        if needs_new_session {
            let server_id = self.state.windows.window(wid).and_then(|window| window.info.sys_id);
            let origin_space = self.best_space_for_window(frame, server_id);
            self.drag_manager.drag_state = DragState::Active {
                session: DragSession {
                    window: wid,
                    last_frame: *frame,
                    origin_space,
                    settled_space: origin_space,
                    layout_dirty: false,
                },
            };
        }
        self.drag_manager.skip_layout_for_window = Some(wid);
    }

    fn best_space_for_window_state(&self, window: &WindowState) -> Option<SpaceId> {
        self.best_space_for_window(&window.frame_monotonic, window.info.sys_id)
    }

    fn hidden_assigned_space_for_frame(
        &self,
        window_server_id: Option<WindowServerId>,
        _frame: &CGRect,
    ) -> Option<SpaceId> {
        let wsid = window_server_id?;
        let wid = self.state.windows.tracked_window_id(wsid)?;
        let assigned_space = self.assigned_space_for_window_id(wid)?;
        if !self.is_space_active(assigned_space)
            || !self.window_in_non_active_workspace(assigned_space, wid)
        {
            return None;
        }

        Some(assigned_space)
    }

    fn hidden_assigned_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        let window = self.state.windows.window(wid)?;
        self.hidden_assigned_space_for_frame(window.info.sys_id, &window.frame_monotonic)
    }

    fn assigned_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        self.layout_manager
            .layout_engine
            .virtual_workspace_manager()
            .workspace_info_for_window_any(&self.state.windows, wid)
            .map(|info| info.space)
    }

    fn pending_target_space_for_window_server_id(&self, wsid: WindowServerId) -> Option<SpaceId> {
        let wid = self.state.windows.tracked_window_id(wsid)?;
        let target_frame = self.transaction_manager.get_target_frame(wsid)?;
        let assigned_space = self.assigned_space_for_window_id(wid)?;
        let target_space = self
            .hidden_assigned_space_for_frame(Some(wsid), &target_frame)
            .or_else(|| self.best_space_for_frame(&target_frame))?;
        (target_space == assigned_space).then_some(target_space)
    }

    fn reassign_window_to_authoritative_space(
        &mut self,
        wid: WindowId,
        authoritative_space: SpaceId,
    ) -> bool {
        self.reassign_window_to_authoritative_space_with_workspace_preservation(
            wid,
            authoritative_space,
            false,
        )
    }

    fn apply_topology_window_delta(&mut self, delta: TopologyWindowDelta) -> EventOutcome {
        let appeared: HashMap<WindowServerId, SpaceId> = delta.appeared.into_iter().collect();
        let disappeared: HashMap<WindowServerId, SpaceId> = delta.disappeared.into_iter().collect();
        let window_server_ids: HashSet<WindowServerId> =
            appeared.keys().chain(disappeared.keys()).copied().collect();
        let mut outcome = EventOutcome::default();

        for window_server_id in window_server_ids {
            let appeared_space = appeared.get(&window_server_id).copied();
            let disappeared_space = disappeared.get(&window_server_id).copied();
            let authoritative_space = self.resolve_native_space(window_server_id, appeared_space);
            if let Some(target_space) = authoritative_space {
                self.state.windows.set_window_server_space(window_server_id, Some(target_space));
                if appeared_space == Some(target_space) {
                    self.clear_pending_target_if_confirmed_space(window_server_id, target_space);
                }
                if self.is_space_active(target_space) {
                    self.state.windows.mark_window_visible(window_server_id);
                } else {
                    self.state.windows.mark_window_hidden(window_server_id);
                }
                if let Some(window) = self.state.windows.tracked_window_id(window_server_id) {
                    let restored = self.restore_fullscreen_window_to_user_space(
                        window_server_id,
                        target_space,
                        window,
                        &mut outcome,
                    );
                    if restored.is_none() {
                        self.reassign_window_to_authoritative_space_preserving_workspace_ordinal(
                            window,
                            target_space,
                        );
                    }
                }
            } else if let Some(previous_space) = disappeared_space {
                self.state
                    .windows
                    .set_window_server_space(window_server_id, Some(previous_space));
                self.state.windows.mark_window_hidden(window_server_id);
                if let Some(window) = self.state.windows.tracked_window_id(window_server_id)
                    && self.window_in_drag() != Some(window)
                    && self.assigned_space_for_window_id(window) == Some(previous_space)
                    && self.is_space_active(previous_space)
                {
                    outcome = outcome
                        .with_layout_event(LayoutEvent::WindowRemovedPreserveFloating(window));
                }
            }
        }
        outcome
    }

    fn restore_fullscreen_window_to_user_space(
        &mut self,
        window_server_id: WindowServerId,
        space: SpaceId,
        original_window: WindowId,
        outcome: &mut EventOutcome,
    ) -> Option<bool> {
        let restored = self
            .state
            .windows
            .restore_window_from_native_fullscreen_by_window_server_id(window_server_id)
            .or_else(|| {
                self.state.windows.restore_window_from_native_fullscreen(original_window)
            })?;
        let owner = self
            .state
            .windows
            .contains_window(restored.current_window_id)
            .then_some(restored.current_window_id)
            .or_else(|| {
                restored
                    .window_server_id
                    .and_then(|id| self.state.windows.tracked_window_id(id))
            })
            .or_else(|| self.state.windows.tracked_window_id(window_server_id))
            .or_else(|| {
                self.state.windows.contains_window(original_window).then_some(original_window)
            })?;
        if owner != original_window && self.assigned_space_for_window_id(original_window).is_some()
        {
            *outcome = std::mem::take(outcome)
                .with_layout_event(LayoutEvent::WindowRemoved(original_window));
        }
        *outcome = std::mem::take(outcome).with_window_inventory_request(owner.pid);
        Some(if self.assigned_space_for_window_id(owner) == Some(space) {
            self.is_space_active(space)
                && self.restore_window_to_layout_after_fullscreen(owner, space)
        } else {
            // The window's workspace assignment still says the fullscreen
            // space it is leaving, so this is the ordinary exit, not a window
            // that went somewhere else — dropping the slot here is what used
            // to lose the position. Reassigning puts it back in the tree, and
            // the addition reinstates the slot.
            self.reassign_window_to_authoritative_space(owner, space)
        })
    }

    pub(crate) fn reassign_window_to_authoritative_space_preserving_workspace_ordinal(
        &mut self,
        wid: WindowId,
        authoritative_space: SpaceId,
    ) -> bool {
        self.reassign_window_to_authoritative_space_with_workspace_preservation(
            wid,
            authoritative_space,
            true,
        )
    }

    fn reassign_window_to_authoritative_space_with_workspace_preservation(
        &mut self,
        wid: WindowId,
        authoritative_space: SpaceId,
        preserve_workspace_ordinal: bool,
    ) -> bool {
        self.hold_if_dragged_across_spaces(wid, authoritative_space);
        if self.window_in_drag() == Some(wid) {
            debug!(
                ?wid,
                ?authoritative_space,
                "window is being dragged; not reassigned"
            );
            return false;
        }
        // Native WindowServer visibility is not enough to participate in Rift's
        // layout. Fullscreen exit can surface transient AppKit/Electron windows
        // that are visible and space-owned but are filtered out of query output.
        // Treat this as the single gate for authoritative-space reconciliation:
        // if a window is not query-manageable, remove any stale layout/workspace
        // membership instead of re-assigning it from the WindowServer snapshot.
        if !self.state.windows.window(wid).is_some_and(WindowState::is_admitted) {
            let changed_space = self.assigned_space_for_window_id(wid);
            self.send_layout_event(LayoutEvent::WindowRemoved(wid));
            return changed_space.is_some_and(|space| self.is_space_active(space));
        }

        let assigned_space = self.assigned_space_for_window_id(wid);
        if assigned_space == Some(authoritative_space) {
            return self.restore_window_to_active_layout_if_visible(wid, authoritative_space);
        }

        self.send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(wid));

        let _ = self
            .layout_manager
            .layout_engine
            .virtual_workspace_manager_mut()
            .list_workspaces(authoritative_space);

        let assigned = if preserve_workspace_ordinal {
            self.layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .assign_window_to_workspace_preserving_ordinal(
                    &mut self.state.windows,
                    authoritative_space,
                    wid,
                )
                .is_some()
        } else {
            let Some(target_workspace) = self
                .layout_manager
                .layout_engine
                .ensure_active_workspace_info(authoritative_space)
                .map(|(workspace_id, _)| workspace_id)
                .or_else(|| {
                    self.layout_manager.layout_engine.active_workspace(authoritative_space)
                })
            else {
                return assigned_space.is_some_and(|space| self.is_space_active(space));
            };

            self.layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .assign_window_to_workspace(
                    &mut self.state.windows,
                    authoritative_space,
                    wid,
                    target_workspace,
                )
        };
        if !assigned {
            return assigned_space.is_some_and(|space| self.is_space_active(space));
        }

        let target_active = self.is_space_active(authoritative_space);
        let _ = self.restore_window_to_active_layout_if_visible(wid, authoritative_space);

        assigned_space.is_some_and(|space| self.is_space_active(space)) || target_active
    }

    fn restore_window_to_active_layout_if_visible(
        &mut self,
        wid: WindowId,
        authoritative_space: SpaceId,
    ) -> bool {
        if !self.is_space_active(authoritative_space) {
            return false;
        }

        let Some(window) = self.state.windows.window(wid) else {
            return false;
        };
        // Same invariant as `reassign_window_to_authoritative_space`: a visible
        // WindowServer id may be a transient fullscreen projection. Do not let
        // visibility alone add it back to the active layout.
        if !window.is_admitted() {
            self.send_layout_event(LayoutEvent::WindowRemoved(wid));
            return false;
        }

        let Some(wsid) = window.info.sys_id else {
            return false;
        };
        if !self.state.windows.is_window_visible(wsid) {
            return false;
        }

        let was_on_active_space = self.is_window_on_active_space(wid);
        self.send_layout_event(LayoutEvent::WindowAdded(authoritative_space, wid));
        !was_on_active_space && self.is_window_on_active_space(wid)
    }

    fn reconcile_windows_in_authoritative_active_snapshot(
        &mut self,
        active_windows: &[(WindowServerId, Option<SpaceId>)],
    ) -> bool {
        if self.refreshes_blocked() {
            self.defer_window_inventory_refresh();
            return false;
        }

        let windows: Vec<_> = active_windows
            .iter()
            .filter_map(|&(wsid, observed_space)| {
                let wid = self.state.windows.tracked_window_id(wsid)?;
                let authoritative_space =
                    self.state.windows.window_server_space(wsid).or(observed_space)?;
                Some((wid, authoritative_space))
            })
            .collect();
        let mut layout_changed = false;

        for (wid, authoritative_space) in windows {
            layout_changed |= self.reassign_window_to_authoritative_space(wid, authoritative_space);
        }

        layout_changed
    }

    #[cfg(test)]
    fn reconcile_windows_with_authoritative_spaces(&mut self) -> bool {
        let active_windows: Vec<_> = self
            .state
            .windows
            .iter_windows()
            .filter_map(|(wid, state)| {
                let wsid = state.info.sys_id?;
                Some((wsid, self.authoritative_space_for_window_id(wid)))
            })
            .collect();
        self.reconcile_windows_in_authoritative_active_snapshot(&active_windows)
    }

    fn current_reported_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        self.state
            .windows
            .window(wid)
            .and_then(|window| window.info.sys_id)
            .and_then(|wsid| self.resolve_native_space(wsid, None))
    }

    fn authoritative_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        let reported_space = self.current_reported_space_for_window_id(wid);
        if let Some(hidden_assigned_space) = self.hidden_assigned_space_for_window_id(wid) {
            return match reported_space {
                Some(space) if space != hidden_assigned_space => Some(space),
                _ => Some(hidden_assigned_space),
            };
        }

        reported_space.or_else(|| self.assigned_space_for_window_id(wid))
    }

    /// Resolve native space ownership from the strongest available source.
    ///
    /// `observation` is a direct per-space membership observation. A pending
    /// Rift move wins over an observation that is not backed by the live
    /// WindowServer state, while a live conflict is treated as a newer external
    /// move. With no direct observation, the live WindowServer query wins over
    /// the accepted prior observation and the pending target wins over stale
    /// cached state.
    pub(crate) fn resolve_native_space(
        &self,
        wsid: WindowServerId,
        observation: Option<SpaceId>,
    ) -> Option<SpaceId> {
        if let Some(pin) = self.drag_manager.drop_pin
            && pin.window == wsid
            && crate::sys::trace::now() < pin.until
        {
            return Some(pin.space);
        }
        // A window rift itself sent home to a returned display: the report
        // that it arrived is the move landing, whatever frame write is
        // pending for it in the tree it left.
        if let Some(observed) = observation
            && self
                .state
                .windows
                .tracked_window_id(wsid)
                .and_then(|wid| self.display_archive.homing_destination(wid))
                == Some(observed)
        {
            return Some(observed);
        }
        let pending = self.pending_target_space_for_window_server_id(wsid);
        let live = window_server::window_space(wsid);
        let prior = self.state.windows.window_server_space(wsid);

        match (observation, pending) {
            (Some(observed), Some(target)) if observed != target => {
                // A write is in flight to `target`'s display. The server
                // reports where the window still is, not where it is going,
                // and asking it again gets the same stale answer — acting on
                // it re-tiled the window back on the display it was leaving,
                // and that write moved it again once the first landed: the
                // flicker between displays after a cross-display drop. A
                // fresh write stands; only one the app has had every chance
                // to apply yields to a server that still disagrees.
                let in_flight =
                    self.transaction_manager.target_sent_within(wsid, managers::DropPin::HOLD);
                if !in_flight && live == Some(observed) {
                    Some(observed)
                } else {
                    Some(target)
                }
            }
            (Some(observed), _) => Some(observed),
            (None, _) => live.or(pending).or(prior),
        }
    }

    fn best_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        self.authoritative_space_for_window_id(wid).or_else(|| {
            self.state
                .windows
                .window(wid)
                .and_then(|window| self.best_space_for_window_state(window))
        })
    }

    fn is_window_on_known_inactive_space(&self, wid: WindowId) -> bool {
        self.authoritative_space_for_window_id(wid)
            .is_some_and(|space| !self.is_space_active(space))
    }

    fn discovery_space_for_window_id(&self, wid: WindowId) -> Option<SpaceId> {
        let window = self.state.windows.window(wid)?;
        let authoritative = self.authoritative_space_for_window_id(wid);
        if let Some(space) = authoritative {
            return Some(space);
        }

        if let Some(space) = self.best_space_for_frame(&window.frame_monotonic)
            && self.is_space_active(space)
        {
            return Some(space);
        }

        self.best_space_for_window_id(wid)
    }

    pub(crate) fn geometry_space_for_window(
        &self,
        frame: &CGRect,
        window_server_id: Option<WindowServerId>,
    ) -> Option<SpaceId> {
        if let Some(wsid) = window_server_id
            && self.is_known_fullscreen_window(wsid)
        {
            return None;
        }

        if let Some(space) = self.hidden_assigned_space_for_frame(window_server_id, frame) {
            return Some(space);
        }

        self.best_space_for_frame(frame)
    }

    fn is_known_fullscreen_window(&self, wsid: WindowServerId) -> bool {
        self.state.windows.is_window_server_id_native_fullscreen_suspended(wsid)
    }

    /// The window `wid` stands for: itself when it is an admitted top-level
    /// window, otherwise the same app's admitted root window whose frame
    /// contains it (or, failing that, the largest one).
    fn admitted_root_for(&self, wid: WindowId) -> WindowId {
        if self.state.windows.window(wid).is_some_and(|window| window.is_admitted()) {
            return wid;
        }
        // The child→root remap is only for windows that can actually BE a
        // child: tracked, and not themselves a root standard window
        // (Lightroom's filmstrip panel). Everything else passes through
        // unmapped — a window rift has never seen, and a top-level window
        // that merely is not admitted yet (Preview's second document,
        // rejected at birth on a transient layer). Remapping those glued
        // focus, and every focused-window command, onto an arbitrary
        // admitted sibling — sa+T on the fresh document toggled the
        // previous one — and poisoned the readmission path's candidate,
        // so the real window was never judged again either.
        let is_possible_child = self
            .state
            .windows
            .window(wid)
            .is_some_and(|window| !(window.info.is_root && window.info.is_standard));
        if !is_possible_child {
            return wid;
        }
        let child = self.live_frame_for(wid);
        let mut candidates: Vec<(WindowId, CGRect)> = self
            .state
            .windows
            .iter_windows()
            .filter(|(other, state)| other.pid == wid.pid && *other != wid && state.is_admitted())
            .filter_map(|(other, _)| self.live_frame_for(other).map(|frame| (other, frame)))
            .collect();
        if let Some(child) = child
            && let Some((parent, _)) =
                candidates.iter().find(|(_, frame)| frame.contains(child.mid()))
        {
            return *parent;
        }
        candidates.sort_by(|(_, a), (_, b)| {
            (b.size.width * b.size.height).total_cmp(&(a.size.width * a.size.height))
        });
        candidates.first().map(|(parent, _)| *parent).unwrap_or(wid)
    }

    /// Bring rift's frame record for every floating window in line with the
    /// window server before a layout pass. See the arrange in `managers.rs`.
    pub(crate) fn refresh_floating_frames_from_window_server(&mut self) {
        let floats: Vec<(WindowId, WindowServerId)> = self
            .state
            .windows
            .iter_windows()
            .filter(|(wid, _)| self.layout_manager.layout_engine.is_window_floating(*wid))
            .filter_map(|(wid, state)| state.info.sys_id.map(|wsid| (wid, wsid)))
            .collect();
        for (wid, wsid) in floats {
            // A window the server has not laid out yet (a panel being
            // created, a window ordered out) reports an empty frame. That is
            // the absence of a frame, not a frame; adopting it made the next
            // layout pass write a 0x0 window to the middle of the screen.
            if let Some(frame) = window_server::live_window_frame(wsid)
                && frame.size.width > 0.0
                && frame.size.height > 0.0
                && let Some(window) = self.state.windows.window_mut(wid)
                && !window.frame_monotonic.same_as(frame)
            {
                window.frame_monotonic = frame;
            }
        }
    }

    /// The window's frame as the window server has it, falling back to
    /// rift's own record. Some apps (Lightroom) never report their main
    /// window moving over accessibility, so the record goes stale; anything
    /// aimed at the window — the pointer, above all — must use the truth.
    fn live_frame_for(&self, wid: WindowId) -> Option<CGRect> {
        let window = self.state.windows.window(wid)?;
        Some(
            window
                .info
                .sys_id
                .and_then(window_server::live_window_frame)
                .unwrap_or(window.frame_monotonic),
        )
    }

    fn window_center_on_known_screen(&self, wid: WindowId) -> Option<CGPoint> {
        let window_center = self.live_frame_for(wid)?.mid();
        self.screen_for_point(window_center).map(|_| window_center)
    }

    pub fn warp_mouse(&mut self, point: CGPoint) {
        #[cfg(test)]
        self.test_mouse_warps.push(point);
        let Some(event_tap_tx) = self.communication_manager.event_tap_tx.clone() else {
            return;
        };
        _ = event_tap_tx.send(crate::actor::event_tap::Request::Warp(point));
    }

    fn warp_mouse_to_space_center(&mut self, space: SpaceId) -> bool {
        let Some(screen) = self.space_state.screen_by_space(space) else {
            return false;
        };
        self.warp_mouse(screen.frame.mid());
        true
    }

    fn try_focus_or_warp_without_raise(
        &mut self,
        warp_space: Option<SpaceId>,
        focus_window: &mut Option<WindowId>,
    ) -> bool {
        if let Some(wid) = self.window_id_under_cursor() {
            *focus_window = Some(wid);
            return false;
        }
        if self.focus_untracked_window_under_cursor() {
            return true;
        }
        self.config.settings.mouse_follows_focus
            && warp_space.is_some_and(|space| self.warp_mouse_to_space_center(space))
    }

    /// Whether `mouse_follows_focus` should warp the cursor onto this window.
    ///
    /// Global setting first, then the per-app opt-out. The window keeps focus
    /// either way; only the cursor warp is suppressed.
    /// Records what a modifier drag started on.
    ///
    /// Which edges a resize moves is decided here, from where in the window the
    /// press landed, exactly as dragging a corner would: press in the left half
    /// and the left edge follows the cursor, press in the bottom-right and both
    /// the right and bottom edges do. Deciding this per movement instead is
    /// what made the window travel the wrong way — growing from the origin
    /// always moves the right and bottom edges, whichever way the cursor went.
    fn begin_mouse_modifier_drag(
        &mut self,
        window_server_id: WindowServerId,
        at: CGPoint,
        action: crate::common::config::MouseAction,
    ) {
        self.modifier_drag = None;
        let Some(wid) = self.state.windows.tracked_window_id(window_server_id) else {
            return;
        };
        let Some(window) = self.state.windows.window(wid) else {
            return;
        };
        let frame = window.frame_monotonic;
        self.modifier_drag = Some(ModifierDragState {
            window: wid,
            action,
            origin_frame: frame,
            edges: ResizeEdges::from_press(frame, at),
        });
    }

    /// Applies movement during a modifier drag.
    ///
    /// `dx`/`dy` are measured from where the drag began and applied to the
    /// frame captured then, so the window tracks the cursor exactly however
    /// long the drag runs. Returns a layout event when the change has to go
    /// through the layout engine, which is the case for tiled windows.
    fn handle_mouse_modifier_drag(&mut self, dx: f64, dy: f64) -> Option<LayoutEvent> {
        use crate::common::config::MouseAction;

        let drag = self.modifier_drag?;
        let wid = drag.window;
        let old_frame = self.state.windows.window(wid)?.frame_monotonic;

        let target = match drag.action {
            MouseAction::Move => {
                let mut frame = drag.origin_frame;
                frame.origin.x += dx;
                frame.origin.y += dy;
                frame
            }
            MouseAction::Resize => drag.edges.apply(drag.origin_frame, dx, dy),
            MouseAction::None => return None,
        };

        if !self.layout_manager.layout_engine.is_window_floating(wid) {
            // A tiled window's frame belongs to its layout, so writing one is
            // pointless: the next arrange overwrites it. Moving one has no
            // meaning either — drag-to-swap already covers that — but a resize
            // does, so report it as the resize it is and let the ordinary
            // resize path fold it in. That is the path a drag of the window's
            // own edge takes, and it already knows each container's pixel size
            // and gaps.
            if drag.action != MouseAction::Resize {
                return None;
            }
            let screens = self
                .space_state
                .screens
                .iter()
                .filter_map(|screen| {
                    Some((screen.space?, screen.frame, screen.display_uuid_owned()))
                })
                .collect();
            return Some(LayoutEvent::WindowResized {
                wid,
                old_frame,
                new_frame: target,
                screens,
            });
        }

        let window_server_id = self.state.windows.window(wid).and_then(|w| w.info.sys_id);
        let transaction = match window_server_id {
            Some(window_server_id) => {
                let transaction = self.transaction_manager.generate_next_txid(window_server_id);
                self.transaction_manager.store_txid(window_server_id, transaction, target);
                transaction
            }
            None => TransactionId::default(),
        };
        if let Some(app) = self.app_manager.apps.get(&wid.pid)
            && let Err(error) =
                app.handle.send(Request::SetWindowFrame(wid, target, transaction, true))
        {
            warn!(window = ?wid, %error, "failed to apply modifier drag");
        }
        // A floating window's frame is written directly; the layout is not
        // involved.
        None
    }

    /// Creates a space immediately to the right of the active one and switches
    /// to it.
    ///
    /// The addition can only append a space to the end of its display, so the
    /// new one is then reordered to sit after the active space — which is what
    /// a `space --create` followed by a loop of `space --move prev` was doing,
    /// in one step rather than one per space in between.
    fn create_space_after_active(&mut self) {
        use crate::sys::{scripting_addition, space_switch};

        let active = space_switch::active_space();
        let before = space_switch::spaces_on_active_display().unwrap_or_default();

        if !scripting_addition::create_space(active.get()) {
            self.fail_command(SA_REQUIRED_CREATE);
            return;
        }

        let Some(created) = space_switch::spaces_on_active_display()
            .unwrap_or_default()
            .into_iter()
            .find(|space| !before.contains(space))
        else {
            self.fail_command("created a space but could not find it on the display");
            return;
        };

        // Already in the right place when the active space was the last one.
        if before.last() == Some(&active) {
            scripting_addition::focus_space(created.get());
            return;
        }
        if !scripting_addition::move_space_after_space(created.get(), active.get(), true) {
            warn!(?created, "Created the space but could not move it into place");
        }
    }

    /// Moves the focused window to a macOS space by 1-based index.
    ///
    /// Goes through rift's scripting addition, because macOS 26 leaves no
    /// unprivileged way to do it — see `sys::scripting_addition`. The window
    /// server tells rift where the window went, so nothing here has to update
    /// the model by hand.
    fn move_focused_window_to_space(&mut self, index: usize, follow: bool) {
        let Some(space) = crate::sys::space_switch::space_at_index(index) else {
            self.fail_command(format!("there is no macOS space {index}"));
            return;
        };
        let Some(wid) = self.main_window().or_else(|| self.window_id_under_cursor()) else {
            self.fail_command("there is no focused window to move");
            return;
        };
        let Some(window_server_id) =
            self.state.windows.window(wid).and_then(|window| window.info.sys_id)
        else {
            self.fail_command("that window has no window-server id, so it cannot be moved");
            return;
        };

        if !crate::sys::scripting_addition::move_window_to_space(
            window_server_id.as_u32(),
            space.get(),
        ) {
            self.fail_command(SA_REQUIRED_MOVE);
            return;
        }
        self.note_window_sent_to_space(window_server_id);
        if follow {
            unsafe {
                crate::sys::space_switch::switch_to_space_index(
                    index,
                    self.config.settings.space_switch_method,
                )
            };
        }
    }

    /// Whether the pointer is what changed focus: the button is down, or
    /// only just came up. Clicking into a window, clicking the menu bar of
    /// another display (which activates the app shown there), dismissing a
    /// status-item popover — the pointer is where the user put it, and
    /// moving it to the centre of whatever became frontmost is never what
    /// they meant.
    ///
    /// Unless a key was pressed after that click. Select a paragraph with a
    /// triple-click and cmd-tab straight to the other app: the focus change
    /// lands well inside the grace, but the keyboard made it, and the click
    /// only happened to be recent. Every pointer interaction above ends with
    /// a release and no key, so the key is what tells them apart.
    fn focus_change_is_pointer_driven(&self) -> bool {
        crate::sys::event::get_mouse_state() == Some(crate::sys::event::MouseState::Down)
            || (self.last_mouse_up.is_some_and(|at| {
                crate::sys::trace::now().saturating_duration_since(at) < Self::CLICK_FOCUS_GRACE
            }) && !crate::sys::event::key_pressed_since_mouse_up())
    }

    /// yabai skips the warp when the pointer already sits inside the window
    /// being focused (window_manager_center_mouse). Focusing something you
    /// are already hovering should not move your hand, and it matters most
    /// when stepping through a stack: every window there has the same frame,
    /// so each step would otherwise yank the cursor back to the middle.
    fn pointer_is_inside(&self, wid: WindowId) -> bool {
        self.live_frame_for(wid).is_some_and(|frame| {
            window_server::current_cursor_location().is_ok_and(|cursor| frame.contains(cursor))
        })
    }

    /// A click on a Dock tile is the one click that means the opposite of
    /// both gates above: the pointer is on the Dock, the window it summons is
    /// somewhere else entirely, and arriving there is the whole of the
    /// request. So this click, alone among clicks, still warps — and so does
    /// a Dock click onto a window the auto-hidden Dock happens to sit over.
    ///
    /// Only once the button is up. Dragging a file onto a tile activates the
    /// app too, and yanking the pointer off the Dock mid-drag would drop the
    /// file somewhere the user never aimed at.
    fn click_landed_on_the_dock(&self) -> bool {
        crate::sys::event::get_mouse_state() != Some(crate::sys::event::MouseState::Down)
            && window_server::pointer_is_over_dock()
    }

    fn mouse_follows_focus_allowed_for(&self, wid: WindowId) -> bool {
        if !self.mouse_follows_focus_permitted_for_app(wid) {
            return false;
        }
        // Both of these say the same thing: the pointer is already where the
        // user put it, so leave it there.
        !((self.focus_change_is_pointer_driven() || self.pointer_is_inside(wid))
            && !self.click_landed_on_the_dock())
    }

    /// The setting and the per-app opt-out alone, without asking where the
    /// pointer is. For a warp that follows a window the user just moved: the
    /// pointer was inside it a moment ago by definition, which is no reason
    /// not to follow it to where it went.
    fn mouse_follows_focus_permitted_for_app(&self, wid: WindowId) -> bool {
        if !self.config.settings.mouse_follows_focus {
            return false;
        }
        if self.config.settings.mouse_follows_focus_blacklist.is_empty() {
            return true;
        }
        let Some(bundle_id) = self
            .app_manager
            .apps
            .get(&wid.pid)
            .and_then(|app| app.info.bundle_id.as_deref())
        else {
            return true;
        };
        !self
            .config
            .settings
            .mouse_follows_focus_blacklist
            .iter()
            .any(|blocked| blocked == bundle_id)
    }

    fn insert_app_handle_for_window(
        &self,
        app_handles: &mut HashMap<pid_t, AppThreadHandle>,
        wid: WindowId,
    ) {
        if let Some(app) = self.app_manager.apps.get(&wid.pid) {
            app_handles.insert(wid.pid, app.handle.clone());
        }
    }

    fn expose_all_spaces(&mut self) {
        let spaces: Vec<SpaceId> = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| screen.space)
            .filter(|space| self.is_space_active(*space))
            .collect();
        for space in spaces {
            self.expose_space_if_known(space);
        }
    }

    fn window_is_standard(&self, id: WindowId) -> bool {
        self.state.windows.window(id).is_some_and(WindowState::is_admitted)
    }

    pub(crate) fn visible_spaces_for_layout(
        &self,
        include_inactive: bool,
    ) -> (Vec<SpaceId>, HashMap<SpaceId, CGPoint>) {
        let visible_spaces_input: Vec<(SpaceId, CGPoint)> = self
            .space_state
            .screens
            .iter()
            .filter_map(|screen| {
                let space = screen.space?;
                if !include_inactive && !self.is_space_active(space) {
                    return None;
                }
                Some((space, screen.frame.mid()))
            })
            .collect();

        let mut visible_space_centers = HashMap::default();
        for (space, center) in &visible_spaces_input {
            visible_space_centers.insert(*space, *center);
        }

        let visible_spaces = order_visible_spaces_by_position(visible_spaces_input.iter().cloned());

        (visible_spaces, visible_space_centers)
    }

    fn send_layout_event(&mut self, event: LayoutEvent) {
        // Nothing that is not admitted goes into a layout, whichever path asks.
        // Discovery checks this, but the drag-end and cross-space paths did
        // not: Lightroom reports a drag of its main window as frame changes
        // on one of its *panels* (a non-standard AX window), so the panel got
        // a drag session, was added to the tree on mouse-up, and every
        // arrange from then on shoved Lightroom's panels around and wrote the
        // real window's stale frame back.
        if let LayoutEvent::WindowAdded(space, wid) = &event
            && self.state.windows.window(*wid).is_some_and(|window| !window.is_admitted())
        {
            debug!(
                ?wid,
                space = space.get(),
                "Refusing to add a window that is not admitted"
            );
            return;
        }
        if matches!(event, LayoutEvent::WindowRemovedPreserveFloating(_)) {
            self.capture_pre_churn_layout();
        }
        self.note_fullscreen_slot_lifecycle(&event);
        // A window coming back from native fullscreen is put into the tree by
        // whichever path notices it first, and none of them knows where it
        // used to sit. Note which slots are still waiting before the engine
        // sees this event; any of them tiled afterwards was inserted by it,
        // and gets its slot back before this event's arrange writes a frame.
        let slots_awaiting_insertion = self.fullscreen_slots_awaiting_insertion();
        let focus_changed = matches!(
            &event,
            LayoutEvent::WindowFocused(_, window)
                if self.layout_manager.layout_engine.focused_window() != Some(*window)
        );
        let event_space = match &event {
            LayoutEvent::WindowFocused(space, _) => Some(*space),
            _ => None,
        };
        let focus_desktop = matches!(
            event,
            LayoutEvent::WindowRemoved(wid)
                if self.layout_manager.layout_engine.focused_window() == Some(wid)
        );
        let event_clone = event.clone();
        let layout_outcome =
            self.layout_manager.layout_engine.handle_event(&mut self.state.windows, event);
        let mut response = layout_outcome.response;
        for (window, space) in slots_awaiting_insertion {
            if self.layout_manager.layout_engine.is_window_tiled(space, window)
                && self.reinstate_fullscreen_slot(window, space)
            {
                response.changed = true;
            }
        }
        let (placements, resizes, workspace_focus) = layout_outcome.app_rules.into_parts();
        self.apply_app_rule_placements(placements);
        self.apply_app_rule_resizes(resizes);
        let workspace_switch_space = workspace_focus.map(|request| request.space);
        if let Some(request) = workspace_focus {
            self.store_current_floating_positions(request.space);
            self.workspace_switch_manager
                .start_workspace_switch(WorkspaceSwitchOrigin::Auto);
            response = self.layout_manager.layout_engine.switch_to_workspace_with_focus(
                &self.state.windows,
                request.space,
                request.workspace_index,
                request.window,
            );
        }
        if focus_changed && let Some(event_tap_tx) = &self.communication_manager.event_tap_tx {
            _ = event_tap_tx.send(crate::actor::event_tap::Request::HideOnFocus);
        }
        let geometry_changed = response.changed;
        self.prepare_refocus_after_layout_event(&event_clone);
        self.handle_layout_response(response, workspace_switch_space);
        if geometry_changed {
            self.update_layout_or_warn(
                false,
                workspace_switch_space.is_some(),
                workspace_switch_space.or(event_space),
            );
        }
        if focus_desktop && let Some(space) = self.workspace_command_space() {
            self.focus_desktop_if_active_workspace_empty(space);
        }
        for space in self.space_state.iter_known_spaces() {
            self.layout_manager.layout_engine.debug_tree_desc(space, "after event", false);
        }
    }

    fn apply_app_rule_placements(
        &mut self,
        placements: Vec<crate::model::app_rules::AppRulePlacement>,
    ) {
        for placement in placements {
            let Some(window) = self.state.windows.window(placement.window) else {
                continue;
            };
            let frame = if placement.position.is_some() {
                let Some(screen) = self.space_state.screen_by_space(placement.space) else {
                    warn!(
                        window = ?placement.window,
                        space = ?placement.space,
                        "could not apply app-rule position without screen geometry"
                    );
                    continue;
                };
                placement.resolve_frame(window.frame_monotonic, screen.frame)
            } else {
                placement.resolve_frame(window.frame_monotonic, CGRect::default())
            };

            let window_server_id = window.info.sys_id;
            let transaction = if let Some(window_server_id) = window_server_id {
                let transaction = self.transaction_manager.generate_next_txid(window_server_id);
                self.transaction_manager.store_txid(window_server_id, transaction, frame);
                transaction
            } else {
                TransactionId::default()
            };
            if let Some(app) = self.app_manager.apps.get(&placement.window.pid)
                && let Err(error) = app.handle.send(Request::SetWindowFrame(
                    placement.window,
                    frame,
                    transaction,
                    true,
                ))
            {
                warn!(window = ?placement.window, %error, "failed to apply app-rule placement");
            }
        }
    }

    fn apply_app_rule_resizes(&mut self, resizes: Vec<crate::model::app_rules::AppRuleResize>) {
        for resize in resizes {
            let Some(window) = self.state.windows.window(resize.window) else {
                continue;
            };
            let Some(screen) = self.space_state.screen_by_space(resize.space) else {
                warn!(
                    window = ?resize.window,
                    space = ?resize.space,
                    "could not apply app-rule resize without screen geometry"
                );
                continue;
            };
            let old_frame = window.frame_monotonic;
            let mut new_frame = old_frame;
            if let Some(width) = resize.size.w {
                new_frame.size.width = width;
            }
            if let Some(height) = resize.size.h {
                new_frame.size.height = height;
            }
            self.layout_manager.layout_engine.apply_app_rule_resize(
                resize,
                old_frame,
                new_frame,
                screen.frame,
                Some(screen.display_uuid.as_str()),
            );
        }
    }

    // Returns true if the window should be raised on mouse over considering
    // active workspace membership and potential occlusion of floating windows above it.
    pub(crate) fn should_raise_on_mouse_over(&self, wid: WindowId) -> bool {
        let Some(window) = self.state.windows.window(wid) else {
            return false;
        };

        if !window.is_admitted() && !self.layout_manager.layout_engine.is_window_floating(wid) {
            return false;
        }

        let candidate_frame = window.frame_monotonic;

        if matches!(self.menu_manager.menu_state, MenuState::Open(_)) {
            trace!(?wid, "Skipping autoraise while menu open");
            return false;
        }

        let Some(space) = self.best_space_for_window(&candidate_frame, window.info.sys_id) else {
            return false;
        };
        if !self.is_space_active(space) {
            return false;
        }

        if !self.layout_manager.layout_engine.is_window_in_active_workspace(
            &self.state.windows,
            space,
            wid,
        ) {
            trace!("Ignoring mouse over window {:?} - not in active workspace", wid);
            return false;
        }

        let Some(candidate_wsid) = window.info.sys_id else {
            return true;
        };

        let order = {
            let space_id = space.get();
            crate::sys::window_server::space_window_list_for_connection(&[space_id], 0, false)
        };
        let candidate_u32 = candidate_wsid.as_u32();
        let candidate_level = window_level(candidate_u32);
        let candidate_sub_level = window_sub_level(candidate_u32);

        for above_u32 in order {
            if above_u32 == candidate_u32 {
                break;
            }

            let above_wsid = WindowServerId::new(above_u32);
            let Some(above_wid) = self.state.windows.tracked_window_id(above_wsid) else {
                continue;
            };

            if !self.layout_manager.layout_engine.is_window_floating(above_wid) {
                continue;
            }

            let Some(above_state) = self.state.windows.window(above_wid) else {
                continue;
            };
            let above_frame = above_state.frame_monotonic;
            if !candidate_frame.contains_rect(above_frame) {
                continue;
            }

            let above_level = window_level(above_u32);
            let above_sub_level = window_sub_level(above_u32);
            if candidate_level
                .zip(above_level)
                .is_some_and(|(candidate, above)| candidate == above)
                && candidate_sub_level == above_sub_level
            {
                return false;
            }
        }

        true
    }

    fn process_windows_for_app_rules(
        &mut self,
        window_ids: Vec<WindowId>,
        app_info: AppInfo,
        reapply_effects: bool,
    ) {
        if window_ids.is_empty() {
            return;
        }

        let mut windows_by_space: BTreeMap<SpaceId, Vec<WindowId>> = BTreeMap::new();
        for &wid in &window_ids {
            // A rule pass may move a window between workspaces; not while
            // the user is holding it.
            if self.window_in_drag() == Some(wid) {
                continue;
            }
            let Some(state) = self.state.windows.window(wid) else {
                continue;
            };
            if !state.can_reconcile_admission() {
                continue;
            }
            let Some(space) = self.best_space_for_window_id(wid) else {
                continue;
            };
            windows_by_space.entry(space).or_default().push(wid);
        }

        for (space, wids) in windows_by_space {
            if !self.is_space_active(space) {
                continue;
            }
            let mut windows_needing_layout_refresh = Vec::new();

            for wid in &wids {
                let (previous_workspace, was_floating, was_ignored) = {
                    let engine = &self.layout_manager.layout_engine;
                    (
                        engine.virtual_workspace_manager().workspace_for_window(
                            &self.state.windows,
                            space,
                            *wid,
                        ),
                        engine.is_window_floating(*wid),
                        self.state
                            .windows
                            .window(*wid)
                            .is_some_and(|window| window.manage_override == Some(false)),
                    )
                };
                let assign_result = {
                    let window_metadata = self.state.windows.window(*wid).map(|window| {
                        (
                            window.info.title.clone(),
                            window.info.ax_role.clone(),
                            window.info.ax_subrole.clone(),
                        )
                    });
                    let engine = &mut self.layout_manager.layout_engine;
                    if reapply_effects {
                        engine.reapply_window_with_app_info(
                            &mut self.state.windows,
                            *wid,
                            space,
                            app_info.bundle_id.as_deref(),
                            app_info.localized_name.as_deref(),
                            window_metadata.as_ref().map(|metadata| metadata.0.as_str()),
                            window_metadata.as_ref().and_then(|metadata| metadata.1.as_deref()),
                            window_metadata.as_ref().and_then(|metadata| metadata.2.as_deref()),
                        )
                    } else {
                        engine.assign_window_with_app_info(
                            &mut self.state.windows,
                            *wid,
                            space,
                            app_info.bundle_id.as_deref(),
                            app_info.localized_name.as_deref(),
                            window_metadata.as_ref().map(|metadata| metadata.0.as_str()),
                            window_metadata.as_ref().and_then(|metadata| metadata.1.as_deref()),
                            window_metadata.as_ref().and_then(|metadata| metadata.2.as_deref()),
                        )
                    }
                };

                match assign_result {
                    Ok(AppRuleResult::Managed(assignment)) => {
                        let effective_floating = assignment.should_float(was_floating);
                        let needs_layout_refresh = reapply_effects
                            || previous_workspace != Some(assignment.workspace_id)
                            || was_floating != effective_floating
                            || was_ignored;
                        if needs_layout_refresh {
                            windows_needing_layout_refresh.push((*wid, assignment));
                        }
                    }
                    Ok(AppRuleResult::Rejected(_)) => {
                        if utils::rejection_needs_removal(
                            &self.state,
                            &self.layout_manager,
                            *wid,
                            space,
                        ) {
                            self.send_layout_event(LayoutEvent::WindowRemoved(*wid));
                        }
                    }
                    Err(e) => {
                        warn!("Failed to assign window {:?} to workspace: {:?}", wid, e);
                        utils::clear_rule_admission(&mut self.state, *wid);
                    }
                }
            }

            if windows_needing_layout_refresh.is_empty() {
                continue;
            }

            for (wid, effects) in windows_needing_layout_refresh {
                let Some(window) = self.state.windows.window(wid) else {
                    continue;
                };
                self.send_layout_event(LayoutEvent::WindowObserved(space, ResolvedWindow {
                    info: window.layout_info(wid),
                    effects,
                }));
            }
        }
    }

    fn handle_app_activation_workspace_switch(&mut self, pid: pid_t) -> EventOutcome {
        if self.refresh_quarantine_manager.suppress_auto_workspace_switch_until_input {
            debug!(
                pid,
                "Skipping auto workspace switch for lifecycle-restored activation before user input"
            );
            return EventOutcome::no_change();
        }

        if self.workspace_switch_manager.active_workspace_switch.is_some() {
            trace!(
                "Skipping auto workspace switch for pid {} because a workspace switch is in progress",
                pid
            );
            return EventOutcome::no_change();
        }

        if self.workspace_switch_manager.manual_switch_in_progress() {
            debug!(
                "Skipping auto workspace switch for pid {} because a manual switch is in progress",
                pid
            );
            return EventOutcome::no_change();
        }

        if let Some(active_space) = self.raw_command_space()
            && self.is_fullscreen_space(active_space)
        {
            debug!(
                "Skipping auto workspace switch for pid {} because the active space is fullscreen",
                pid
            );
            return EventOutcome::no_change();
        }

        if let Some(wsid) = self.activation_from_unmanageable_window(pid) {
            debug!(
                ?wsid,
                "Skipping auto workspace switch for pid {} because the activated window is not manageable",
                pid
            );
            return EventOutcome::no_change();
        }

        let Some(bundle_id_str) =
            self.app_manager.apps.get(&pid).and_then(|app| app.info.bundle_id.clone())
        else {
            return EventOutcome::no_change();
        };

        if self.config.settings.auto_focus_blacklist.contains(&bundle_id_str) {
            debug!(
                "App {} is blacklisted for auto-focus workspace switching, ignoring activation",
                bundle_id_str
            );
            return EventOutcome::no_change();
        }

        debug!(
            "App activation detected: {} (pid: {}), checking for workspace switch",
            bundle_id_str, pid
        );

        // Carbon activation is reconciled by the app thread before this runs,
        // so a missing main window means there is no authoritative switch
        // target. Picking an arbitrary window for the process is especially
        // unsafe for apps whose windows span multiple virtual workspaces.
        let app_window =
            self.main_window().filter(|wid| wid.pid == pid && self.window_is_standard(*wid));

        let Some(app_window_id) = app_window else {
            return EventOutcome::no_change();
        };

        let Some(window_space) = self.best_space_for_window_id(app_window_id) else {
            return EventOutcome::no_change();
        };

        self.maybe_auto_switch_to_window_workspace(pid, app_window_id, window_space)
    }

    fn maybe_auto_switch_to_window_workspace(
        &mut self,
        pid: pid_t,
        app_window_id: WindowId,
        window_space: SpaceId,
    ) -> EventOutcome {
        let workspace_state = self.layout_manager.layout_engine.virtual_workspace_manager();
        let Some(window_workspace) =
            workspace_state.workspace_for_window(&self.state.windows, window_space, app_window_id)
        else {
            return EventOutcome::no_change();
        };

        let Some(current_workspace) =
            self.layout_manager.layout_engine.active_workspace(window_space)
        else {
            return EventOutcome::no_change();
        };

        if window_workspace != current_workspace {
            let workspaces = self
                .layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .list_workspaces(window_space);
            if let Some((workspace_index, _)) =
                workspaces.iter().enumerate().find(|(_, (ws_id, _))| *ws_id == window_workspace)
            {
                debug!(
                    "Auto-switching to workspace {} for activated app (pid: {})",
                    workspace_index, pid
                );

                self.store_current_floating_positions(window_space);
                self.workspace_switch_manager
                    .start_workspace_switch(WorkspaceSwitchOrigin::Auto);

                let response = self.layout_manager.layout_engine.switch_to_workspace_with_focus(
                    &self.state.windows,
                    window_space,
                    workspace_index,
                    app_window_id,
                );
                return EventOutcome::layout_changed(false)
                    .with_layout_response(response, Some(window_space));
            }
        }

        EventOutcome::no_change()
    }

    fn handle_layout_response(
        &mut self,
        response: layout::EventResponse,
        workspace_switch_space: Option<SpaceId>,
    ) {
        if self.is_in_drag() {
            self.workspace_switch_manager.mark_workspace_switch_inactive();
            return;
        }

        let mut pending_refocus_space =
            match std::mem::replace(&mut self.refocus_manager.refocus_state, RefocusState::None) {
                RefocusState::Pending(space) => Some(space),
                RefocusState::None => None,
            };
        let layout::EventResponse {
            changed: _,
            raise_windows,
            mut focus_window,
            boundary_hit,
        } = response;

        if let Some(space) = workspace_switch_space
            && matches!(
                self.workspace_switch_manager.workspace_switch_state,
                WorkspaceSwitchState::Active
            )
        {
            focus_window = self.visible_focus_candidate_in_active_workspace(space, focus_window);
        }

        if let Some(dir) = boundary_hit
            && self.config.settings.layout.scrolling.gestures.propagate_to_workspace_swipe
        {
            let skip_empty = self.config.settings.gestures.skip_empty;
            let invert_horizontal =
                self.config.settings.layout.scrolling.gestures.invert_horizontal;
            let cmd = if invert_horizontal {
                match dir {
                    Direction::Left => Some(layout::LayoutCommand::NextWorkspace(Some(skip_empty))),
                    Direction::Right => {
                        Some(layout::LayoutCommand::PrevWorkspace(Some(skip_empty)))
                    }
                    _ => None,
                }
            } else {
                match dir {
                    Direction::Left => Some(layout::LayoutCommand::PrevWorkspace(Some(skip_empty))),
                    Direction::Right => {
                        Some(layout::LayoutCommand::NextWorkspace(Some(skip_empty)))
                    }
                    _ => None,
                }
            };
            if let Some(cmd) = cmd {
                let space = workspace_switch_space.or_else(|| self.command_context_space());
                if let Some(space) = space {
                    let resp = self.layout_manager.layout_engine.handle_virtual_workspace_command(
                        &mut self.state.windows,
                        space,
                        &cmd,
                    );

                    if self.config.settings.gestures.haptics_enabled {
                        let _ = crate::sys::haptics::perform_haptic(
                            self.config.settings.gestures.haptic_pattern,
                        );
                    }

                    // Recurse to handle the new response (e.g. focus window on the new workspace)
                    self.handle_layout_response(resp, Some(space));
                    self.update_event_tap_layout_mode();
                    return;
                }
            }
        }

        let original_focus = focus_window;

        let focus_quiet = workspace_switch_space.map_or(Quiet::No, |_| Quiet::Yes);

        let handled_without_raise = if raise_windows.is_empty() && focus_window.is_none() {
            if matches!(
                self.workspace_switch_manager.workspace_switch_state,
                WorkspaceSwitchState::Active
            ) && !self.is_in_drag()
            {
                if let Some(wid) = self.window_id_under_cursor() {
                    // Avoid duplicate focus events for the already focused window.
                    if self.main_window() != Some(wid) {
                        focus_window = Some(wid);
                    }
                    false
                } else {
                    let skip_center_warp = workspace_switch_space
                        .map(|space| {
                            self.layout_manager
                                .layout_engine
                                .windows_in_active_workspace(&self.state.windows, space)
                                .is_empty()
                        })
                        .unwrap_or(false);
                    if skip_center_warp {
                        workspace_switch_space.is_some_and(|space| {
                            self.focus_desktop_if_active_workspace_empty(space)
                        })
                    } else {
                        let space = workspace_switch_space.or_else(|| self.command_context_space());
                        self.try_focus_or_warp_without_raise(space, &mut focus_window)
                    }
                }
            } else if let Some(space) = pending_refocus_space.take() {
                if let Some(wid) = self.last_focused_window_in_space(space) {
                    focus_window = Some(wid);
                    false
                } else if !self.is_in_drag() {
                    self.try_focus_or_warp_without_raise(Some(space), &mut focus_window)
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if let Some(wid) = focus_window
            && let Some(state) = self.state.windows.window(wid)
            && let Some(wsid) = state.info.sys_id
        {
            let is_visible = self.state.windows.is_window_visible(wsid);
            let best_space = self.best_space_for_window_state(state);
            if !is_visible {
                focus_window = None;
                if let Some(space) = workspace_switch_space
                    && !self.is_in_drag()
                {
                    let _ = self.try_focus_or_warp_without_raise(Some(space), &mut focus_window);
                }
            } else if !best_space.is_some_and(|space| self.is_space_active(space)) {
                focus_window = None;
            }
        }

        if raise_windows.is_empty() && focus_window.is_none() {
            if handled_without_raise {
                self.workspace_switch_manager.mark_workspace_switch_inactive();
            }
            if handled_without_raise
                || matches!(
                    self.workspace_switch_manager.workspace_switch_state,
                    WorkspaceSwitchState::Inactive
                )
            {
                return;
            }
        }

        if let Some(space) = pending_refocus_space {
            // Preserve the pending refocus request if it was not consumed above.
            if matches!(self.refocus_manager.refocus_state, RefocusState::None) {
                self.refocus_manager.refocus_state = RefocusState::Pending(space);
            }
        }

        let mut app_handles = HashMap::default();
        for &wid in raise_windows.iter() {
            self.insert_app_handle_for_window(&mut app_handles, wid);
        }

        if let Some(wid) = original_focus {
            self.insert_app_handle_for_window(&mut app_handles, wid);
        }

        let raise_windows: Vec<WindowId> = raise_windows
            .into_iter()
            .filter(|wid| self.is_window_on_active_space(*wid))
            .collect();
        let focus_window = focus_window.filter(|wid| self.is_window_on_active_space(*wid));
        if let Some(space) = workspace_switch_space {
            self.layout_manager.layout_engine.commit_workspace_focus(
                &mut self.state.windows,
                space,
                focus_window,
            );
        }
        let mut windows_by_app_and_screen = HashMap::default();
        for &wid in &raise_windows {
            windows_by_app_and_screen
                .entry((wid.pid, self.best_space_for_window_id(wid)))
                .or_insert(vec![])
                .push(wid);
        }
        let focus_window_with_warp = focus_window.map(|wid| {
            let warp = if self.mouse_follows_focus_allowed_for(wid) {
                if self.workspace_switch_manager.workspace_switch_state
                    == WorkspaceSwitchState::Active
                {
                    // During workspace switches, defer mouse warping until after layout completes.
                    self.workspace_switch_manager.pending_workspace_mouse_warp = Some(wid);
                    None
                } else {
                    self.window_center_on_known_screen(wid)
                }
            } else {
                None
            };
            (wid, warp)
        });

        let msg = raise_manager::Event::RaiseRequest(RaiseRequest {
            raise_windows: windows_by_app_and_screen.into_values().collect(),
            focus_window: focus_window_with_warp,
            app_handles,
            focus_quiet,
        });

        if let Err(e) = self.communication_manager.raise_manager_tx.try_send(msg) {
            warn!("Failed to send raise request to raise manager: {}", e);
        }
    }

    fn collect_drag_swap_candidates(
        &self,
        wid: WindowId,
        space: SpaceId,
    ) -> Vec<(WindowId, CGRect)> {
        self.state
            .windows
            .iter_windows()
            .filter_map(|(other_wid, other_state)| {
                if other_wid == wid {
                    return None;
                }
                // Only a window in the layout can be swapped with or split.
                // Membership in the space's active tree is the whole test,
                // not workspace assignment (a window with no assignment
                // counts as being in the active workspace, which let popups
                // and minimized windows be offered as targets) and not the
                // window server's live answer: asking it where every window
                // is on every pointer move was ~12 queries per MouseDragged
                // — most of the drag's lag — and under the one-tree law the
                // tree already says which space a tiled window belongs to.
                let engine = &self.layout_manager.layout_engine;
                if !other_state.is_admitted()
                    || engine.is_window_floating(other_wid)
                    || !engine.is_window_tiled(space, other_wid)
                {
                    return None;
                }
                // A window in the same stack is no target. Every member of a
                // stack is given the container's whole rect, so the pointer
                // is over all of them at once and a swap trades two identical
                // places: the overlay drew the tile the dragged window is
                // already in and promised a move that could not happen. Under
                // the stack layout that is every other window on the space,
                // so a drag there now offers nothing rather than a
                // screen-sized rectangle that means nothing.
                if engine.windows_share_a_stack(space, wid, other_wid) {
                    return None;
                }
                Some((other_wid, other_state.frame_monotonic))
            })
            .collect()
    }

    /// Shows where the drop would put the dragged window.
    ///
    /// The region is the same one the drop itself will use: the whole target
    /// when the pointer is in its middle, and the half it would occupy after
    /// the split when the pointer is near an edge — so what is drawn is a
    /// promise about what will happen, not a hint.
    fn preview_drop_region(&mut self, dragged: WindowId, target: WindowId, cursor: CGPoint) {
        // Anything that leaves nothing to promise hides the overlay rather
        // than merely declining to update it, or a region drawn a moment ago
        // stays on screen for the rest of the drag.
        let Some(preview) = self.drop_preview_for(dragged, target, cursor) else {
            self.hide_drop_region();
            return;
        };
        // Re-aiming at the region already shown is pure churn — the pointer
        // moves at report rate, the region only when the zone changes.
        let unchanged = self.drag_manager.drop_overlay_shown
            && self.drag_manager.drop_preview_cache == Some(preview);
        self.drag_manager.drop_preview_cache = Some(preview);
        if unchanged {
            return;
        }
        if let Some(tx) = &self.communication_manager.drop_overlay_tx {
            tx.send(crate::actor::drop_overlay::Event::Aim {
                screen: preview.screen,
                region: preview.region,
            });
        }
        self.drag_manager.drop_overlay_shown = true;
    }

    /// What dropping `dragged` on `target` will do with the cursor at
    /// `cursor`, or `None` when the drop would not rearrange anything.
    ///
    /// This is the one place that decides, and both the preview and the drop
    /// itself ask it, so what the overlay draws is what the drop does. Only
    /// windows in the layout can be swapped or split, and an edge drop only
    /// splits where the layout can express one: everywhere else it falls back
    /// to a swap, and the preview shows the swap rather than a half that will
    /// never happen.
    fn drop_action_for(
        &self,
        dragged: WindowId,
        target: WindowId,
        cursor: CGPoint,
    ) -> Option<crate::actor::drag_swap::DropAction> {
        use crate::actor::drag_swap::DropAction;
        let engine = &self.layout_manager.layout_engine;
        if engine.is_window_floating(dragged) || engine.is_window_floating(target) {
            return None;
        }
        // Membership, not the window server: the target is a tiled window,
        // and asking the server where it lives on every pointer move was a
        // query per report for the whole drag.
        let space = self
            .assigned_space_for_window_id(target)
            .or_else(|| self.best_space_for_window_id(target))?;
        if !engine.is_window_tiled(space, target) {
            return None;
        }
        // A dragged window's membership is frozen for the drag, so during a
        // cross-display drag it is still tiled on its origin space.
        let dragged_space = self
            .assigned_space_for_window_id(dragged)
            .or_else(|| self.best_space_for_window_id(dragged))?;
        if !engine.is_window_tiled(dragged_space, dragged) {
            return None;
        }
        let frame = self.state.windows.window(target)?.frame_monotonic;
        let action = crate::actor::drag_swap::DragManager::drop_action(frame, cursor);
        if dragged_space == space {
            return Some(match action {
                DropAction::Insert(_) if !engine.can_insert_next_to(space) => DropAction::Swap,
                action => action,
            });
        }
        // Dropped from another display. A swap across two trees is nothing
        // the layout can express, so the whole target is edge zones: every
        // point over it means an insert on the side the cursor is nearest.
        // A dead middle made the overlay blink out and the drop land at the
        // tree's default slot instead of where the user pointed.
        if !engine.can_insert_next_to(space) {
            return None;
        }
        Some(DropAction::Insert(match action {
            DropAction::Insert(direction) => direction,
            DropAction::Swap => crate::actor::drag_swap::DragManager::edge_direction(frame, cursor),
        }))
    }

    /// `drop_action_for` with dwell: a zone change is believed only once the
    /// pointer has stayed in the new zone for `ZoneCandidate::DWELL`.
    /// Successive samples near a boundary land on either side of it, and
    /// following each one flapped the preview at report rate.
    fn sticky_drop_action(
        &mut self,
        dragged: WindowId,
        target: WindowId,
        cursor: CGPoint,
    ) -> Option<crate::actor::drag_swap::DropAction> {
        let raw = self.drop_action_for(dragged, target, cursor)?;
        let Some(cached) = self
            .drag_manager
            .drop_preview_cache
            .filter(|cached| cached.dragged == dragged && cached.target == target)
        else {
            self.drag_manager.zone_candidate = None;
            return Some(raw);
        };
        if raw == cached.action {
            self.drag_manager.zone_candidate = None;
            return Some(raw);
        }
        let now = crate::sys::trace::now();
        match self.drag_manager.zone_candidate {
            Some(candidate)
                if candidate.dragged == dragged
                    && candidate.target == target
                    && candidate.action == raw =>
            {
                if now.duration_since(candidate.since) >= managers::ZoneCandidate::DWELL {
                    self.drag_manager.zone_candidate = None;
                    Some(raw)
                } else {
                    Some(cached.action)
                }
            }
            _ => {
                self.drag_manager.zone_candidate = Some(managers::ZoneCandidate {
                    dragged,
                    target,
                    action: raw,
                    since: now,
                });
                Some(cached.action)
            }
        }
    }

    /// The screen and region a drop would land in, or `None` when the drop
    /// would not rearrange anything.
    ///
    /// The insert region is found by laying out a copy of the tree, which is
    /// far too expensive per pointer move; the region only depends on the
    /// (dragged, target, action) triple, which changes when the pointer
    /// crosses a zone, not with every report — so the last answer is kept on
    /// the drag and reused until the triple changes.
    fn drop_preview_for(
        &mut self,
        dragged: WindowId,
        target: WindowId,
        cursor: CGPoint,
    ) -> Option<managers::DropPreview> {
        if !self.config.settings.ui.drop_overlay.enabled {
            return None;
        }
        let action = self.sticky_drop_action(dragged, target, cursor)?;
        if let Some(cached) = self.drag_manager.drop_preview_cache
            && cached.dragged == dragged
            && cached.target == target
            && cached.action == action
        {
            return Some(cached);
        }
        let frame = self.state.windows.window(target)?.frame_monotonic;
        // The region belongs to the target's display, not the cursor's. The
        // two differ whenever the pointer crosses a display edge while the
        // window it drags still overlaps a target on the first one, and
        // drawing in the wrong display's window put the region wherever that
        // display's origin happened to shift it.
        let screen = self
            .screen_for_point(frame.mid())
            .or_else(|| self.screen_for_point(cursor))
            .map(|screen| screen.frame)?;

        let region = match action {
            // A swap hands the dragged window exactly the target's place.
            crate::actor::drag_swap::DropAction::Swap => frame,
            // A split reshapes the tree, so ask the layout where the window
            // ends up; the target's half is only a guess for when it cannot
            // say.
            crate::actor::drag_swap::DropAction::Insert(direction) => self
                .preview_insert_frame(target, direction, dragged)
                .unwrap_or_else(|| half_of(frame, direction)),
        };
        Some(managers::DropPreview {
            dragged,
            target,
            action,
            screen,
            region,
        })
    }

    /// The frame `window` would be given after being inserted on the
    /// `direction` side of `target`, laid out the way the real arrange pass
    /// would do it: same screen, gaps and stack-line allowance.
    fn preview_insert_frame(
        &self,
        target: WindowId,
        direction: Direction,
        window: WindowId,
    ) -> Option<CGRect> {
        let space = self.best_space_for_window_id(target)?;
        let screen = self.space_state.screen_by_space(space)?;
        let gaps = self
            .config
            .settings
            .layout
            .gaps
            .effective_for_display(screen.display_uuid_owned().as_deref());
        let stack_line = &self.config.settings.ui.stack_line;
        self.layout_manager.layout_engine.preview_insert_next_to(
            space,
            target,
            direction,
            window,
            screen.frame,
            &gaps,
            stack_line.thickness(),
            stack_line.horiz_placement,
            stack_line.vert_placement,
        )
    }

    fn hide_drop_region(&mut self) {
        self.drag_manager.drop_preview_cache = None;
        self.drag_manager.zone_candidate = None;
        if !std::mem::replace(&mut self.drag_manager.drop_overlay_shown, false) {
            return;
        }
        if let Some(tx) = &self.communication_manager.drop_overlay_tx {
            tx.send(crate::actor::drop_overlay::Event::Hide);
        }
    }

    fn maybe_swap_on_drag(&mut self, wid: WindowId, new_frame: CGRect) {
        let cursor = window_server::current_cursor_location().ok();
        self.evaluate_drop_target(wid, new_frame, cursor);
    }

    /// Decides what a drop of `wid` would do right now and shows it.
    ///
    /// The target is the tiled window under the pointer, as in yabai
    /// (`window_manager_find_window_at_point`). It used to be chosen by how
    /// much the dragged window's frame overlapped a candidate, while the drop
    /// zone inside that target was chosen by the pointer — and the two
    /// disagreed constantly. To reach the left edge of a full-width window the
    /// pointer, on the title bar, has to carry the dragged window most of the
    /// way off screen, at which point its overlap fell below the threshold and
    /// there was no target at all. Which zones were reachable depended on the
    /// window sizes, which is why the overlay seemed to come and go at random.
    /// The pointer alone decides both, and the overlap scorer is only a
    /// fallback for when the pointer cannot be read.
    fn evaluate_drop_target(&mut self, wid: WindowId, new_frame: CGRect, cursor: Option<CGPoint>) {
        if !self.is_in_drag() {
            trace!(?wid, "Skipping swap: not in drag (mouse up received)");
            return;
        }

        let server_id = {
            let Some(window) = self.state.windows.window(wid) else {
                return;
            };
            window.info.sys_id
        };

        // The pointer decides which display's tree is being dropped into,
        // exactly as it decides the drop itself at MouseUp (`pointer_space`
        // there): the dragged window's frame is only ever as far onto the
        // next display as the user got it, and choosing by the frame meant
        // no target — and no overlay — until the window had mostly crossed.
        let pointer_space = cursor
            .and_then(|cursor| self.screen_for_point(cursor))
            .and_then(|screen| screen.space)
            .filter(|space| self.is_space_active(*space));
        let Some(space) = pointer_space
            .or_else(|| self.current_drag_session().and_then(|session| session.settled_space))
            .or_else(|| self.best_space_for_window(&new_frame, server_id))
        else {
            return;
        };

        let origin_space_hint = self
            .current_drag_session()
            .and_then(|session| session.origin_space)
            .or_else(|| {
                self.drag_manager
                    .origin_frame()
                    .and_then(|frame| self.best_space_for_window(&frame, server_id))
            });

        // A drag that crosses onto another display keeps its session and its
        // origin membership — the window's tree is frozen for the drag — but
        // targets are looked for on the display under the pointer, so a drop
        // there can preview the split it would make. `membership_space` is
        // where the dragged window's own layout lives, the origin; for a drag
        // that has not left its display it equals `space`.
        let membership_space = origin_space_hint.unwrap_or(space);

        if !self.layout_manager.layout_engine.is_window_in_active_workspace(
            &self.state.windows,
            membership_space,
            wid,
        ) {
            return;
        }

        // A window that is not in the layout has no place to trade or split
        // from, so nothing is a target for it; an empty candidate list clears
        // any target it was given earlier.
        let candidates = if self.layout_manager.layout_engine.is_window_tiled(membership_space, wid)
        {
            self.collect_drag_swap_candidates(wid, space)
        } else {
            Vec::new()
        };

        let previous_pending = self.get_pending_drag_swap();
        let active_target = match cursor {
            Some(cursor) => {
                // The gaps between tiles belong to their neighbours: a
                // pointer crossing one used to drop the target for a few
                // reports and blink the overlay out and back.
                const GAP_GRACE: f64 = 24.0;
                let under_cursor = candidates
                    .iter()
                    .find(|(_, frame)| frame.contains(cursor))
                    .map(|(target, _)| *target)
                    .or_else(|| {
                        let (pending_wid, target) = previous_pending?;
                        if pending_wid != wid {
                            return None;
                        }
                        let (_, frame) = candidates.iter().find(|(other, _)| *other == target)?;
                        let near = CGRect::new(
                            CGPoint::new(frame.origin.x - GAP_GRACE, frame.origin.y - GAP_GRACE),
                            CGSize::new(
                                frame.size.width + 2.0 * GAP_GRACE,
                                frame.size.height + 2.0 * GAP_GRACE,
                            ),
                        );
                        near.contains(cursor).then_some(target)
                    });
                self.drag_manager.drag_swap_manager.set_target(wid, new_frame, under_cursor);
                under_cursor
            }
            None => {
                self.drag_manager.drag_swap_manager.on_frame_change(wid, new_frame, &candidates);
                self.drag_manager.drag_swap_manager.last_target()
            }
        };
        if let Some(target_wid) = active_target {
            if previous_pending != Some((wid, target_wid)) {
                trace!(
                    ?wid,
                    ?target_wid,
                    "Detected swap candidate; deferring until MouseUp"
                );
            }

            match cursor {
                Some(cursor) => self.preview_drop_region(wid, target_wid, cursor),
                None => self.hide_drop_region(),
            }

            if let Some(session) = self.take_active_drag_session() {
                self.drag_manager.drag_state =
                    DragState::PendingSwap { session, target: target_wid };
            } else {
                trace!(
                    ?wid,
                    ?target_wid,
                    "Skipping pending swap; no active drag session"
                );
                self.drag_manager.drag_state = DragState::Inactive;
                self.drag_manager.skip_layout_for_window = None;
                return;
            }

            self.drag_manager.skip_layout_for_window = Some(wid);
            return;
        }

        self.hide_drop_region();

        if let Some((pending_wid, pending_target)) = previous_pending
            && pending_wid == wid
        {
            trace!(
                ?wid,
                ?pending_target,
                "Clearing pending drag swap; overlap ended before MouseUp"
            );
            if let Some(session) = self.take_active_drag_session() {
                self.drag_manager.drag_state = DragState::Active { session };
            } else {
                self.drag_manager.drag_state = DragState::Inactive;
            }
        }

        if self.drag_manager.skip_layout_for_window == Some(wid) {
            self.drag_manager.skip_layout_for_window = None;
        }
        // wait for mouse::up before doing *anything*
    }

    pub(crate) fn window_id_under_cursor(&self) -> Option<WindowId> {
        self.tracked_window_under_cursor().map(|(_, wid)| wid)
    }

    fn window_server_id_under_cursor(&self) -> Option<WindowServerId> {
        window_server::window_under_cursor()
    }

    fn tracked_window_under_cursor(&self) -> Option<(WindowServerId, WindowId)> {
        let wsid = self.window_server_id_under_cursor()?;
        let wid = self.state.windows.tracked_window_id(wsid)?;
        Some((wsid, wid))
    }

    fn activation_from_unmanageable_window(&self, pid: pid_t) -> Option<WindowServerId> {
        let (wsid, wid) = self.tracked_window_under_cursor()?;
        let window = self.state.windows.window(wid)?;
        (wid.pid == pid && !window.is_admitted()).then_some(wsid)
    }

    fn focus_untracked_window_under_cursor(&mut self) -> bool {
        let Some(wsid) = self.window_server_id_under_cursor() else {
            return false;
        };
        if self.state.windows.tracked_window_id(wsid).is_some() {
            return false;
        }

        let window_info = self
            .state
            .windows
            .get_window_server_info(wsid)
            .or_else(|| window_server::get_window(wsid));

        let Some(info) = window_info else { return false };
        // The untracked-window fallback exists for ordinary application
        // windows that are intentionally outside Rift's model. Desktop,
        // menu-bar, Dock, and other system surfaces use nonzero layers and
        // must never be made key merely because the pointer crossed them.
        if info.layer != 0 {
            trace!(
                ?wsid,
                layer = info.layer,
                "Skipping non-application surface under cursor"
            );
            return false;
        }
        window_server::make_key_window(info.pid, wsid).is_ok()
    }

    fn focus_desktop_if_active_workspace_empty(&mut self, space: SpaceId) -> bool {
        if !self.is_space_active(space)
            || !self
                .layout_manager
                .layout_engine
                .windows_in_active_workspace(&self.state.windows, space)
                .is_empty()
        {
            return false;
        }
        let Some(screen) = self.space_state.screen_by_space(space) else {
            return false;
        };
        if !window_server::focus_desktop_window(screen) {
            return false;
        }

        self.layout_manager.layout_engine.commit_workspace_focus(
            &mut self.state.windows,
            space,
            None,
        );
        true
    }

    fn last_focused_window_in_space(&self, space: SpaceId) -> Option<WindowId> {
        let active_workspace = self.layout_manager.layout_engine.active_workspace(space)?;
        let wid = self
            .layout_manager
            .layout_engine
            .virtual_workspace_manager()
            .last_focused_window(space, active_workspace)?;
        let window = self.state.windows.window(wid)?;

        if self.best_space_for_window_id(wid)? != space {
            return None;
        }
        if window
            .info
            .sys_id
            .is_some_and(|wsid| !self.state.windows.is_window_visible(wsid))
        {
            return None;
        }
        Some(wid)
    }

    fn visible_focus_candidate_in_active_workspace(
        &self,
        space: SpaceId,
        preferred: Option<WindowId>,
    ) -> Option<WindowId> {
        let is_visible_in_space = |wid: WindowId| {
            let Some(window) = self.state.windows.window(wid) else {
                return false;
            };
            let Some(wsid) = window.info.sys_id else {
                return false;
            };
            self.state.windows.is_window_visible(wsid)
                && self.best_space_for_window_id(wid) == Some(space)
                && self.layout_manager.layout_engine.is_window_in_active_workspace(
                    &self.state.windows,
                    space,
                    wid,
                )
        };

        if let Some(wid) = preferred.filter(|wid| is_visible_in_space(*wid)) {
            return Some(wid);
        }

        if let Some(wid) =
            self.last_focused_window_in_space(space).filter(|wid| is_visible_in_space(*wid))
        {
            return Some(wid);
        }

        self.layout_manager
            .layout_engine
            .windows_in_active_workspace(&self.state.windows, space)
            .into_iter()
            .find(|wid| is_visible_in_space(*wid))
    }

    fn request_refocus_if_hidden(&mut self, space: SpaceId, window_id: WindowId) {
        if self.window_in_non_active_workspace(space, window_id) {
            self.refocus_manager.refocus_state = RefocusState::Pending(space);
        }
    }

    fn window_in_non_active_workspace(&self, space: SpaceId, window_id: WindowId) -> bool {
        let Some(active_workspace) = self.layout_manager.layout_engine.active_workspace(space)
        else {
            return false;
        };
        self.layout_manager
            .layout_engine
            .virtual_workspace_manager()
            .workspace_for_window(&self.state.windows, space, window_id)
            .is_some_and(|window_workspace| window_workspace != active_workspace)
    }

    fn prepare_refocus_after_layout_event(&mut self, event: &LayoutEvent) {
        match event {
            LayoutEvent::WindowAdded(space, wid) => {
                self.request_refocus_if_hidden(*space, *wid);
            }
            LayoutEvent::WindowObserved(space, window) => {
                if self.window_in_non_active_workspace(*space, window.info.0) {
                    self.refocus_manager.refocus_state = RefocusState::Pending(*space);
                }
            }
            _ => {}
        }
    }

    #[instrument(skip(self))]
    fn clear_menu_state_for_pid(&mut self, pid: pid_t) {
        if matches!(self.menu_manager.menu_state, MenuState::Open(owner) if owner == pid) {
            debug!(pid, "Clearing menu-open state for deactivated app");
            self.menu_manager.menu_state = MenuState::Closed;
            self.update_focus_follows_mouse_state();
        }
    }

    fn clear_menu_state_for_non_owner(&mut self, pid: pid_t) {
        if matches!(self.menu_manager.menu_state, MenuState::Open(owner) if owner != pid) {
            debug!(pid, "Clearing stale menu-open state after app focus changed");
            self.menu_manager.menu_state = MenuState::Closed;
            self.update_focus_follows_mouse_state();
        }
    }

    fn set_focus_follows_mouse_enabled(&self, enabled: bool) {
        if let Some(event_tap_tx) = self.communication_manager.event_tap_tx.as_ref() {
            event_tap_tx.send(event_tap::Request::SetFocusFollowsMouseEnabled(enabled));
        }
    }

    fn update_focus_follows_mouse_state(&mut self) {
        let should_enable = self.config.settings.focus_follows_mouse
            && matches!(self.menu_manager.menu_state, MenuState::Closed)
            && !self.is_mission_control_active();
        self.set_focus_follows_mouse_enabled(should_enable);
    }

    fn update_event_tap_layout_mode(&mut self) {
        let Some(event_tap_tx) = self.communication_manager.event_tap_tx.as_ref() else {
            return;
        };

        let last_modes = &self.notification_manager.last_layout_modes_by_space;
        let mut modes: Vec<(SpaceId, crate::common::config::LayoutMode)> =
            Vec::with_capacity(self.space_state.screens.len());
        let mut changed = false;

        for screen in &self.space_state.screens {
            let Some(space) = screen.space else {
                continue;
            };

            // Keep first occurrence only if multiple screens briefly report the same space.
            if modes.iter().any(|(existing, _)| *existing == space) {
                continue;
            }

            let mode = self.layout_manager.layout_engine.active_layout_mode_at(space);
            if last_modes.get(&space).copied() != Some(mode) {
                changed = true;
            }
            modes.push((space, mode));
        }

        if modes.is_empty() || (!changed && modes.len() == last_modes.len()) {
            return;
        }

        let modes_by_space = modes.iter().copied().collect();
        self.notification_manager.last_layout_modes_by_space = modes_by_space;
        if let Some(gesture_tap_tx) = self.communication_manager.gesture_tap_tx.as_ref() {
            gesture_tap_tx.send(gesture_tap::GestureRequest::LayoutModesChanged(modes.clone()));
        }
        event_tap_tx.send(crate::actor::event_tap::Request::LayoutModesChanged(modes));
    }

    fn set_mission_control_active(&mut self, active: bool) {
        let new_state = if active {
            MissionControlState::Active
        } else {
            MissionControlState::Inactive
        };
        if self.is_mission_control_active() == active {
            return;
        }
        self.mission_control_manager.mission_control_state = new_state;
        self.update_focus_follows_mouse_state();
    }

    fn refresh_windows_after_mission_control(&mut self) {
        debug!("Refreshing window state after Mission Control");
        // AX inventories are space-filtered, so only request one when a user Space
        // is active. The inventory coordinator rejects replies from older topology.
        if !self.has_user_space_context() {
            return;
        }
        let active_windows = self.authoritative_active_space_windows();
        self.refresh_windows_after_mission_control_with_active_windows(active_windows);
    }

    fn refresh_windows_after_mission_control_with_active_windows(
        &mut self,
        active_windows: Vec<(WindowServerId, Option<SpaceId>)>,
    ) {
        if self.refreshes_blocked() {
            self.defer_window_inventory_refresh();
            return;
        }

        // Mission Control can move windows between native spaces without emitting a
        // matching destroy/appear pair for the origin space. Reconcile the active
        // spaces from the same space-aware WS-id list used everywhere else so we do
        // not depend on the global CG on-screen window list during recovery.
        self.reconcile_authoritative_active_window_snapshot(active_windows, false);
        self.request_window_inventories();
        self.update_layout_or_warn(false, false, None);
        self.maybe_send_menu_update();
    }

    fn has_user_space_context(&self) -> bool {
        self.raw_command_space().is_some_and(|space| !self.is_fullscreen_space(space))
    }

    fn request_close_window(&mut self, pid: pid_t, window_server_id: Option<WindowServerId>) {
        if let Some(app) = self.app_manager.apps.get(&pid) {
            if let Err(err) = app.handle.send(Request::CloseWindow(window_server_id)) {
                warn!(
                    pid,
                    ?window_server_id,
                    "Failed to send close window request: {}",
                    err
                );
            }
        }
    }

    pub(crate) fn main_window(&self) -> Option<WindowId> { self.main_window_tracker.main_window() }

    fn main_window_space(&self) -> Option<SpaceId> {
        // TODO: Optimize this with a cache or something.
        let wid = self.main_window()?;
        self.best_space_for_window_id(wid)
    }

    /// Window discovery is scoped to one application. It may restore that
    /// application's current focus after its windows have been inserted into
    /// the layout, but it must never replay another application's global main
    /// window. Requiring the command space also prevents a refresh racing an
    /// active-display change from restoring focus on the display being left.
    fn focused_window_for_discovery(&self, pid: pid_t) -> Option<(SpaceId, WindowId)> {
        let window = self.main_window().filter(|window| window.pid == pid)?;
        let space = self.main_window_space()?;
        (self.workspace_command_space() == Some(space)).then_some((space, window))
    }

    fn raw_command_space(&self) -> Option<SpaceId> { self.space_state.command_space }

    fn active_display_space(&self) -> Option<SpaceId> {
        self.raw_command_space()
            .filter(|space| {
                self.space_state.active_spaces.contains(space)
                    && self.space_state.screens.iter().any(|screen| screen.space == Some(*space))
            })
            .or_else(|| {
                self.space_state
                    .screens
                    .iter()
                    .filter_map(|screen| screen.space)
                    .find(|space| self.space_state.active_spaces.contains(space))
            })
    }

    fn workspace_command_space(&self) -> Option<SpaceId> {
        self.active_display_space().filter(|space| self.is_space_active(*space))
    }

    fn command_context_space(&self) -> Option<SpaceId> {
        self.workspace_command_space().or_else(|| {
            self.layout_manager
                .layout_engine
                .focused_window()
                .and_then(|wid| {
                    self.assigned_space_for_window_id(wid)
                        .or_else(|| self.best_space_for_window_id(wid))
                })
                .filter(|space| self.is_space_active(*space))
                .or_else(|| self.main_window_space().filter(|space| self.is_space_active(*space)))
        })
    }

    fn screen_for_point(&self, point: CGPoint) -> Option<&ScreenInfo> {
        self.space_state.screens.iter().find(|screen| screen.frame.contains(point))
    }

    fn current_screen_center(&self) -> Option<CGPoint> {
        if let Some(space) = self.raw_command_space() {
            if let Some(screen) = self.space_state.screen_by_space(space) {
                return Some(screen.frame.mid());
            }
        }

        self.space_state.screens.first().map(|screen| screen.frame.mid())
    }

    fn screen_for_direction_from_point(
        &self,
        origin: CGPoint,
        direction: Direction,
    ) -> Option<&ScreenInfo> {
        fn interval_gap(a_min: f64, a_max: f64, b_min: f64, b_max: f64) -> f64 {
            if a_max < b_min {
                b_min - a_max
            } else if b_max < a_min {
                a_min - b_max
            } else {
                0.0
            }
        }

        let mut best: Option<(f64, f64, &ScreenInfo)> = None;

        for screen in &self.space_state.screens {
            let frame = screen.frame;

            if frame.contains(origin) {
                continue;
            }

            let min = frame.min();
            let max = frame.max();

            let (primary_dist, orth_gap) = match direction {
                Direction::Left => {
                    if max.x > origin.x {
                        continue;
                    }
                    (origin.x - max.x, interval_gap(min.y, max.y, origin.y, origin.y))
                }
                Direction::Right => {
                    if min.x < origin.x {
                        continue;
                    }
                    (min.x - origin.x, interval_gap(min.y, max.y, origin.y, origin.y))
                }
                Direction::Up => {
                    // Smaller y means visually "up".
                    if max.y > origin.y {
                        continue;
                    }
                    (origin.y - max.y, interval_gap(min.x, max.x, origin.x, origin.x))
                }
                Direction::Down => {
                    if min.y < origin.y {
                        continue;
                    }
                    (min.y - origin.y, interval_gap(min.x, max.x, origin.x, origin.x))
                }
            };

            let should_replace = best.as_ref().map_or(true, |(best_primary, best_orth, _)| {
                primary_dist < *best_primary
                    || (primary_dist == *best_primary && orth_gap < *best_orth)
            });

            if should_replace {
                best = Some((primary_dist, orth_gap, screen));
            }
        }

        best.map(|(_, _, screen)| screen)
    }

    fn screen_for_selector(
        &self,
        selector: &DisplaySelector,
        origin_override: Option<CGPoint>,
    ) -> Option<&ScreenInfo> {
        match selector {
            DisplaySelector::Direction(direction) => {
                let origin = origin_override.or_else(|| self.current_screen_center())?;
                self.screen_for_direction_from_point(origin, *direction)
            }
            DisplaySelector::Index(index) => self.screens_in_physical_order().get(*index).copied(),
            DisplaySelector::Uuid(uuid) => {
                self.space_state.screens.iter().find(|screen| screen.display_uuid == *uuid)
            }
        }
    }

    fn screens_in_physical_order(&self) -> Vec<&ScreenInfo> {
        let mut screens: Vec<&ScreenInfo> = self.space_state.screens.iter().collect();
        screens.sort_by(|a, b| {
            let x_order = a.frame.origin.x.total_cmp(&b.frame.origin.x);
            if x_order == std::cmp::Ordering::Equal {
                a.frame.origin.y.total_cmp(&b.frame.origin.y)
            } else {
                x_order
            }
        });
        screens
    }

    fn store_current_floating_positions(&mut self, space: SpaceId) {
        let floating_windows_in_workspace = self
            .layout_manager
            .layout_engine
            .windows_in_active_workspace(&self.state.windows, space)
            .into_iter()
            .filter(|&wid| self.layout_manager.layout_engine.is_window_floating(wid))
            .filter_map(|wid| {
                self.state
                    .windows
                    .window(wid)
                    .map(|window_state| (wid, window_state.frame_monotonic))
            })
            .collect::<Vec<_>>();

        if !floating_windows_in_workspace.is_empty() {
            self.layout_manager
                .layout_engine
                .store_floating_window_positions(space, &floating_windows_in_workspace);
        }
    }

    pub(crate) fn update_layout_or_warn(
        &mut self,
        is_resize: bool,
        is_workspace_switch: bool,
        space_scope: Option<SpaceId>,
    ) -> bool {
        self.update_layout_or_warn_with(
            is_resize,
            is_workspace_switch,
            space_scope,
            "Layout update failed",
        )
    }

    pub(crate) fn update_layout_or_warn_with(
        &mut self,
        is_resize: bool,
        is_workspace_switch: bool,
        space_scope: Option<SpaceId>,
        context: &'static str,
    ) -> bool {
        LayoutManager::update_layout(self, is_resize, is_workspace_switch, space_scope)
            .unwrap_or_else(|e| {
                warn!(error = ?e, "{}", context);
                false
            })
    }
}
