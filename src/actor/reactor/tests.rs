use objc2_core_foundation::{CGPoint, CGSize};
use test_log::test;

use super::testing::*;
use super::*;
use crate::actor::app::{AppThreadHandle, Request, pid_t};
use crate::actor::wm_controller::WmEvent;
use crate::common::config::{LayoutMode, OuterGaps, WorkspaceSelector};
use crate::layout_engine::{Direction, LayoutCommand, LayoutEvent};
use crate::model::window_store::NativeFullscreenTransition;
use crate::sys::app::{AppInfo, WindowInfo};
use crate::sys::geometry::SameAs;
use crate::sys::window_server::WindowServerId;

#[test]
fn layout_query_exposes_active_and_inactive_workspace_container_trees() {
    let mut reactor = test_reactor();
    let space = SpaceId::new(1);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.send_layout_event(LayoutEvent::SpaceExposed(space, screen.size));
    reactor.send_layout_event(LayoutEvent::WindowAdded(space, WindowId::new(42, 1)));
    reactor.send_layout_event(LayoutEvent::WindowAdded(space, WindowId::new(42, 2)));

    let state = reactor.query_layout_state(None, None).expect("layout state");
    assert_eq!(state.space_id, space.get());
    assert!(state.is_active_workspace);
    assert_eq!(state.selected_window, state.container_tree.children[1].window_id);
    assert_eq!(
        state.container_tree.node_type,
        rift_protocol::ContainerNodeType::Container
    );
    assert_eq!(state.container_tree.children.len(), 2);
    assert!(state.container_tree.frame.size.width > 0.0);
    assert!(state.container_tree.frame.size.height > 0.0);
    assert!(state.container_tree.children.iter().all(|node| node.frame.size.width > 0.0));
    assert!(state.container_tree.children.iter().all(|node| node.frame.size.height > 0.0));
    assert_eq!(
        state
            .container_tree
            .children
            .iter()
            .filter(|node| node.window_id.is_some())
            .count(),
        2
    );

    let original_workspace = state.workspace_id;
    reactor.handle_test_layout_command(LayoutCommand::NextWorkspace(Some(false)));
    let inactive = reactor
        .query_layout_state(Some(space.get()), Some(original_workspace))
        .expect("inactive workspace layout state");
    assert!(!inactive.is_active_workspace);
    assert_eq!(inactive.workspace_id, original_workspace);
    assert!(reactor.query_layout_state(Some(space.get()), Some(usize::MAX)).is_none());
}

#[test]
fn config_reload_propagates_non_keybinding_changes_to_wm_controller() {
    let mut reactor = test_reactor();
    let (wm_tx, mut wm_rx) = actor::channel();
    reactor.communication_manager.wm_sender = Some(wm_tx);

    let mut updated = reactor.config.clone();
    updated.settings.focus_follows_mouse = !updated.settings.focus_follows_mouse;
    updated.settings.mouse_follows_focus = !updated.settings.mouse_follows_focus;
    updated.settings.mouse_hides_on_focus = !updated.settings.mouse_hides_on_focus;

    reactor.handle_event(Event::ConfigUpdated(updated.clone()));

    let (_, event) = wm_rx.try_recv().expect("config update should reach wm controller");
    let WmEvent::ConfigUpdated(actual) = event else {
        panic!("expected config update, got {event:?}");
    };
    assert_eq!(
        actual.settings.focus_follows_mouse,
        updated.settings.focus_follows_mouse
    );
    assert_eq!(
        actual.settings.mouse_follows_focus,
        updated.settings.mouse_follows_focus
    );
    assert_eq!(
        actual.settings.mouse_hides_on_focus,
        updated.settings.mouse_hides_on_focus
    );
}

#[test]
fn it_ignores_stale_resize_events() {
    let (mut apps, mut reactor) = test_context();
    reactor.handle_event(space_state_event(
        vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
        vec![Some(SpaceId::new(1))],
    ));

    reactor.handle_events(apps.make_app(1, make_windows(2)));
    let requests = apps.requests();
    assert!(!requests.is_empty());
    let events_1 = apps.simulate_events_for_requests(requests);

    reactor.handle_events(apps.make_app(2, make_windows(2)));
    assert!(!apps.requests().is_empty());

    for event in dbg!(events_1) {
        reactor.handle_event(event);
    }
    let requests = apps.requests();
    assert!(
        requests.is_empty(),
        "got requests when there should have been none: {requests:?}"
    );
}

#[test]
fn inventory_from_an_older_space_topology_is_discarded_and_retried() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let old_space = SpaceId::new(1);
    let new_space = SpaceId::new(59);
    let pid = 91;
    let wid = WindowId::new(pid, 1);
    let (app_tx, mut app_rx) = actor::channel();

    reactor.handle_event(space_state_event(vec![screen], vec![Some(old_space)]));
    reactor.app_manager.apps.insert(pid, AppState {
        info: AppInfo {
            bundle_id: Some("com.test.stale-inventory".into()),
            localized_name: Some("Stale Inventory".into()),
        },
        handle: AppThreadHandle::new_for_test(app_tx),
    });

    reactor.request_window_inventory(pid);
    let (_, Request::RefreshWindowInventory(stale_token)) =
        app_rx.try_recv().expect("the initial inventory should be requested")
    else {
        panic!("expected a window inventory request");
    };

    reactor.handle_event(space_state_event(vec![screen], vec![Some(new_space)]));
    reactor.handle_event(Event::WindowsDiscovered {
        pid,
        token: stale_token,
        successful: true,
        new: vec![(wid, make_window_info(screen, None, "Late Window", None))],
        known_visible: vec![wid],
    });

    assert!(
        !reactor.state.windows.contains_window(wid),
        "a reply requested for the old Space topology must not mutate window state"
    );
    let (_, Request::RefreshWindowInventory(fresh_token)) = app_rx
        .try_recv()
        .expect("discarding a stale reply should immediately request a fresh inventory")
    else {
        panic!("expected a replacement window inventory request");
    };
    assert_ne!(fresh_token.request_id, stale_token.request_id);
    assert_ne!(fresh_token.topology_revision, stale_token.topology_revision);
}

#[test]
fn it_sends_writes_when_stale_read_state_looks_same_as_written_state() {
    let (mut apps, mut reactor) = test_context();
    reactor.handle_event(space_state_event(
        vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
        vec![Some(SpaceId::new(1))],
    ));

    reactor.handle_events(apps.make_app(1, make_windows(2)));
    let events_1 = apps.simulate_events();
    let state_1 = apps.windows.clone();
    assert!(!state_1.is_empty());

    for event in events_1 {
        reactor.handle_event(event);
    }
    assert!(apps.requests().is_empty());

    reactor.handle_events(apps.make_app(2, make_windows(1)));
    let _events_2 = apps.simulate_events();

    reactor.handle_event(Event::WindowDestroyed(WindowId::new(2, 1)));
    let _events_3 = apps.simulate_events();
    let state_3 = apps.windows;

    // These should be the same, because we should have resized the first
    // two windows both at the beginning, and at the end when the third
    // window was destroyed.
    for (wid, state) in dbg!(state_1) {
        assert!(state_3.contains_key(&wid), "{wid:?} not in {state_3:#?}");
        assert_eq!(state.frame, state_3[&wid].frame);
    }
}

#[test]
fn it_manages_windows_on_enabled_spaces() {
    let (mut apps, mut reactor) = test_context();
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(SpaceId::new(1))]));

    reactor.handle_events(apps.make_app(1, make_windows(1)));

    let _events = apps.simulate_events();
    assert_eq!(
        full_screen,
        apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
    );
}

#[test]
fn it_clears_screen_state_when_no_displays_are_reported() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(SpaceId::new(1))]));
    assert_eq!(1, reactor.space_state.screens.len());

    reactor.handle_event(space_state_event(vec![], vec![]));
    assert!(reactor.space_state.screens.is_empty());
    assert_eq!(reactor.raw_command_space(), None);
    assert_eq!(reactor.space_state.menu_bar_space, None);
    assert!(reactor.space_state.display_space_ids.is_empty());

    reactor.handle_event(space_state_event(vec![], vec![]));
    assert!(reactor.space_state.screens.is_empty());
    assert_eq!(reactor.raw_command_space(), None);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(SpaceId::new(1))]));
    assert_eq!(1, reactor.space_state.screens.len());
}

#[test]
fn workspace_command_space_follows_forwarded_space_snapshot() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let old_space = SpaceId::new(1);
    let new_space = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(old_space)]));
    make_active_app_with_count(&mut apps, &mut reactor, 1, 1, Some(WindowId::new(1, 1)));

    assert_eq!(reactor.workspace_command_space(), Some(old_space));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(new_space)]));

    assert_eq!(
        reactor.workspace_command_space(),
        Some(new_space),
        "workspace commands must follow the forwarded active screen space, not stale main-window space",
    );
}

#[test]
fn forwarded_active_spaces_filter_active_workspace_context() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let inactive_space = SpaceId::new(1);
    let active_space = SpaceId::new(2);

    reactor.handle_event(space_state_event_with(
        vec![left, right],
        vec![Some(inactive_space), Some(active_space)],
        |state| {
            state.active_spaces = [active_space].into_iter().collect();
            state.menu_bar_space = Some(active_space);
            state.command_space = Some(active_space);
        },
    ));

    assert!(!reactor.is_space_active(inactive_space));
    assert!(reactor.is_space_active(active_space));
    assert_eq!(
        reactor.space_state.active_spaces,
        [active_space].into_iter().collect(),
        "the stored forwarded state should reflect the authority's active-space set",
    );
}

#[test]
fn forwarded_space_snapshot_respects_default_disable_policy() {
    let mut reactor = test_reactor();
    reactor.config.settings.default_disable = true;

    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

    assert!(
        !reactor.is_space_active(space),
        "forwarded raw active spaces must still be filtered by default_disable policy"
    );
}

#[test]
fn forwarded_space_snapshot_respects_one_space_policy() {
    let mut reactor = test_reactor();
    reactor.one_space = true;

    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(space1),
        Some(space2),
    ]));

    assert!(reactor.is_space_active(space1));
    assert!(
        !reactor.is_space_active(space2),
        "forwarded raw active spaces must not bypass one_space filtering"
    );
}

#[test]
fn forwarded_space_snapshot_respects_toggled_space_activation_policy() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    assert!(reactor.is_space_active(space));

    reactor.handle_event(Event::Command(Command::Reactor(
        ReactorCommand::ToggleSpaceActivated,
    )));
    assert!(!reactor.is_space_active(space));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

    assert!(
        !reactor.is_space_active(space),
        "forwarded raw active spaces must not re-enable a space disabled by ToggleSpaceActivated"
    );
}

#[test]
fn layout_commands_follow_active_display_space_across_active_displays() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let right = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let left_space = SpaceId::new(1);
    let right_space = SpaceId::new(2);
    let source = WindowId::new(1, 1);
    let target_a = WindowId::new(1, 2);
    let target_b = WindowId::new(1, 3);
    let windows = [
        (source, WindowServerId::new(101), left_space, left),
        (target_a, WindowServerId::new(102), right_space, right),
        (target_b, WindowServerId::new(103), right_space, right),
    ];

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space),
        Some(right_space),
    ]));

    reactor.add_test_app(1);

    reactor.send_layout_event(LayoutEvent::SpaceExposed(left_space, left.size));
    reactor.send_layout_event(LayoutEvent::SpaceExposed(right_space, right.size));

    let left_workspace = reactor.test_workspace(left_space, 0);
    let right_workspace = reactor.test_workspace(right_space, 0);

    for (wid, wsid, space, frame) in windows {
        reactor.add_test_window(wid, wsid, Some(space), frame);
        let workspace = if space == left_space {
            left_workspace
        } else {
            right_workspace
        };
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
    }

    reactor.send_layout_event(LayoutEvent::WindowFocused(right_space, target_a));

    assert_eq!(reactor.workspace_command_space(), Some(left_space));
    assert_eq!(reactor.command_context_space(), Some(left_space));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(target_a)
    );

    // NextWindow steps through a stack and does nothing outside one, so the
    // space this command is meant to land on has to be stacked for it to say
    // anything about routing.
    reactor.handle_test_workspace_command(left_space, &LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode: rift_protocol::LayoutMode::Stack,
    });
    reactor.handle_test_layout_command(LayoutCommand::NextWindow);

    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(source),
        "non-workspace layout commands should follow the active display space"
    );
}

#[test]
fn workspace_commands_follow_active_display_space_across_active_displays() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let right = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let left_space = SpaceId::new(1);
    let right_space = SpaceId::new(2);
    let source = WindowId::new(1, 1);
    let target = WindowId::new(1, 2);
    let windows = [
        (source, WindowServerId::new(201), left_space, left),
        (target, WindowServerId::new(202), right_space, right),
    ];

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space),
        Some(right_space),
    ]));

    reactor.add_test_app(1);

    reactor.send_layout_event(LayoutEvent::SpaceExposed(left_space, left.size));
    reactor.send_layout_event(LayoutEvent::SpaceExposed(right_space, right.size));

    let left_workspaces = reactor.test_workspace_ids(left_space);
    let right_workspaces = reactor.test_workspace_ids(right_space);
    let left_workspace = left_workspaces[0];
    let next_left_workspace = left_workspaces[1];
    let right_workspace = right_workspaces[0];

    for (wid, wsid, space, frame) in windows {
        reactor.add_test_window(wid, wsid, Some(space), frame);
        let workspace = if space == left_space {
            left_workspace
        } else {
            right_workspace
        };
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
    }

    reactor.send_layout_event(LayoutEvent::WindowFocused(right_space, target));

    assert_eq!(reactor.workspace_command_space(), Some(left_space));
    assert_eq!(reactor.command_context_space(), Some(left_space));
    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace(right_space),
        Some(right_workspace)
    );

    reactor.handle_test_layout_command(LayoutCommand::NextWorkspace(None));

    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace(left_space),
        Some(next_left_workspace),
        "workspace commands should follow the active display space"
    );
    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace(right_space),
        Some(right_workspace),
        "workspace commands should not switch the focused window's display when it is not active"
    );
}

#[test]
fn workspace_switch_arrange_is_scoped_to_its_command_space() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let right = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let left_space = SpaceId::new(1);
    let right_space = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space),
        Some(right_space),
    ]));

    let switch = reactor.dispatch_test_layout_command(LayoutCommand::NextWorkspace(None));
    assert_eq!(switch.arrange.space_scope, Some(left_space));

    let ordinary = reactor.dispatch_test_layout_command(LayoutCommand::NextWindow);
    assert_eq!(ordinary.arrange.space_scope, None);
}

#[test]
fn no_op_workspace_switch_does_not_request_arrangement() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let space = SpaceId::new(1);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

    let already_active = reactor.dispatch_test_layout_command(LayoutCommand::SwitchToWorkspace(0));
    assert!(!already_active.arrange.requested);
    assert!(already_active.layout_responses.is_empty());

    let missing =
        reactor.dispatch_test_layout_command(LayoutCommand::SwitchToWorkspace(usize::MAX));
    assert!(!missing.arrange.requested);
    assert!(missing.layout_responses.is_empty());
}

#[test]
fn only_move_node_requests_post_arrange_mouse_warp() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let focused = WindowId::new(1, 2);

    reactor.config.settings.mouse_follows_focus = true;
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(2), Some(focused));
    reactor.handle_test_layout_command(LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode: LayoutMode::Scrolling,
    });
    apps.simulate_until_quiet(&mut reactor);
    assert_eq!(reactor.main_window(), Some(focused));

    let move_node = reactor.dispatch_test_layout_command(LayoutCommand::MoveNode(Direction::Left));
    assert_eq!(move_node.post_arrange_mouse_warp, Some(focused));

    let move_focus =
        reactor.dispatch_test_layout_command(LayoutCommand::MoveFocus(Direction::Left));
    assert_eq!(move_focus.post_arrange_mouse_warp, None);
}

#[test]
fn command_space_only_snapshot_does_not_trigger_full_space_reconcile() {
    let (mut apps, mut reactor) = test_context();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event_with(
        vec![left, right],
        vec![Some(space1), Some(space2)],
        |state| state.has_seen_display_set = true,
    ));

    apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
    assert!(apps.requests().is_empty());

    reactor.handle_event(space_state_event_with(
        vec![left, right],
        vec![Some(space1), Some(space2)],
        |state| {
            state.has_seen_display_set = true;
            state.menu_bar_space = Some(space2);
            state.command_space = Some(space2);
        },
    ));

    assert_eq!(reactor.workspace_command_space(), Some(space2));
    assert!(
        apps.requests().is_empty(),
        "changing only command_space should not trigger visible-window refresh or space reconciliation"
    );
}

#[test]
fn active_display_update_only_changes_command_context() {
    let (mut apps, mut reactor) = test_context();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let left_space = SpaceId::new(1);
    let right_space = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space),
        Some(right_space),
    ]));
    apps.make_app_and_settle(&mut reactor, 1, make_windows(1));
    assert!(apps.requests().is_empty());

    reactor.handle_event(Event::ActiveDisplayChanged {
        menu_bar_space: Some(right_space),
        command_space: Some(right_space),
    });

    assert_eq!(reactor.workspace_command_space(), Some(right_space));
    assert_eq!(reactor.space_state.menu_bar_space, Some(right_space));
    assert!(
        apps.requests().is_empty(),
        "active-display updates must not trigger window discovery"
    );
}

#[test]
fn passive_command_space_change_does_not_override_clicked_window_focus() {
    let (mut apps, mut reactor) = test_context();
    let (raise_manager_tx, mut raise_manager_rx) = actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_manager_tx;

    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let left_space = SpaceId::new(1);
    let right_space = SpaceId::new(2);
    reactor.handle_event(space_state_event_with(
        vec![left, right],
        vec![Some(left_space), Some(right_space)],
        |state| state.has_seen_display_set = true,
    ));

    let mut windows = make_windows(2);
    windows[1].frame.origin = CGPoint::new(1100., 100.);
    reactor.handle_event(Event::ApplicationGloballyActivated(1));
    reactor.handle_events(apps.make_app_with_opts(
        1,
        windows,
        Some(WindowId::new(1, 1)),
        true,
        true,
    ));
    apps.simulate_until_quiet(&mut reactor);

    let old_focus = WindowId::new(1, 1);
    let destination_focus = WindowId::new(1, 2);
    reactor.send_layout_event(LayoutEvent::WindowFocused(right_space, destination_focus));
    reactor.send_layout_event(LayoutEvent::WindowFocused(left_space, old_focus));
    while raise_manager_rx.try_recv().is_ok() {}

    reactor.handle_event(space_state_event_with(
        vec![left, right],
        vec![Some(left_space), Some(right_space)],
        |state| {
            state.has_seen_display_set = true;
            state.menu_bar_space = Some(right_space);
            state.command_space = Some(right_space);
        },
    ));

    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(old_focus),
        "a passive display snapshot must leave focus ownership to the AX click event"
    );
    assert!(
        raise_manager_rx.try_recv().is_err(),
        "a passive active-display change must not raise the workspace's stale selection"
    );

    reactor.handle_event(Event::ApplicationMainWindowChanged(
        1,
        Some(destination_focus),
        Quiet::No,
    ));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(destination_focus),
        "the subsequent AX focus event should select the window that activated the display"
    );
}

#[test]
fn discovery_does_not_replay_another_apps_global_main_window() {
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

    reactor.handle_event(Event::ApplicationGloballyActivated(1));
    reactor.handle_events(apps.make_app_with_opts(
        1,
        make_windows(1),
        Some(WindowId::new(1, 1)),
        true,
        true,
    ));
    reactor.handle_events(apps.make_app_with_opts(2, make_windows(1), None, false, true));
    apps.simulate_until_quiet(&mut reactor);

    let app_two_window = WindowId::new(2, 1);
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, app_two_window));
    let info = reactor
        .state
        .windows
        .window(app_two_window)
        .expect("app two window should be tracked")
        .info
        .clone();

    reactor.discover_test_windows(2, vec![(app_two_window, info)], vec![app_two_window]);

    assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(app_two_window),
        "app-scoped discovery must not replay another app's global main window"
    );
}

#[test]
fn forwarded_space_state_updates_fullscreen_spaces() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());

    reactor.handle_event(space_state_event_with(
        vec![screen],
        vec![Some(user_space)],
        |state| {
            state.fullscreen_spaces.insert(fullscreen_space);
        },
    ));

    assert!(reactor.space_state.fullscreen_spaces.contains(&fullscreen_space));
}

#[test]
fn queries_prefer_authoritative_active_space_over_stale_command_space() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space1)]));
    reactor.handle_test_workspace_command(space1, &LayoutCommand::SwitchToWorkspace(0));
    reactor.handle_test_workspace_command(space2, &LayoutCommand::SwitchToWorkspace(1));

    reactor.handle_event(space_state_event_with(
        vec![screen],
        vec![Some(space2)],
        |state| state.command_space = Some(space1),
    ));

    assert_eq!(
        reactor.query_active_workspace(None),
        reactor.layout_manager.layout_engine.active_workspace(space2),
        "default queries must follow authoritative active space state, not stale command_space"
    );
}

#[test]
fn menu_bar_space_prefers_active_menu_bar_display_space() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(space1),
        Some(space2),
    ]));

    assert_eq!(reactor.test_default_query_space(), Some(space1));
    assert_eq!(
        reactor.test_resolve_menu_bar_space_with_preferred(Some(space2)),
        Some(space2),
        "menubar updates should follow the display currently hosting the menu bar"
    );
}

#[test]
fn menu_bar_space_falls_back_when_preferred_space_is_not_visible() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let visible_space = SpaceId::new(1);
    let hidden_space = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(visible_space)]));

    assert_eq!(
        reactor.test_resolve_menu_bar_space_with_preferred(Some(hidden_space)),
        Some(visible_space),
        "menubar updates should fall back to the normal active context if the preferred menubar space is unavailable"
    );
}

#[test]
fn workspace_queries_are_isolated_per_macos_space() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(space1),
        Some(space2),
    ]));

    reactor.handle_test_workspace_command(space1, &LayoutCommand::SwitchToWorkspace(0));
    reactor.handle_test_workspace_command(space2, &LayoutCommand::SwitchToWorkspace(1));

    let space1_workspaces = reactor.query_workspaces(Some(space1));
    let space2_workspaces = reactor.query_workspaces(Some(space2));

    assert_eq!(space1_workspaces.iter().filter(|ws| ws.is_active).count(), 1);
    assert_eq!(space2_workspaces.iter().filter(|ws| ws.is_active).count(), 1);
    assert_ne!(
        space1_workspaces.iter().position(|ws| ws.is_active),
        space2_workspaces.iter().position(|ws| ws.is_active),
        "each macOS space must retain its own active virtual workspace state",
    );

    reactor.handle_event(space_state_event(vec![left], vec![Some(space2)]));

    let default_workspaces = reactor.query_workspaces(None);
    assert_eq!(
        default_workspaces.iter().position(|ws| ws.is_active),
        space2_workspaces.iter().position(|ws| ws.is_active),
        "default workspace queries must reflect the currently active macOS space",
    );
}

#[test]
fn best_space_prefers_authoritative_window_server_space_over_geometry() {
    let mut reactor = test_reactor();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(11);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space2)]));
    reactor.insert_test_window(wid, wsid, Some(space1), frame, true);

    assert_eq!(reactor.best_space_for_window_id(wid), Some(space1));
}

/// These two carried `#[serde(skip)]`, so the recorder dropped them and a trace
/// could not show where a window went when it entered or left a space. Native
/// fullscreen is decided here, so that was exactly the blind spot.
#[test]
fn window_server_space_events_survive_the_recorder() {
    let wsid = WindowServerId::new(7);
    let space = SpaceId::new(3);

    for event in [
        Event::WindowServerAppeared(wsid, space, SpaceEventKind::Fullscreen),
        Event::WindowServerDestroyed(wsid, space, SpaceEventKind::User),
    ] {
        let json = serde_json::to_string(&event).expect("event must serialize into a trace");
        let back: Event = serde_json::from_str(&json).expect("event must replay from a trace");
        assert_eq!(format!("{back:?}"), format!("{event:?}"));
    }
}

#[test]
fn user_space_window_server_events_preserve_hidden_window_state() {
    let mut reactor = test_reactor();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(21);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space1)]));
    reactor.insert_test_window(wid, wsid, Some(space1), frame, true);

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(true));
    window_server_destroyed(&mut reactor, wsid, space1, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(reactor.state.windows.contains_window(wid));
    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space1));
    assert!(!reactor.state.windows.is_window_visible(wsid));
}

#[test]
fn user_space_window_server_destroyed_removes_window_when_window_server_is_gone() {
    let mut reactor = test_reactor();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(22);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space1)]));
    reactor.insert_test_window(wid, wsid, Some(space1), frame, true);
    reactor.state.windows.mark_window_visible(wsid);

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    crate::sys::window_server::set_window_gone_override(wsid, true);
    window_server_destroyed(&mut reactor, wsid, space1, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);
    crate::sys::window_server::set_window_gone_override(wsid, false);

    assert!(!reactor.state.windows.contains_window(wid));
    assert_eq!(reactor.state.windows.tracked_window_id(wsid), None);
    assert_eq!(reactor.assigned_space_for_window_id(wid), None);
}

/// Entering native fullscreen looks exactly like a close from the space the
/// window left: it stops being ordered in there. The window server still knows
/// it, though, and destroying it would drop its `WindowRecord` — `user_floating`
/// included — so it would come back for the app rules to re-float.
#[test]
fn user_space_window_server_destroyed_keeps_window_the_window_server_still_knows() {
    let mut reactor = test_reactor();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(23);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space1)]));
    reactor.insert_test_window(wid, wsid, Some(space1), frame, true);
    reactor.state.windows.mark_window_visible(wsid);
    reactor.state.windows.set_user_floating(wid, false);

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    window_server_destroyed(&mut reactor, wsid, space1, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(
        reactor.state.windows.contains_window(wid),
        "a window the window server still knows must not be retired"
    );
    assert_eq!(
        reactor.state.windows.user_floating(wid),
        Some(false),
        "the user's manual tile must survive the departure"
    );
    assert!(!reactor.state.windows.is_window_visible(wsid));
}

/// Builds a reactor with `space1` active on a screen and a single tiled window
/// (`wid`/`wsid`) assigned to `space1`. `space2` exists with workspaces so it can
/// be a reassignment target. Returns the pieces the `appeared` tests need.
fn reactor_with_window_on_space1() -> (Reactor, WindowId, WindowServerId, SpaceId, SpaceId, CGRect)
{
    let mut reactor = test_reactor();
    let pid = 1;
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new(101);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space1)]));

    reactor.add_test_app(pid);

    let space1_workspace = reactor.test_workspace(space1, 0);
    let _ = reactor.test_workspace_ids(space2);

    reactor.add_test_window(wid, wsid, Some(space1), frame);

    assert!(reactor.assign_test_window_to_workspace(space1, wid, space1_workspace));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space1));

    (reactor, wid, wsid, space1, space2, frame)
}

fn reactor_with_window_moved_to_space2()
-> (Reactor, WindowId, WindowServerId, SpaceId, SpaceId, CGRect) {
    let mut reactor = test_reactor();
    let pid = 1;
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let screen2 = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let moved_frame = CGRect::new(CGPoint::new(1600., 100.), CGSize::new(800., 600.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new(111);

    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(space1),
        Some(space2),
    ]));

    reactor.add_test_app(pid);

    let space1_workspace = reactor.test_workspace(space1, 0);
    let space2_workspace = reactor.test_workspace(space2, 0);

    reactor.add_test_window(wid, wsid, Some(space2), moved_frame);

    assert!(reactor.assign_test_window_to_workspace(space1, wid, space1_workspace));
    assert!(reactor.assign_test_window_to_workspace(space2, wid, space2_workspace));
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, moved_frame);
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));

    (reactor, wid, wsid, space1, space2, moved_frame)
}

fn reactor_with_window_on_space1_two_displays() -> (
    Reactor,
    WindowId,
    WindowServerId,
    SpaceId,
    SpaceId,
    CGRect,
    CGRect,
) {
    let mut reactor = test_reactor();
    let pid = 1;
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let screen2 = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let initial_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new(121);

    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(space1),
        Some(space2),
    ]));

    reactor.add_test_app(pid);

    let space1_workspace = reactor.test_workspace(space1, 0);
    let _ = reactor.test_workspace_ids(space2);

    reactor.add_test_window(wid, wsid, Some(space1), initial_frame);

    assert!(reactor.assign_test_window_to_workspace(space1, wid, space1_workspace));

    (reactor, wid, wsid, space1, space2, initial_frame, screen2)
}

fn reactor_with_floating_window() -> (Reactor, WindowId, SpaceId, CGRect, CGRect) {
    let (mut reactor, wid, _wsid, space1, _space2, screen) = reactor_with_window_on_space1();
    reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space1, wid));
    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    assert!(reactor.layout_manager.layout_engine.is_window_floating(wid));

    let workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(space1)
        .expect("workspace");
    let floating_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(400., 300.));
    if let Some(w) = reactor.state.windows.window_mut(wid) {
        w.frame_monotonic = floating_frame;
    }
    reactor.layout_manager.layout_engine.store_floating_position(
        space1,
        workspace,
        wid,
        floating_frame,
    );

    (reactor, wid, space1, screen, floating_frame)
}

fn window_server_appeared(
    reactor: &mut Reactor,
    wsid: WindowServerId,
    space: SpaceId,
    kind: SpaceEventKind,
) {
    SpaceEventHandler::handle_window_server_appeared(reactor, wsid, space, kind);
}

fn window_server_destroyed(
    reactor: &mut Reactor,
    wsid: WindowServerId,
    space: SpaceId,
    kind: SpaceEventKind,
) {
    SpaceEventHandler::handle_window_server_destroyed(
        reactor,
        SpaceEventHandler::WindowServerLifecyclePayload {
            window_server_id: wsid,
            space,
            kind,
        },
    )
    .unwrap();
}

#[test]
fn appeared_reassigns_window_without_pending_rift_move() {
    let (mut reactor, wid, wsid, space1, space2, _frame) = reactor_with_window_on_space1();

    // No pending transaction: this is a genuine external space change, so Rift should
    // follow it and reassign the window to the reported space.
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space1));

    window_server_appeared(&mut reactor, wsid, space2, SpaceEventKind::User);

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "window without an in-flight Rift move must follow a genuine external space change"
    );
}

#[test]
fn geometry_cross_display_frame_change_updates_authoritative_space() {
    let (mut reactor, wid, wsid, _space1, space2, _initial_frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let moved_frame = CGRect::new(
        CGPoint::new(screen2.origin.x + 100., 100.),
        CGSize::new(800., 600.),
    );

    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        moved_frame,
        None,
        Requested(false),
        Some(MouseState::Up),
    ));

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "geometry-only cross-display move should update workspace ownership"
    );
    assert_eq!(
        reactor.state.windows.window_server_space(wsid),
        Some(space2),
        "geometry-only cross-display move should update authoritative server space"
    );
}

#[test]
fn matching_rift_frame_clears_pending_target() {
    let (mut reactor, wid, wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let target_frame = CGRect::new(
        CGPoint::new(frame.origin.x + 40.0, frame.origin.y + 25.0),
        frame.size,
    );
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, target_frame);

    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        target_frame,
        Some(txid),
        Requested(true),
        Some(MouseState::Up),
    ));

    assert_eq!(
        reactor.transaction_manager.get_target_frame(wsid),
        None,
        "a confirmed Rift frame must clear the pending target"
    );
    assert!(
        reactor
            .state
            .windows
            .window(wid)
            .expect("window should still exist")
            .frame_monotonic
            .same_as(target_frame)
    );

    // AX may adjust a requested frame; cache the accepted geometry but keep the target pending.
    let adjusted_target = CGRect::new(CGPoint::new(80.0, 40.0), frame.size);
    let accepted = CGRect::new(CGPoint::new(81.0, 40.0), frame.size);
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, adjusted_target);
    let outcome = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            accepted,
            Some(txid),
            Requested(true),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(reactor.state.windows.window(wid).unwrap().frame_monotonic.same_as(accepted));
    assert_eq!(
        reactor.transaction_manager.get_target_frame(wsid),
        Some(adjusted_target)
    );
    assert!(!outcome.arrange.requested && !outcome.refresh_layout_mode);

    // A user drag beginning during the transaction clears it instead of accepting it blindly.
    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        accepted,
        Some(txid),
        Requested(true),
        Some(MouseState::Down),
    ));
    assert_eq!(reactor.transaction_manager.get_target_frame(wsid), None);
}

#[test]
fn frame_acknowledgements_and_unchanged_frames_do_not_invalidate_layout() {
    let (mut reactor, wid, wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let target_frame = CGRect::new(
        CGPoint::new(frame.origin.x + 40.0, frame.origin.y + 25.0),
        frame.size,
    );
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, target_frame);

    let acknowledgement = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            target_frame,
            Some(txid),
            Requested(true),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!acknowledgement.arrange.requested);
    assert!(!acknowledgement.refresh_layout_mode);

    let unchanged = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            target_frame,
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!unchanged.arrange.requested);
    assert!(!unchanged.refresh_layout_mode);

    let explicitly_requested_frame = CGRect::new(
        CGPoint::new(target_frame.origin.x + 10.0, target_frame.origin.y),
        target_frame.size,
    );
    let requested = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            explicitly_requested_frame,
            None,
            Requested(true),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!requested.arrange.requested);
    assert!(!requested.refresh_layout_mode);
}

#[test]
fn genuine_external_frame_changes_invalidate_layout() {
    let (mut reactor, wid, _wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let moved_frame = CGRect::new(
        CGPoint::new(frame.origin.x + 40.0, frame.origin.y + 25.0),
        frame.size,
    );

    let outcome = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            moved_frame,
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();

    assert!(outcome.arrange.requested);
    assert_eq!(outcome.arrange.passes, 1);
    assert!(outcome.refresh_layout_mode);
}

#[test]
fn stale_and_inactive_frame_events_request_no_arrange_passes() {
    let (mut reactor, wid, wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let target_frame = CGRect::new(
        CGPoint::new(frame.origin.x + 40.0, frame.origin.y + 25.0),
        frame.size,
    );
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, target_frame);
    let acknowledgement = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            target_frame,
            Some(txid),
            Requested(true),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!acknowledgement.arrange.requested);

    let duplicate = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            target_frame,
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!duplicate.arrange.requested);

    // Stale transaction notification while a newer target is pending.
    let current_txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, current_txid, target_frame);
    let stale = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            CGRect::new(
                CGPoint::new(target_frame.origin.x + 20.0, target_frame.origin.y),
                target_frame.size,
            ),
            Some(current_txid.next()),
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!stale.arrange.requested);

    // Geometry on an inactive native space.
    reactor.transaction_manager.clear_target_for_window(wsid);
    reactor.set_active_spaces(&[]);
    let inactive = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            CGRect::new(
                CGPoint::new(target_frame.origin.x + 30.0, target_frame.origin.y),
                target_frame.size,
            ),
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();
    assert!(!inactive.arrange.requested);
}

#[test]
fn external_resize_requests_one_arrange_pass() {
    let (mut reactor, wid, _wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let resized = CGRect::new(
        frame.origin,
        CGSize::new(frame.size.width + 80.0, frame.size.height + 40.0),
    );

    let outcome = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            resized,
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();

    assert!(outcome.arrange.requested);
    assert_eq!(outcome.arrange.passes, 1);
    assert!(outcome.arrange.is_resize);
}

/// Dropping a dragged window has to lay the space out again even when nothing
/// was swapped: layout was skipped for the window while it followed the
/// pointer, so its frame no longer matches the tree and it would otherwise be
/// left wherever the drag ended.
#[test]
fn dropping_with_no_swap_still_arranges_the_window_back() {
    let (mut reactor, wid, _wsid, space1, _space2, _frame) = reactor_with_window_on_space1();
    let (visible_spaces, visible_space_centers) = reactor.visible_spaces_for_layout(true);

    // The state a drag ends in once it has had, and then lost, a swap target:
    // no session left, but layout was skipped along the way.
    reactor.drag_manager.drag_state = DragState::Inactive;
    reactor.drag_manager.skip_layout_for_window = Some(wid);

    let outcome = crate::actor::reactor::events::drag::handle_mouse_up(
        &mut reactor.state,
        &mut reactor.layout_manager,
        &mut reactor.drag_manager,
        crate::actor::reactor::events::drag::MouseUpPayload {
            pending_swap: None,
            drop_action: None,
            swap_space: Some(space1),
            final_space: Some(space1),
            visible_spaces,
            visible_space_centers,
            screens: vec![],
            pointer: None,
        },
    )
    .unwrap();

    assert!(
        outcome.arrange.requested && outcome.arrange.passes > 0,
        "a drop that skipped layout must arrange, got {:?}",
        outcome.arrange
    );
}

/// A tile drag leaves the window off its slot. The drop's arrange skips a
/// target that matches the write still pending for the window — and the last
/// arrange's write for that very slot was still on the books — so the drop
/// wrote nothing and the window stayed where it was dropped until a later
/// sweep happened to clear the entry. Taking hold of a window clears it.
#[test]
fn drop_after_a_drag_writes_the_slot_even_when_that_write_was_still_pending() {
    let (mut reactor, wid, wsid, space1, _space2, screen) = reactor_with_window_on_space1();
    reactor.send_layout_event(crate::actor::reactor::LayoutEvent::WindowAdded(space1, wid));
    let (app_tx, mut app_rx) = crate::actor::channel();
    reactor.app_manager.apps.get_mut(&wid.pid).unwrap().handle =
        crate::actor::app::AppThreadHandle::new_for_test(app_tx);
    let slot = crate::sys::geometry::Round::round(
        &test_layout(&mut reactor, space1, screen)
            .into_iter()
            .find(|(w, _)| *w == wid)
            .map(|(_, frame)| frame)
            .expect("the window is tiled"),
    );
    // In its slot, with the write that put it there still pending.
    reactor.state.windows.window_mut(wid).unwrap().frame_monotonic = slot;
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, slot);
    while app_rx.try_recv().is_ok() {}

    let moved = CGRect::new(CGPoint::new(slot.origin.x + 40., slot.origin.y + 30.), slot.size);
    crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Down));
    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        moved,
        Some(txid),
        crate::model::reactor::Requested(false),
        Some(crate::sys::event::MouseState::Down),
    ));
    assert_eq!(reactor.window_in_drag(), Some(wid), "the drag is tracked");
    crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Up));
    reactor.handle_event(Event::MouseUp);
    crate::sys::event::set_mouse_state_override(None);

    let written = std::iter::from_fn(|| app_rx.try_recv().ok())
        .filter_map(|(_, request)| match request {
            Request::SetWindowFrame(w, frame, _, _) if w == wid => Some(frame),
            Request::AnimationFrame { wid: w, frame, .. } if w == wid => Some(frame),
            Request::SetBatchWindowFrame(frames, _, _) => {
                frames.into_iter().find(|(w, _)| *w == wid).map(|(_, frame)| frame)
            }
            _ => None,
        })
        .last();
    assert_eq!(
        written,
        Some(slot),
        "the drop must put the window back in its slot"
    );
}

/// A modifier resize moves the edges the press landed nearest, so the window
/// follows the cursor. Growing from the origin instead moves the right and
/// bottom edges whichever way the cursor goes, which reads as the window
/// resizing backwards.
#[test]
fn modifier_resize_moves_the_edges_the_press_landed_on() {
    use crate::actor::reactor::ResizeEdges;

    let frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(400., 400.));

    // Pressing the right half and dragging right widens the window in place.
    let right = ResizeEdges::from_press(frame, CGPoint::new(450., 300.));
    let grown = right.apply(frame, 50., 0.);
    assert_eq!(grown.origin.x, 100., "the left edge stays put");
    assert_eq!(grown.size.width, 450.);

    // Pressing the left half and dragging left also widens it, by moving the
    // left edge out — the opposite sign on the same gesture.
    let left = ResizeEdges::from_press(frame, CGPoint::new(150., 300.));
    let grown = left.apply(frame, -50., 0.);
    assert_eq!(grown.origin.x, 50., "the left edge follows the cursor");
    assert_eq!(grown.size.width, 450.);
    assert_eq!(grown.max().x, 500., "the right edge stays put");

    // Top-left press moves both the left and top edges.
    let corner = ResizeEdges::from_press(frame, CGPoint::new(120., 120.));
    let grown = corner.apply(frame, -10., -20.);
    assert_eq!((grown.origin.x, grown.origin.y), (90., 80.));
    assert_eq!((grown.size.width, grown.size.height), (410., 420.));

    // A fast drag past the far edge stops rather than inverting the window.
    let collapsed = left.apply(frame, 10_000., 0.);
    assert!(collapsed.size.width > 0.);
    assert_eq!(collapsed.max().x, 500., "the fixed edge is still fixed");
}

/// Self-fullscreen is a *transition* into or out of covering the screen, and
/// only the transitions are kept out of the layout. Anything else is an
/// ordinary resize, including resizing a window that already fills its screen.
#[test]
fn only_transitions_across_full_screen_are_kept_out_of_the_layout() {
    let (mut reactor, wid, _wsid, _space1, _space2, frame) = reactor_with_window_on_space1();
    let resize = |reactor: &mut Reactor, to: CGRect| {
        reactor
            .dispatch_workflow(Event::WindowFrameChanged(
                wid,
                to,
                None,
                Requested(false),
                Some(MouseState::Up),
            ))
            .unwrap()
    };
    let scaled = |factor: f64| {
        CGRect::new(
            frame.origin,
            CGSize::new(frame.size.width * factor, frame.size.height * factor),
        )
    };

    // The fixture's window starts filling the screen, so shrinking it is the
    // "leaving" transition: the stored layout is re-asserted rather than
    // rebuilt from the restored frame.
    let outcome = resize(&mut reactor, scaled(0.5));
    assert!(!outcome.arrange.is_resize, "leaving full screen is not a resize");

    // Neither frame covers, so this is just a resize.
    let outcome = resize(&mut reactor, scaled(0.75));
    assert!(outcome.arrange.is_resize, "an ordinary resize is still a resize");

    // Growing back over the whole screen is the "entering" transition, which
    // must not rewrite the split ratios around the window.
    let outcome = resize(&mut reactor, frame);
    assert!(
        !outcome.arrange.is_resize,
        "self-fullscreen must not be folded into the layout"
    );
}

#[test]
fn crossing_native_spaces_reconciles_membership_with_one_arrange_pass() {
    let (mut reactor, wid, wsid, _space1, space2, frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let moved = CGRect::new(
        CGPoint::new(screen2.origin.x + 100.0, frame.origin.y),
        frame.size,
    );

    let outcome = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            wid,
            moved,
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert!(outcome.arrange.requested);
    assert_eq!(outcome.arrange.passes, 1);
}

#[test]
fn unmanageable_window_crossing_spaces_is_not_reinserted_into_layout() {
    let mut reactor = test_reactor();
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 800.));
    let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 800.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(101);
    let initial_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(500., 400.));
    let moved_frame = CGRect::new(CGPoint::new(1100., 100.), CGSize::new(500., 400.));

    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(space1),
        Some(space2),
    ]));
    reactor.add_test_window_with_manageability(wid, wsid, Some(space1), initial_frame, false);

    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        moved_frame,
        None,
        Requested(false),
        Some(crate::sys::event::MouseState::Up),
    ));

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), None);
    assert!(!has_window_in_layout(&mut reactor, space2, screen2, wid));
}

#[test]
fn duplicate_minimize_deminimize_and_unknown_window_events_do_not_arrange() {
    let (mut reactor, wid, _wsid, _space1, _space2, _frame) = reactor_with_window_on_space1();

    reactor.dispatch_workflow(Event::WindowMinimized(wid)).unwrap();
    let duplicate_minimize = reactor.dispatch_workflow(Event::WindowMinimized(wid)).unwrap();
    assert!(!duplicate_minimize.arrange.requested);

    reactor.dispatch_workflow(Event::WindowDeminiaturized(wid)).unwrap();
    let duplicate_deminimize = reactor.dispatch_workflow(Event::WindowDeminiaturized(wid)).unwrap();
    assert!(!duplicate_deminimize.arrange.requested);

    let unknown = WindowId::new(wid.pid + 100, wid.idx.get());
    let unknown_minimize = reactor.dispatch_workflow(Event::WindowMinimized(unknown)).unwrap();
    let unknown_deminimize =
        reactor.dispatch_workflow(Event::WindowDeminiaturized(unknown)).unwrap();
    let unknown_frame = reactor
        .dispatch_workflow(Event::WindowFrameChanged(
            unknown,
            CGRect::default(),
            None,
            Requested(false),
            Some(MouseState::Up),
        ))
        .unwrap();

    assert!(!unknown_minimize.arrange.requested);
    assert!(!unknown_deminimize.arrange.requested);
    assert!(!unknown_frame.arrange.requested);
}

#[test]
fn cross_display_drag_clears_source_floating_position() {
    let (mut reactor, wid, _wsid, space1, space2, initial_frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let source_workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(space1)
        .expect("source workspace");
    let target_workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(space2)
        .expect("target workspace");

    reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space1, wid));
    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    assert!(reactor.layout_manager.layout_engine.is_window_floating(wid));
    reactor.layout_manager.layout_engine.store_floating_position(
        space1,
        source_workspace,
        wid,
        initial_frame,
    );

    let moved_frame = CGRect::new(
        CGPoint::new(screen2.origin.x + 120.0, initial_frame.origin.y),
        initial_frame.size,
    );
    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: wid,
            last_frame: moved_frame,
            origin_space: None,
            settled_space: Some(space2),
            layout_dirty: true,
        },
    };

    let (visible_spaces, visible_space_centers) = reactor.visible_spaces_for_layout(true);
    let outcome = crate::actor::reactor::events::drag::handle_mouse_up(
        &mut reactor.state,
        &mut reactor.layout_manager,
        &mut reactor.drag_manager,
        crate::actor::reactor::events::drag::MouseUpPayload {
            pending_swap: None,
            drop_action: None,
            swap_space: Some(space2),
            final_space: Some(space2),
            visible_spaces,
            visible_space_centers,
            screens: vec![],
            pointer: None,
        },
    )
    .unwrap();
    assert!(outcome.arrange.requested);
    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(
        reactor
            .layout_manager
            .layout_engine
            .get_floating_position(space1, source_workspace, wid),
        None,
        "cross-display drags must clear the source workspace's floating position"
    );
    assert_eq!(
        reactor
            .layout_manager
            .layout_engine
            .get_floating_position(space2, target_workspace, wid),
        Some(moved_frame)
    );
}

#[test]
fn stale_user_space_disappearance_does_not_restore_old_display_assignment() {
    let (mut reactor, wid, wsid, space1, space2, _) = reactor_with_window_moved_to_space2();

    window_server_destroyed(&mut reactor, wsid, space1, SpaceEventKind::User);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert!(reactor.state.windows.is_window_visible(wsid));

    let _ = reactor.reconcile_windows_with_authoritative_spaces();

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "late disappearance from the old display must not drag a moved window back"
    );
}

#[test]
fn stale_user_space_appearance_does_not_restore_old_display_assignment() {
    let (mut reactor, wid, wsid, space1, space2, _) = reactor_with_window_moved_to_space2();

    window_server_appeared(&mut reactor, wsid, space1, SpaceEventKind::User);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));

    let _ = reactor.reconcile_windows_with_authoritative_spaces();

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "late appearance on the old display must not overwrite the newer target assignment"
    );
}

#[test]
fn stale_user_space_appearance_is_ignored_when_server_state_already_matches_pending_target() {
    let (mut reactor, wid, wsid, space1, space2, _frame) = reactor_with_window_moved_to_space2();
    let space1_workspace = reactor.test_workspace(space1, 0);

    assert!(reactor.assign_test_window_to_workspace(space1, wid, space1_workspace));
    reactor.state.windows.set_window_server_space(wsid, Some(space1));
    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    let target_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    reactor.transaction_manager.store_txid(wsid, txid, target_frame);

    window_server_appeared(&mut reactor, wsid, space2, SpaceEventKind::User);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space1));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space1));
    assert_eq!(
        reactor.authoritative_space_for_window_id(wid),
        Some(space1),
        "late appearance from the old display should be ignored once Rift has already committed the new server-space target"
    );
}

#[test]
fn stale_user_space_appearance_is_ignored_when_authoritative_window_space_differs() {
    let (mut reactor, wid, wsid, space1, space2, _frame) = reactor_with_window_moved_to_space2();
    crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![space2.get()]));

    window_server_appeared(&mut reactor, wsid, space1, SpaceEventKind::User);

    crate::sys::window_server::set_window_spaces_override(wsid, None);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(reactor.authoritative_space_for_window_id(wid), Some(space2));
}

#[test]
fn multi_active_visible_window_appearance_keeps_display_assignment_and_visibility() {
    let (mut reactor, wid, wsid, space1, space2, _frame) = reactor_with_window_moved_to_space2();

    window_server_appeared(&mut reactor, wsid, space1, SpaceEventKind::User);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(reactor.authoritative_space_for_window_id(wid), Some(space2));
    assert!(reactor.state.windows.is_window_visible(wsid));
}

#[test]
fn multi_active_visible_window_disappearance_does_not_reassign_between_display_spaces() {
    let (mut reactor, wid, wsid, space1, space2, _frame) = reactor_with_window_moved_to_space2();

    window_server_destroyed(&mut reactor, wsid, space1, SpaceEventKind::User);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert!(reactor.state.windows.is_window_visible(wsid));
}

#[test]
fn hidden_window_can_move_to_another_native_space_without_staying_pinned_to_old_display() {
    let mut reactor = test_reactor_with_workspace_count(2);
    let pid = 1;
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let right = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new(121);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(space1),
        Some(space2),
    ]));

    reactor.add_test_app(pid);

    let workspaces = reactor.test_workspace_ids(space1);
    let hidden_workspace = workspaces[0];
    let visible_workspace = workspaces[1];
    let _ = reactor.test_workspace_ids(space2);

    reactor.add_test_window(wid, wsid, Some(space1), frame);

    assert!(reactor.set_test_active_workspace(space1, visible_workspace));
    assert!(reactor.assign_test_window_to_workspace(space1, wid, hidden_workspace));
    assert_eq!(reactor.hidden_assigned_space_for_window_id(wid), Some(space1));

    crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![space2.get()]));
    window_server_appeared(&mut reactor, wsid, space2, SpaceEventKind::User);
    crate::sys::window_server::set_window_spaces_override(wsid, None);

    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(reactor.authoritative_space_for_window_id(wid), Some(space2));
}

#[test]
fn discovery_prefers_authoritative_space_over_geometry_when_displays_overlap_workspaces() {
    let (mut reactor, wid, wsid, space1, space2, _moved_frame) =
        reactor_with_window_moved_to_space2();
    let conflicting_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));

    reactor
        .state
        .windows
        .window_mut(wid)
        .expect("window should exist")
        .frame_monotonic = conflicting_frame;
    reactor.track_test_window_server_info(wsid, wid.pid, conflicting_frame);

    assert_eq!(
        reactor.discovery_space_for_window_id(wid),
        Some(space2),
        "discovery should stay in the authoritative native space instead of hopping to another display's geometry"
    );
    assert_ne!(
        reactor.discovery_space_for_window_id(wid),
        Some(space1),
        "same-index workspaces on other displays must stay isolated"
    );
}

#[test]
fn recent_cross_display_move_ignores_conflicting_geometry_space_change() {
    let (mut reactor, wid, wsid, _space1, space2, _) = reactor_with_window_moved_to_space2();
    let conflicting_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));

    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        conflicting_frame,
        None,
        Requested(false),
        Some(MouseState::Up),
    ));

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
}

#[test]
fn central_space_resolution_prefers_recent_move_target_over_stale_server_space() {
    let (mut reactor, wid, wsid, space1, space2, moved_frame) =
        reactor_with_window_moved_to_space2();

    reactor.state.windows.set_window_server_space(wsid, Some(space1));

    assert_eq!(reactor.authoritative_space_for_window_id(wid), Some(space2));
    assert_eq!(
        reactor.best_space_for_window(&moved_frame, Some(wsid)),
        Some(space2),
        "core space resolution should prefer the recent move target when geometry and assignment agree"
    );
}

#[test]
fn active_space_membership_refresh_does_not_overwrite_recent_move_target() {
    let (mut reactor, wid, wsid, space1, space2, _) = reactor_with_window_moved_to_space2();

    reactor.refresh_active_space_window_membership(vec![(wsid, Some(space1))]);

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert_eq!(
        reactor.state.windows.window_server_space(wsid),
        Some(space2),
        "active-space reconciliation must not overwrite a recent cross-display move with stale membership"
    );
    assert!(reactor.state.windows.is_window_visible(wsid));
}

#[test]
fn known_fullscreen_window_appearance_removes_window_from_layout() {
    let (mut apps, mut reactor) = test_context();

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let wid = WindowId::new(1, 1);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(1), Some(wid));

    assert!(has_window_in_layout(&mut reactor, user_space, frame, wid));
    let wsid = reactor.state.windows.window(wid).unwrap().info.sys_id.unwrap();

    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);

    assert!(
        !has_window_in_layout(&mut reactor, user_space, frame, wid),
        "managed window should be removed from layout when it enters native fullscreen"
    );
    assert!(
        reactor
            .state
            .windows
            .native_fullscreen_record_for_window_server_id(wsid)
            .is_some_and(|record| record.fullscreen_space == fullscreen_space),
        "fullscreen transition should record suspended window state"
    );
}

/// The window server announces native fullscreen twice: the window leaves its
/// own space, and it arrives on the fullscreen one. When the departure lands
/// first the slot used to be read after the window had already left its tree,
/// so a window tiled on the left came back on the right.
#[test]
fn fullscreen_exit_restores_the_slot_when_the_departure_is_seen_first() {
    let (mut apps, mut reactor) = test_context();

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let left = WindowId::new(1, 1);
    let right = WindowId::new(1, 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(2), Some(left));

    let order = |reactor: &Reactor| {
        reactor
            .layout_manager
            .layout_engine
            .windows_on_space_in_layout_order(user_space)
    };
    assert_eq!(order(&reactor), vec![left, right]);

    let wsid = reactor.state.windows.window(left).unwrap().info.sys_id.unwrap();

    // Departure first, arrival second — the order that lost the slot.
    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    window_server_destroyed(&mut reactor, wsid, user_space, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);
    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);

    assert!(!has_window_in_layout(&mut reactor, user_space, frame, left));

    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);

    assert_eq!(
        order(&reactor),
        vec![left, right],
        "a window tiled on the left must come back on the left, not beside the selection"
    );
}

/// The live sequence, as recorded off a YouTube video going fullscreen in Zen
/// with the browser tiled left of an editor: the window leaves its user space,
/// arrives on a fullscreen space, the display's active space *becomes* that
/// fullscreen space, and then the whole thing runs backwards. The window is
/// announced home before the display is, so the exit itself cannot put it
/// back — the addition that follows has to.
fn fullscreen_exit_across_an_active_fullscreen_space(mode: LayoutMode) {
    let mut apps = Apps::new();
    let settings = crate::common::config::LayoutSettings {
        mode,
        ..crate::common::config::LayoutSettings::default()
    };
    let mut reactor = Reactor::new_for_test(LayoutEngine::new(
        &crate::common::config::VirtualWorkspaceSettings::default(),
        &settings,
        None,
    ));

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let left = WindowId::new(1, 1);
    let right = WindowId::new(1, 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(2), Some(left));

    let order = |reactor: &Reactor| {
        reactor
            .layout_manager
            .layout_engine
            .windows_on_space_in_layout_order(user_space)
    };
    assert_eq!(order(&reactor), vec![left, right]);

    let wsid = reactor.state.windows.window(left).unwrap().info.sys_id.unwrap();

    // Going fullscreen: the window leaves its space, lands on the fullscreen
    // one, and the display follows it there.
    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    window_server_destroyed(&mut reactor, wsid, user_space, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);
    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);
    assert!(!has_window_in_layout(&mut reactor, user_space, frame, left));

    reactor.space_state.fullscreen_spaces.insert(fullscreen_space);
    reactor.handle_event(space_state_event(vec![frame], vec![None]));
    apps.simulate_until_quiet(&mut reactor);

    // Coming back: the window leaves the fullscreen space and reappears on its
    // own while the display is still on the fullscreen space, which only then
    // goes away.
    window_server_destroyed(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);
    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);
    reactor.space_state.fullscreen_spaces.remove(&fullscreen_space);
    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(
        order(&reactor),
        vec![left, right],
        "a window tiled on the left must come back on the left, not beside the selection"
    );
}

#[test]
fn fullscreen_exit_across_an_active_fullscreen_space_traditional() {
    fullscreen_exit_across_an_active_fullscreen_space(LayoutMode::Traditional);
}

#[test]
fn fullscreen_exit_across_an_active_fullscreen_space_bsp() {
    fullscreen_exit_across_an_active_fullscreen_space(LayoutMode::Bsp);
}

/// The same exit, with the window server's ordering-out mixed in — recorded
/// off a YouTube video in Zen. The window is ordered out while it leaves the
/// fullscreen space and only ordered back in *after* the display has reached
/// the user space, so every path that runs on the arrival — the appearance,
/// the space change, the discovery it triggers — finds the window hidden and
/// leaves it out. The order-in is the last event that mentions the window,
/// and it has to be the one that puts it back.
#[test]
fn fullscreen_exit_restores_the_slot_when_the_window_is_ordered_in_last() {
    let (mut apps, mut reactor) = test_context();

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let left = WindowId::new(1, 1);
    let right = WindowId::new(1, 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(2), Some(left));

    let order = |reactor: &Reactor| {
        reactor
            .layout_manager
            .layout_engine
            .windows_on_space_in_layout_order(user_space)
    };
    assert_eq!(order(&reactor), vec![left, right]);

    let wsid = reactor.state.windows.window(left).unwrap().info.sys_id.unwrap();

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    window_server_destroyed(&mut reactor, wsid, user_space, SpaceEventKind::User);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);
    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);
    assert!(!has_window_in_layout(&mut reactor, user_space, frame, left));

    reactor.space_state.fullscreen_spaces.insert(fullscreen_space);
    reactor.handle_event(space_state_event(vec![frame], vec![None]));
    apps.simulate_until_quiet(&mut reactor);

    // Leaving: ordered out, gone from the fullscreen space, back on the user
    // space, and the display follows — all while the window is still hidden.
    // An inventory taken meanwhile leaves the window out, as the app actor
    // does for an AX window without an on-screen window-server peer; the
    // window server still knows the window and calls it suitable, so the
    // omission does not retire it.
    let hidden = apps.windows.remove(&left).expect("the window is known to its app");
    crate::sys::window_server::set_window_suitability_override(wsid, Some(true));
    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    window_server_destroyed(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);
    reactor.handle_event(Event::WindowServerVisibilityChanged(wsid, false));
    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);
    reactor.space_state.fullscreen_spaces.remove(&fullscreen_space);
    // The display's snapshot lists what is on screen, which is not yet the
    // returning window.
    let right_wsid = reactor.state.windows.window(right).unwrap().info.sys_id.unwrap();
    reactor.handle_event(space_state_event_with(
        vec![frame],
        vec![Some(user_space)],
        |state| {
            state.active_window_spaces.insert(right_wsid, user_space);
        },
    ));
    apps.simulate_until_quiet(&mut reactor);
    assert!(
        reactor.state.windows.contains_window(left),
        "the window is not dead, only hidden"
    );
    assert!(
        !has_window_in_layout(&mut reactor, user_space, frame, left),
        "a window the window server still reports hidden must not be tiled yet"
    );

    // Then the window server orders it in.
    apps.windows.insert(left, hidden);
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);
    crate::sys::window_server::set_window_suitability_override(wsid, None);
    reactor.handle_event(Event::WindowServerVisibilityChanged(wsid, true));
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(
        order(&reactor),
        vec![left, right],
        "the order-in is the last event about the window; it must put it back where it was"
    );
}

#[test]
fn known_window_server_appearance_restores_same_workspace_after_fullscreen() {
    let (mut apps, mut reactor) = test_context();

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let wid = WindowId::new(1, 1);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(1), Some(wid));

    let wsid = reactor.state.windows.window(wid).unwrap().info.sys_id.unwrap();
    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);
    assert!(!has_window_in_layout(&mut reactor, user_space, frame, wid));

    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);

    assert!(
        has_window_in_layout(&mut reactor, user_space, frame, wid),
        "managed window should return to layout when native fullscreen exits back to the same space"
    );
}

#[test]
fn fullscreen_tracking_survives_until_ax_window_id_arrives() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let pid: pid_t = 61;
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 1);
    let frame = CGRect::new(CGPoint::new(50., 50.), CGSize::new(900., 700.));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(user_space)]));

    let (app_tx, mut app_rx) = crate::actor::channel();
    reactor.app_manager.apps.insert(pid, AppState {
        info: AppInfo {
            bundle_id: Some("com.test.pending-fullscreen".to_string()),
            localized_name: Some("Pending Fullscreen".to_string()),
        },
        handle: AppThreadHandle::new_for_test(app_tx),
    });

    reactor.track_test_window_server_info(wsid, pid, frame);

    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);

    assert!(
        reactor
            .state
            .windows
            .pending_native_fullscreen_record_for_window_server_id(wsid)
            .is_some_and(|record| {
                record.pid == pid
                    && record.last_known_user_space == Some(user_space)
                    && record.fullscreen_space == fullscreen_space
            }),
        "fullscreen lifecycle should be retained by wsid until AX tracking binds the window"
    );
    assert!(
        matches!(app_rx.try_recv(), Ok((_, Request::RefreshWindowInventory(_)))),
        "fullscreen appearance without AX tracking should still request a visible-window refresh"
    );

    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);

    assert!(
        app_rx.try_recv().is_err(),
        "fullscreen exit should coalesce behind the in-flight inventory"
    );

    reactor.discover_test_windows(
        pid,
        vec![(
            wid,
            make_window_info(frame, Some(wsid), "Recovered Window", None),
        )],
        vec![wid],
    );
    assert!(
        matches!(app_rx.try_recv(), Ok((_, Request::RefreshWindowInventory(_)))),
        "completing the in-flight inventory should issue the queued refresh"
    );

    assert!(
        reactor
            .state
            .windows
            .pending_native_fullscreen_record_for_window_server_id(wsid)
            .is_none(),
        "binding the AX window id should consume the pending fullscreen record"
    );
    assert!(
        reactor.state.windows.native_fullscreen_record_for_window(wid).is_none(),
        "once the window is back on its user space, the fullscreen lifecycle should retire"
    );
    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(user_space));
}

#[test]
fn fullscreen_does_not_suppress_other_same_pid_windows() {
    let (mut reactor, original_wid, original_wsid, user_space, _other_space, frame) =
        reactor_with_window_on_space1();
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let second_wid = WindowId::new(original_wid.pid, 1002);
    let second_wsid = WindowServerId::new(10002);

    window_server_appeared(
        &mut reactor,
        original_wsid,
        fullscreen_space,
        SpaceEventKind::Fullscreen,
    );

    reactor.handle_event(Event::WindowCreated(
        second_wid,
        make_window_info(frame, Some(second_wsid), "Second Window", None),
        Some(crate::sys::window_server::WindowServerInfo {
            id: second_wsid,
            pid: original_wid.pid,
            layer: 0,
            frame,
            min_frame: frame.size,
            max_frame: frame.size,
        }),
        None,
    ));

    assert_eq!(
        reactor.assigned_space_for_window_id(second_wid),
        Some(user_space)
    );
}

#[test]
fn fullscreen_exit_removes_non_queryable_duplicate_from_layout() {
    let (mut reactor, original_wid, original_wsid, user_space, other_space, frame) =
        reactor_with_window_on_space1();
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let duplicate_wid = WindowId::new(original_wid.pid, 27481);
    let duplicate_wsid = WindowServerId::new(27481);
    let active_workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(user_space)
        .expect("active workspace");

    window_server_appeared(
        &mut reactor,
        original_wsid,
        fullscreen_space,
        SpaceEventKind::Fullscreen,
    );

    reactor.add_test_window_with_manageability(
        duplicate_wid,
        duplicate_wsid,
        Some(fullscreen_space),
        frame,
        false,
    );

    window_server_appeared(
        &mut reactor,
        duplicate_wsid,
        fullscreen_space,
        SpaceEventKind::Fullscreen,
    );

    assert!(reactor.assign_test_window_to_workspace(user_space, duplicate_wid, active_workspace));
    reactor.send_layout_event(LayoutEvent::WindowAdded(user_space, duplicate_wid));
    // The layout-event sink refuses a window that is not admitted, so the
    // non-queryable duplicate never becomes a layout ghost in the first
    // place; the restore below must still leave nothing of it behind.
    assert!(!has_window_in_layout(
        &mut reactor,
        user_space,
        frame,
        duplicate_wid
    ));
    assert!(
        reactor.create_window_data(duplicate_wid).is_none(),
        "duplicate is absent from query windows because it is not manageable"
    );

    reactor.mark_test_window_visible_in_space(duplicate_wsid, user_space);
    window_server_appeared(&mut reactor, duplicate_wsid, user_space, SpaceEventKind::User);

    assert!(
        !has_window_in_layout(&mut reactor, user_space, frame, duplicate_wid),
        "fullscreen restore must evict non-queryable duplicate layout ghosts"
    );
    assert_eq!(reactor.assigned_space_for_window_id(duplicate_wid), None);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(other_space)]));
    assert_eq!(reactor.assigned_space_for_window_id(duplicate_wid), None);
    reactor.handle_event(space_state_event(vec![frame], vec![Some(user_space)]));
    assert_eq!(reactor.assigned_space_for_window_id(duplicate_wid), None);
    assert!(
        !has_window_in_layout(&mut reactor, user_space, frame, duplicate_wid),
        "ghost must not reappear when switching back to the original space"
    );
}

#[test]
fn fullscreen_restore_uses_live_rekeyed_window_id() {
    let (mut reactor, old_wid, wsid, user_space, _other_space, frame) =
        reactor_with_window_on_space1();
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let new_wid = WindowId::new(old_wid.pid, 1999);

    window_server_appeared(&mut reactor, wsid, fullscreen_space, SpaceEventKind::Fullscreen);

    rekey_window(&mut reactor, old_wid, new_wid);

    assert!(
        reactor.state.windows.window(old_wid).is_none(),
        "rekey should retire the old AX window id before fullscreen restore"
    );

    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);

    assert!(has_window_in_layout(&mut reactor, user_space, frame, new_wid));
    assert!(!has_window_in_layout(&mut reactor, user_space, frame, old_wid));
}

#[test]
fn known_window_server_appearance_restores_layout_membership_without_reassignment() {
    let (mut reactor, wid, wsid, user_space, _other_space, frame) = reactor_with_window_on_space1();

    reactor.send_layout_event(LayoutEvent::WindowAdded(user_space, wid));
    assert!(has_window_in_layout(&mut reactor, user_space, frame, wid));

    reactor.send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(wid));

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(user_space));
    assert!(
        !has_window_in_layout(&mut reactor, user_space, frame, wid),
        "temporary removal should clear active layout membership before the appearance event"
    );

    window_server_appeared(&mut reactor, wsid, user_space, SpaceEventKind::User);

    assert!(
        has_window_in_layout(&mut reactor, user_space, frame, wid),
        "same-space appearance should heal active layout membership even when workspace assignment already matches"
    );
}

#[test]
fn discovery_preserves_hidden_windows_on_their_original_same_display_space() {
    let mut reactor = test_reactor();
    let pid = 1;
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space1)]));
    reactor.add_test_app(pid);

    let space1_workspace = reactor.test_workspace(space1, 0);
    let space2_workspace = reactor.test_workspace(space2, 0);

    let windows = [
        (WindowId::new(pid, 1), WindowServerId::new(101), space1),
        (WindowId::new(pid, 2), WindowServerId::new(102), space1),
        (WindowId::new(pid, 3), WindowServerId::new(103), space2),
    ];

    for (wid, wsid, space) in windows {
        reactor.insert_test_window(wid, wsid, Some(space), frame, true);
    }

    assert!(reactor.assign_test_window_to_workspace(
        space1,
        WindowId::new(pid, 1),
        space1_workspace
    ));
    assert!(reactor.assign_test_window_to_workspace(
        space1,
        WindowId::new(pid, 2),
        space1_workspace
    ));
    assert!(reactor.assign_test_window_to_workspace(
        space2,
        WindowId::new(pid, 3),
        space2_workspace
    ));

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space2)]));
    reactor.state.windows.clear_visible_windows();
    reactor.state.windows.mark_window_visible(WindowServerId::new(103));
    reactor.on_windows_discovered_with_app_info(pid, vec![], vec![WindowId::new(pid, 3)], None);

    let space1_workspaces = reactor.query_workspaces(Some(space1));
    let space2_workspaces = reactor.query_workspaces(Some(space2));
    let space1_count: usize = space1_workspaces.iter().map(|ws| ws.window_count).sum();
    let space2_count: usize = space2_workspaces.iter().map(|ws| ws.window_count).sum();

    assert_eq!(
        space1_count, 2,
        "inactive native space windows must stay on space1"
    );
    assert_eq!(
        space2_count, 1,
        "only the visible window should belong to space2"
    );
    assert!(reactor.test_workspace_for_window(space1, WindowId::new(pid, 1)).is_some());
    assert!(reactor.test_workspace_for_window(space1, WindowId::new(pid, 2)).is_some());
    assert!(reactor.test_workspace_for_window(space2, WindowId::new(pid, 1)).is_none());
    assert!(reactor.test_workspace_for_window(space2, WindowId::new(pid, 2)).is_none());
}

#[test]
fn forwarded_space_state_is_queued_during_mission_control_and_applied_on_exit() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let old_space = SpaceId::new(1);
    let new_space = SpaceId::new(2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(old_space)]));
    reactor.handle_event(Event::MissionControlNativeEntered);
    reactor.handle_event(space_state_event(vec![screen], vec![Some(new_space)]));

    assert_eq!(
        reactor
            .pending_space_change_manager
            .pending_space_change
            .as_ref()
            .map(|pending| pending.screens.iter().map(|screen| screen.space).collect::<Vec<_>>()),
        Some(vec![Some(new_space)])
    );

    reactor.handle_event(Event::MissionControlNativeExited);

    assert_eq!(reactor.workspace_command_space(), Some(new_space));
    assert!(reactor.pending_space_change_manager.pending_space_change.is_none());
}

#[test]
fn mission_control_exit_does_not_restore_cached_space_without_authoritative_snapshot() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let stale_space = SpaceId::new(1);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(stale_space)]));
    reactor.handle_event(Event::MissionControlNativeEntered);
    reactor.handle_event(space_state_event(vec![screen], vec![None]));
    reactor.handle_event(Event::MissionControlNativeExited);

    assert_eq!(reactor.workspace_command_space(), None);
    assert_eq!(reactor.space_state.screens[0].space, None);
}

#[test]
fn mission_control_exit_refresh_drops_windows_missing_from_origin_space_snapshot() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid: pid_t = 42;
    let moved = WindowId::new(pid, 1);
    let retained = WindowId::new(pid, 2);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, pid, make_windows(2));

    assert!(has_window_in_layout(&mut reactor, space, screen, moved));
    assert!(has_window_in_layout(&mut reactor, space, screen, retained));

    apps.windows.remove(&moved);
    let retained_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 2);
    reactor.refresh_windows_after_mission_control_with_active_windows(vec![(
        retained_wsid,
        Some(space),
    )]);
    apps.simulate_until_quiet(&mut reactor);

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, moved),
        "window moved to another native space during Mission Control should be removed from the origin layout immediately"
    );
    assert!(has_window_in_layout(&mut reactor, space, screen, retained));
}

#[test]
fn mission_control_refresh_known_visible_fallback_does_not_restore_window_moved_to_other_space() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid: pid_t = 45;
    let moved = WindowId::new(pid, 1);
    let retained = WindowId::new(pid, 2);
    let retained_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 2);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, pid, make_windows(2));

    reactor.handle_test_workspace_command(space, &LayoutCommand::CreateWorkspace);

    reactor.refresh_windows_after_mission_control_with_active_windows(vec![(
        retained_wsid,
        Some(space),
    )]);
    apps.simulate_until_quiet(&mut reactor);

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, moved),
        "known_visible fallback must not recreate a layout ghost for a window missing from the authoritative active-space snapshot"
    );

    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(0));

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, moved),
        "workspace switching must not re-project a window that Mission Control moved to another native space"
    );
    assert!(has_window_in_layout(&mut reactor, space, screen, retained));
}

#[test]
fn mission_control_enter_clears_active_drag_state() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let frame = CGRect::new(CGPoint::new(50., 50.), CGSize::new(100., 100.));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.insert_test_window_state(wid, frame, Some(WindowServerId::new(1)), true);
    reactor.ensure_active_drag(wid, &frame);

    assert!(matches!(
        reactor.drag_manager.drag_state,
        DragState::Active { .. }
    ));

    reactor.handle_event(Event::MissionControlNativeEntered);

    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));
    assert!(reactor.drag_manager.skip_layout_for_window.is_none());
}

#[test]
fn it_ignores_windows_on_disabled_spaces() {
    let (mut apps, mut reactor) = test_context();
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![None]));

    reactor.handle_events(apps.make_app(1, make_windows(1)));

    let state_before = apps.windows.clone();
    let _events = apps.simulate_events();
    assert_eq!(state_before, apps.windows, "Window should not have been moved",);

    // Make sure it doesn't choke on destroyed events for ignored windows.
    reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
    reactor.handle_event(Event::WindowCreated(
        WindowId::new(1, 2),
        make_window(2),
        None,
        Some(MouseState::Up),
    ));
    reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
}

#[test]
fn it_keeps_discovered_windows_on_their_initial_screen() {
    let (mut apps, mut reactor) = test_context();
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(SpaceId::new(1)),
        Some(SpaceId::new(2)),
    ]));

    let mut windows = make_windows(2);
    windows[1].frame.origin = CGPoint::new(1100., 100.);
    reactor.handle_events(apps.make_app(1, windows));

    let _events = apps.simulate_events();
    assert_eq!(
        screen1,
        apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
    );
    assert_eq!(
        screen2,
        apps.windows.get(&WindowId::new(1, 2)).expect("Window was not resized").frame,
    );
}

#[test]
fn it_ignores_windows_on_nonzero_layers() {
    let (mut apps, mut reactor) = test_context();
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(SpaceId::new(1))]));

    reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), None, true, false));

    let state_before = apps.windows.clone();
    let _events = apps.simulate_events();
    assert_eq!(state_before, apps.windows, "Window should not have been moved",);

    // Make sure it doesn't choke on destroyed events for ignored windows.
    reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
    reactor.handle_event(Event::WindowCreated(
        WindowId::new(1, 2),
        make_window(2),
        None,
        Some(MouseState::Up),
    ));
    reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
}

#[test]
fn handle_layout_response_groups_windows_by_app_and_screen() {
    let (mut apps, mut reactor) = test_context();
    let (raise_manager_tx, mut raise_manager_rx) = actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_manager_tx;
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(SpaceId::new(1)),
        Some(SpaceId::new(2)),
    ]));

    reactor.handle_events(apps.make_app(1, make_windows(2)));

    let mut windows = make_windows(2);
    windows[1].frame.origin = CGPoint::new(1100., 100.);
    reactor.handle_events(apps.make_app(2, windows));

    let _events = apps.simulate_events();
    while raise_manager_rx.try_recv().is_ok() {}

    reactor.handle_layout_response(
        layout::EventResponse {
            changed: true,
            raise_windows: vec![
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(2, 1),
                WindowId::new(2, 2),
            ],
            focus_window: None,
            boundary_hit: None,
        },
        None,
    );
    let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
    match msg {
        raise_manager::Event::RaiseRequest(RaiseRequest {
            raise_windows, focus_window, ..
        }) => {
            let raise_windows: HashSet<Vec<WindowId>> = raise_windows.into_iter().collect();
            let expected = [
                vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                vec![WindowId::new(2, 1)],
                vec![WindowId::new(2, 2)],
            ]
            .into_iter()
            .collect();
            assert_eq!(raise_windows, expected);
            assert!(focus_window.is_none());
        }
        _ => panic!("Unexpected event: {msg:?}"),
    }
}

#[test]
fn handle_layout_response_includes_handles_for_raise_and_focus_windows() {
    let (mut apps, mut reactor) = test_context();
    let (raise_manager_tx, mut raise_manager_rx) = actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_manager_tx;

    reactor.handle_events(apps.make_app(1, make_windows(1)));
    reactor.handle_events(apps.make_app(2, make_windows(1)));

    let _events = apps.simulate_events();
    while raise_manager_rx.try_recv().is_ok() {}
    reactor.handle_layout_response(
        layout::EventResponse {
            changed: true,
            raise_windows: vec![WindowId::new(1, 1)],
            focus_window: Some(WindowId::new(2, 1)),
            boundary_hit: None,
        },
        None,
    );
    let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
    match msg {
        raise_manager::Event::RaiseRequest(RaiseRequest { app_handles, .. }) => {
            assert!(app_handles.contains_key(&1));
            assert!(app_handles.contains_key(&2));
        }
        _ => panic!("Unexpected event: {msg:?}"),
    }
}

#[test]
fn workspace_switch_batches_all_window_positions_with_eui_enabled() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
    let _ = apps.requests();

    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(2),
    });
    apps.simulate_until_quiet(&mut reactor);
    let _ = apps.requests();

    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));

    let requests = apps.requests();
    assert!(
        requests.iter().any(|req| {
            matches!(
                req,
                Request::SetWorkspaceSwitchPositions(positions, _, true)
                    if positions.iter().any(|(wid, _)| *wid == WindowId::new(1, 1))
            )
        }),
        "expected a position-only workspace-switch batch with eui enabled: {requests:?}"
    );
}

#[test]
fn non_workspace_instant_layout_keeps_full_frame_batch() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let _ = apps.requests();

    let target = CGRect::new(CGPoint::new(25., 30.), CGSize::new(700., 650.));
    assert!(super::animation::AnimationManager::instant_layout(
        &mut reactor,
        space,
        &[(wid, target)],
        None,
    ));

    let requests = apps.requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            Request::SetBatchWindowFrame(frames, _, true)
                if frames.as_slice() == [(wid, target)]
        )),
        "ordinary instant layouts must retain full-frame writes: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !matches!(request, Request::SetWorkspaceSwitchPositions(..))),
        "the workspace-switch-only request escaped into an ordinary instant layout: {requests:?}"
    );
}

#[test]
fn workspace_switch_layout_falls_back_to_full_frames_for_size_changes() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let _ = apps.requests();

    let target = CGRect::new(CGPoint::new(25., 30.), CGSize::new(700., 650.));
    assert!(super::animation::AnimationManager::workspace_switch_layout(
        &mut reactor,
        space,
        &[(wid, target)],
        None,
    ));

    let requests = apps.requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            Request::SetBatchWindowFrame(frames, _, true)
                if frames.as_slice() == [(wid, target)]
        )),
        "workspace layouts with size changes must retain full-frame writes: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !matches!(request, Request::SetWorkspaceSwitchPositions(..))),
        "a size-changing workspace layout must not use position-only writes: {requests:?}"
    );
}

#[test]
fn topology_change_clears_stale_pending_hide_target_before_next_workspace_layout() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let _ = apps.requests();

    let wsid = reactor.test_window_server_id(wid);
    let workspaces = reactor.test_workspace_ids(space);
    let hidden_workspace = workspaces[0];
    let active_workspace = workspaces[1];

    assert!(reactor.set_test_active_workspace(space, active_workspace));
    assert!(reactor.assign_test_window_to_workspace(space, wid, hidden_workspace));

    if let Some(window) = reactor.state.windows.window_mut(wid) {
        window.frame_monotonic = CGRect::new(CGPoint::new(200.0, 200.0), CGSize::new(400.0, 400.0));
    }

    let gaps = reactor.config.settings.layout.gaps.clone();
    let hidden_target = reactor
        .layout_manager
        .layout_engine
        .calculate_layout_with_virtual_workspaces(
            &reactor.state.windows,
            space,
            screen,
            &gaps,
            0.0,
            Default::default(),
            Default::default(),
            |query_wid| {
                reactor.state.windows.window(query_wid).map(|window| window.frame_monotonic)
            },
            &[screen],
        )
        .into_iter()
        .find(|(layout_wid, _)| *layout_wid == wid)
        .map(|(_, frame)| frame)
        .expect("inactive-workspace window should still be laid out to a hidden position");

    let txid = reactor.transaction_manager.generate_next_txid(wsid);
    reactor.transaction_manager.store_txid(wsid, txid, hidden_target);

    assert!(!reactor.update_layout_or_warn(false, true, None));
    assert!(
        apps.requests().is_empty(),
        "a stale pending target suppresses the hide write before topology invalidation"
    );

    reactor.handle_event(space_state_event_with(
        vec![screen],
        vec![Some(space)],
        |state| {
            state.has_seen_display_set = true;
            state.display_set_changed = true;
            state.topology_changed = true;
        },
    ));
    let requests = apps.requests();
    assert!(
        requests.iter().any(|req| {
            matches!(req,
                Request::SetWindowFrame(req_wid, frame, _, true)
                    if *req_wid == wid && frame.same_as(hidden_target)
            ) || matches!(req,
                Request::SetBatchWindowFrame(frames, _, true)
                    if frames.iter().any(|(req_wid, frame)| *req_wid == wid && frame.same_as(hidden_target))
            )
        }),
        "topology invalidation must resend the hidden-window frame write instead of treating the stale target as still pending: {requests:?}"
    );
}

#[test]
fn auto_workspace_switch_follows_activated_window_when_same_app_is_visible_elsewhere() {
    let (mut apps, mut reactor) = test_context();
    let (raise_manager_tx, mut raise_manager_rx) = actor::channel();
    reactor.communication_manager.raise_manager_tx = raise_manager_tx;

    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let stale_focus = WindowId::new(1, 1);
    let activated = WindowId::new(2, 1);
    let same_app_visible = WindowId::new(2, 2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.handle_events(apps.make_app(1, make_windows(1)));
    apps.make_app_and_settle(&mut reactor, 2, make_windows(2));

    reactor.send_layout_event(LayoutEvent::WindowFocused(space, stale_focus));
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    apps.simulate_until_quiet(&mut reactor);

    reactor.send_layout_event(LayoutEvent::WindowFocused(space, activated));
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, stale_focus));
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(0));
    apps.simulate_until_quiet(&mut reactor);
    while raise_manager_rx.try_recv().is_ok() {}

    assert!(
        reactor.layout_manager.layout_engine.is_window_in_active_workspace(
            &reactor.state.windows,
            space,
            same_app_visible
        ),
        "another window from the activated app should remain visible on the current workspace"
    );
    reactor.handle_event(Event::ApplicationGloballyActivated(activated.pid));
    assert_eq!(reactor.main_window(), Some(activated));
    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace_idx(space),
        Some(0),
        "Carbon activation must wait for the app thread to resolve its AX focus"
    );
    let activation_requests = apps.requests();
    assert!(
        activation_requests
            .iter()
            .all(|request| !matches!(request, Request::RefreshWindowInventory(_))),
        "Carbon activation should not enumerate every AX window: {activation_requests:?}"
    );
    assert!(
        activation_requests
            .iter()
            .any(|request| matches!(request, Request::ApplicationGloballyActivated(pid) if *pid == activated.pid)),
        "Carbon activation should be reconciled on the app thread: {activation_requests:?}"
    );
    assert!(raise_manager_rx.try_recv().is_err());

    // This is the resolved event emitted by the app thread after it refreshes
    // the current main window and applies quiet-activation bookkeeping.
    reactor.handle_event(Event::ApplicationActivated(activated.pid, Quiet::No));

    let requests = apps.requests();
    assert!(
        requests.iter().any(|request| match request {
            Request::SetWindowFrame(wid, _, _, _) => *wid == activated,
            Request::SetBatchWindowFrame(frames, _, _) => {
                frames.iter().any(|(wid, _)| *wid == activated)
            }
            Request::SetWorkspaceSwitchPositions(positions, _, _) => {
                positions.iter().any(|(wid, _)| *wid == activated)
            }
            _ => false,
        }),
        "auto workspace switch should arrange the activated window immediately: {requests:?}"
    );

    let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
    match msg {
        raise_manager::Event::RaiseRequest(RaiseRequest { focus_window, focus_quiet, .. }) => {
            assert_eq!(focus_window.map(|(wid, _)| wid), Some(activated));
            assert_eq!(focus_quiet, Quiet::Yes);
        }
        _ => panic!("Unexpected event: {msg:?}"),
    }
}

#[test]
fn wake_restored_activation_does_not_switch_workspace_before_user_input() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let activated = WindowId::new(2, 1);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, 2, make_windows(2));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, activated));
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(0));
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::ApplicationGloballyActivated(activated.pid));
    reactor.handle_event(Event::ApplicationActivated(activated.pid, Quiet::No));

    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace_idx(space),
        Some(0),
        "loginwindow's restored activation must not change virtual workspaces"
    );

    // A real input event ends lifecycle suppression, so normal click/Dock
    // activation semantics continue to work after recovery.
    reactor.handle_event(Event::MouseUp);
    reactor.handle_event(Event::ApplicationActivated(activated.pid, Quiet::No));
    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace_idx(space),
        Some(1),
        "auto workspace switching should resume after explicit user input"
    );
}

#[test]
fn dock_activation_reveals_window_in_active_scrolling_workspace() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(600., 600.));
    let space = SpaceId::new(1);
    let pid = 2;
    let activated = WindowId::new(pid, 3);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, pid, make_windows(3));
    reactor.handle_test_layout_command(LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode: LayoutMode::Scrolling,
    });
    apps.simulate_until_quiet(&mut reactor);
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, WindowId::new(pid, 1)));
    apps.simulate_until_quiet(&mut reactor);
    let _ = apps.requests();

    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    let _ = apps.requests();
    reactor.handle_event(Event::ApplicationMainWindowChanged(
        pid,
        Some(activated),
        Quiet::No,
    ));

    let outcome = reactor
        .dispatch_workflow(Event::ApplicationActivated(pid, Quiet::No))
        .expect("resolved Dock activation");
    assert!(!outcome.arrange.requested);
    assert!(outcome.layout_events.is_empty());
    assert_eq!(outcome.focused_window, Some(activated));

    reactor.apply_event_outcome(outcome);
    assert_eq!(
        reactor.layout_manager.layout_engine.focused_window(),
        Some(activated)
    );
    assert!(
        !apps.requests().is_empty(),
        "revealing the activated scrolling window should write the adjusted strip layout"
    );
}

#[test]
fn carbon_activation_is_replayed_when_it_arrives_before_app_registration() {
    let (mut apps, mut reactor) = test_context();
    let pid = 7;

    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    assert!(apps.requests().is_empty());

    reactor.handle_events(apps.make_app_with_opts(
        pid,
        make_windows(1),
        Some(WindowId::new(pid, 1)),
        true,
        true,
    ));

    let requests = apps.requests();
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, Request::ApplicationGloballyActivated(request_pid) if *request_pid == pid)),
        "launching the current Carbon-frontmost app must replay activation on its app thread: {requests:?}"
    );
}

#[test]
fn duplicate_carbon_activation_is_forwarded_to_app_thread_once() {
    let (mut apps, mut reactor) = test_context();
    let pid = 7;

    reactor.handle_events(apps.make_app(pid, make_windows(1)));
    let _ = apps.requests();

    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    reactor.handle_event(Event::ApplicationGloballyActivated(pid));

    let activation_count = apps
        .requests()
        .iter()
        .filter(|request| {
            matches!(request, Request::ApplicationGloballyActivated(request_pid) if *request_pid == pid)
        })
        .count();
    assert_eq!(activation_count, 1);
}

#[test]
fn carbon_activation_is_forwarded_during_refresh_quarantine() {
    let (mut apps, mut reactor) = test_context();
    let pid = 7;

    reactor.handle_events(apps.make_app(pid, make_windows(1)));
    let _ = apps.requests();
    reactor.refresh_quarantine_manager.sleeping = true;

    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    assert!(
        apps.requests()
            .iter()
            .any(|request| matches!(request, Request::ApplicationGloballyActivated(request_pid) if *request_pid == pid))
    );
}

#[test]
fn focus_follows_mouse_emits_focus_without_explicit_arrange() {
    let reactor = test_reactor();
    let space = SpaceId::new(1);
    let window = WindowId::new(7, 1);

    let outcome = window_workflow::handle_mouse_moved_over_window(
        &reactor.app_manager,
        window_workflow::MouseMovedPayload {
            window: Some(window),
            should_sync: true,
            is_main: true,
            needs_layout_sync: true,
            active_space: Some(space),
        },
    )
    .expect("mouse focus workflow");

    assert!(!outcome.arrange.requested);
    assert!(matches!(
        outcome.layout_events.as_slice(),
        [LayoutEvent::WindowFocused(event_space, event_window)]
            if *event_space == space && *event_window == window
    ));
}

#[test]
fn resolved_activation_without_main_window_does_not_choose_arbitrary_app_window() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid = 2;

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, pid, make_windows(2));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, WindowId::new(pid, 1)));
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: None,
    });
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    reactor.handle_event(Event::ApplicationMainWindowChanged(pid, None, Quiet::No));
    reactor.handle_event(Event::ApplicationActivated(pid, Quiet::No));

    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace_idx(space),
        Some(0)
    );
}

#[test]
fn windows_discovered_does_not_reintroduce_inactive_workspace_window() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));

    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(2),
    });
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);

    reactor.discover_test_windows(1, vec![], vec![WindowId::new(1, 1), WindowId::new(1, 2)]);

    assert_eq!(reactor.test_active_workspace_windows(space), vec![
        WindowId::new(1, 2)
    ]);
}

#[test]
fn workspace_query_uses_authoritative_assignment_after_move() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    reactor.handle_test_layout_command(LayoutCommand::CreateWorkspace);
    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(wid.idx.get()),
    });
    apps.simulate_until_quiet(&mut reactor);

    let workspaces = reactor.test_workspace_ids(space);
    let ws1 = workspaces[0];
    let ws2 = workspaces[1];

    assert_eq!(reactor.test_workspace_for_window(space, wid), Some(ws2));

    let queried = reactor.query_workspaces(Some(space));
    assert_eq!(queried[0].window_count, 0);
    assert_eq!(queried[1].window_count, 1);
    assert_eq!(queried[1].windows[0].id, wid);
    assert_eq!(
        reactor.test_workspace_windows(space, ws1),
        Vec::<WindowId>::new()
    );
    assert_eq!(reactor.test_workspace_windows(space, ws2), vec![wid]);
}

#[test]
fn workspace_query_exposes_scrolling_order_for_inactive_workspace() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let w1 = WindowId::new(1, 1);
    let w2 = WindowId::new(1, 2);
    let w3 = WindowId::new(1, 3);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(3));
    reactor.handle_test_layout_command(LayoutCommand::SetWorkspaceLayout {
        workspace: None,
        mode: LayoutMode::Scrolling,
    });
    apps.simulate_until_quiet(&mut reactor);

    // The latest window is selected. Moving it left changes topology without changing
    // workspace membership/insertion order.
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Left));
    apps.simulate_until_quiet(&mut reactor);
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);

    let queried = reactor.query_workspaces(Some(space));
    let inactive = &queried[0];
    assert!(!inactive.is_active);
    assert_eq!(
        inactive.windows.iter().map(|window| window.id).collect::<Vec<_>>(),
        vec![w1, w3, w2]
    );
    assert_eq!(
        inactive
            .windows
            .iter()
            .map(|window| window.layout_position.map(|position| (position.column, position.row)))
            .collect::<Vec<_>>(),
        vec![Some((0, 0)), Some((1, 0)), Some((2, 0))]
    );
}

#[test]
fn it_preserves_layout_after_login_screen() {
    // TODO: This would be better tested with a more complete simulation.
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));

    make_active_app_with_count(&mut apps, &mut reactor, 1, 3, Some(WindowId::new(1, 1)));
    let default = test_layout(&mut reactor, space, full_screen);

    assert!(reactor.layout_manager.layout_engine.selected_window(space).is_some());
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));
    apps.simulate_until_quiet(&mut reactor);
    let modified = test_layout(&mut reactor, space, full_screen);
    assert_ne!(default, modified);

    reactor.handle_event(space_state_event(vec![CGRect::ZERO], vec![None]));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));
    simulate_login_screen_refresh(&mut apps, &mut reactor, 1);

    assert_eq!(test_layout(&mut reactor, space, full_screen), modified);
}

#[test]
fn login_screen_refresh_preserves_manual_workspace_assignment() {
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let wid1 = WindowId::new(1, 1);
    let wid2 = WindowId::new(1, 2);

    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(2), Some(wid1));

    reactor.handle_test_layout_command(LayoutCommand::MoveWindowToWorkspace {
        workspace: WorkspaceSelector::Index(1),
        follow: false,
        window_id: Some(2),
    });
    apps.simulate_until_quiet(&mut reactor);
    reactor.handle_test_layout_command(LayoutCommand::SwitchToWorkspace(1));
    apps.simulate_until_quiet(&mut reactor);

    let workspace_before = reactor
        .test_workspace_for_window(space, wid2)
        .expect("window should be assigned to workspace 2 before login refresh");
    let other_workspace_before = reactor
        .test_workspace_for_window(space, wid1)
        .expect("window should remain assigned to original workspace before login refresh");
    assert_ne!(workspace_before, other_workspace_before);
    assert_eq!(
        reactor.test_active_workspace_windows(space),
        vec![wid2],
        "switched workspace should show only the moved window before login refresh"
    );

    reactor.handle_event(space_state_event(vec![CGRect::ZERO], vec![None]));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));
    simulate_login_screen_refresh(&mut apps, &mut reactor, 1);

    assert_eq!(
        reactor.test_workspace_for_window(space, wid2),
        Some(workspace_before),
        "login refresh must preserve the moved window's workspace assignment"
    );
    assert_eq!(
        reactor.test_workspace_for_window(space, wid1),
        Some(other_workspace_before),
        "login refresh must preserve other windows' original workspace assignments"
    );
    assert_eq!(
        reactor.test_active_workspace_windows(space),
        vec![wid2],
        "active workspace contents must survive login refresh"
    );
}

#[test]
fn title_change_reapply_does_not_rebalance_unchanged_layout() {
    let (mut apps, mut reactor) = test_context();
    reactor.config.virtual_workspaces.reapply_app_rules_on_title_change = true;

    let space = SpaceId::new(1);
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));

    make_active_app_with_count(&mut apps, &mut reactor, 1, 3, Some(WindowId::new(1, 1)));

    assert!(reactor.layout_manager.layout_engine.selected_window(space).is_some());
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));
    apps.simulate_until_quiet(&mut reactor);

    let modified = test_layout(&mut reactor, space, full_screen);

    reactor.handle_event(Event::WindowTitleChanged(
        WindowId::new(1, 1),
        "Renamed window".to_string(),
    ));

    assert_eq!(test_layout(&mut reactor, space, full_screen), modified);
}

#[test]
fn title_change_reapply_does_not_rebalance_when_window_stays_floating() {
    let (mut apps, mut reactor) = test_context();
    reactor.config.virtual_workspaces.reapply_app_rules_on_title_change = true;

    let space = SpaceId::new(1);
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));

    make_active_app_with_count(&mut apps, &mut reactor, 1, 3, Some(WindowId::new(1, 1)));

    assert!(reactor.layout_manager.layout_engine.selected_window(space).is_some());
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    apps.simulate_until_quiet(&mut reactor);
    assert!(reactor.layout_manager.layout_engine.is_window_floating(WindowId::new(1, 1)));

    let modified = test_layout(&mut reactor, space, full_screen);

    reactor.handle_event(Event::WindowTitleChanged(
        WindowId::new(1, 1),
        "Renamed floating window".to_string(),
    ));

    assert!(reactor.layout_manager.layout_engine.is_window_floating(WindowId::new(1, 1)));
    assert_eq!(test_layout(&mut reactor, space, full_screen), modified);
}

#[test]
fn title_change_rule_moves_window_to_matching_workspace() {
    let settings = crate::common::config::VirtualWorkspaceSettings {
        default_workspace_count: 2,
        reapply_app_rules_on_title_change: true,
        app_rules: vec![crate::common::config::AppWorkspaceRule {
            app_id: Some("com.testapp1".into()),
            workspace: Some(WorkspaceSelector::Index(1)),
            title_substring: Some("matched title".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (mut apps, mut reactor) = (Apps::new(), test_reactor_with_workspace_settings(&settings));
    reactor.config.virtual_workspaces = settings;
    let space = SpaceId::new(1);
    let window = WindowId::new(1, 1);
    reactor.handle_event(space_state_event(
        vec![CGRect::new(CGPoint::ZERO, CGSize::new(1000., 1000.))],
        vec![Some(space)],
    ));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(1), Some(window));

    let initial = reactor.test_workspace_for_window(space, window).unwrap();
    reactor.handle_event(Event::WindowTitleChanged(window, "matched title".into()));

    assert_ne!(reactor.test_workspace_for_window(space, window), Some(initial));
    assert_eq!(
        reactor.test_workspace_for_window(space, window),
        Some(reactor.test_workspace(space, 1))
    );
}

#[test]
fn menu_open_state_is_cleared_when_owner_deactivates() {
    let mut reactor = test_reactor();
    let (event_tap_tx, mut event_tap_rx) = actor::channel();
    reactor.communication_manager.event_tap_tx = Some(event_tap_tx);

    reactor.handle_event(Event::MenuOpened(1));
    let disable = event_tap_rx.try_recv().expect("menu-open should update event tap").1;
    assert!(matches!(
        disable,
        crate::actor::event_tap::Request::SetFocusFollowsMouseEnabled(false)
    ));
    assert_eq!(reactor.menu_manager.menu_state, MenuState::Open(1));

    reactor.handle_event(Event::ApplicationDeactivated(1));
    let enable = event_tap_rx
        .try_recv()
        .expect("app deactivation should re-enable focus-follows-mouse")
        .1;
    assert!(matches!(
        enable,
        crate::actor::event_tap::Request::SetFocusFollowsMouseEnabled(true)
    ));
    assert_eq!(reactor.menu_manager.menu_state, MenuState::Closed);
}

#[test]
fn stale_menu_open_state_is_cleared_when_other_app_activates() {
    let mut reactor = test_reactor();
    let (event_tap_tx, mut event_tap_rx) = actor::channel();
    reactor.communication_manager.event_tap_tx = Some(event_tap_tx);

    reactor.handle_event(Event::MenuOpened(1));
    let _ = event_tap_rx.try_recv().expect("menu-open should update event tap");
    assert_eq!(reactor.menu_manager.menu_state, MenuState::Open(1));

    reactor.handle_event(Event::ApplicationGloballyActivated(2));
    let enable = event_tap_rx
        .try_recv()
        .expect("activation of another app should clear stale menu state")
        .1;
    assert!(matches!(
        enable,
        crate::actor::event_tap::Request::SetFocusFollowsMouseEnabled(true)
    ));
    assert_eq!(reactor.menu_manager.menu_state, MenuState::Closed);
}

#[test]
fn same_app_focus_change_hides_mouse_and_window_server_confirmation_reasserts_it() {
    let (mut apps, mut reactor) = test_context();
    let (event_tap_tx, mut event_tap_rx) = actor::channel();
    reactor.communication_manager.event_tap_tx = Some(event_tap_tx);

    let space = SpaceId::new(1);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let first = WindowId::new(1, 1);
    let second = WindowId::new(1, 2);

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, 1, make_windows(2));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, first));
    while event_tap_rx.try_recv().is_ok() {}

    reactor.send_layout_event(LayoutEvent::WindowFocused(space, second));

    let request = event_tap_rx.try_recv().expect("same-app focus change should hide mouse").1;
    assert!(matches!(request, crate::actor::event_tap::Request::HideOnFocus));

    reactor.handle_event(Event::WindowServerFocusChanged(second, space));

    let request = event_tap_rx
        .try_recv()
        .expect("WindowServer focus confirmation should reassert hidden mouse")
        .1;
    assert!(matches!(
        request,
        crate::actor::event_tap::Request::EnforceHidden
    ));
}

#[test]
fn it_retains_windows_without_server_ids_after_login_visibility_failure() {
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));

    let window = WindowInfo {
        is_standard: true,
        is_root: true,
        is_minimized: false,
        is_resizable: true,
        min_size: None,
        max_size: None,
        title: "NoServerId".to_string(),
        frame: CGRect::new(CGPoint::new(50., 50.), CGSize::new(400., 400.)),
        sys_id: None,
        bundle_id: None,
        path: None,
        ax_role: None,
        ax_subrole: None,
    };

    reactor.handle_events(apps.make_app_with_opts(
        1,
        vec![window],
        Some(WindowId::new(1, 1)),
        true,
        false,
    ));
    apps.simulate_until_quiet(&mut reactor);

    reactor.handle_event(space_state_event(vec![full_screen], vec![None]));

    // Simulate a native fullscreen transition: space temporarily becomes a fullscreen
    // space id (reactor suppresses it to None), then returns to the original space.
    let fullscreen_space = SpaceId::new(0x400000000 + space.get());
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(
        fullscreen_space,
    )]));

    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(space)]));

    loop {
        let requests = apps.requests();
        if requests.is_empty() {
            break;
        }

        let mut other_requests = Vec::new();
        for request in requests {
            match request {
                Request::RefreshWindowInventory(_) => {
                    reactor.discover_test_windows(1, vec![], vec![]);
                }
                other => other_requests.push(other),
            }
        }

        if !other_requests.is_empty() {
            let events = apps.simulate_events_for_requests(other_requests);
            for event in events {
                reactor.handle_event(event);
            }
        }
    }
}

#[test]
fn animated_layout_handles_windows_without_server_ids() {
    let (mut apps, mut reactor) = test_context();
    let space = SpaceId::new(1);
    reactor.handle_event(space_state_event(
        vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
        vec![Some(space)],
    ));

    let mut window = make_window(1);
    window.sys_id = None;
    window.frame = CGRect::new(CGPoint::new(50., 50.), CGSize::new(400., 400.));

    reactor.handle_events(apps.make_app_with_opts(
        1,
        vec![window],
        Some(WindowId::new(1, 1)),
        true,
        false,
    ));
    apps.requests();

    let target = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    assert!(super::animation::AnimationManager::animate_layout(
        &mut reactor,
        space,
        &[(WindowId::new(1, 1), target)],
        true,
        None,
    ));

    let requests = apps.requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            Request::SetWindowFrame(..) | Request::SetBatchWindowFrame(..)
        )),
        "expected layout to still request a frame update without a server id: {requests:?}"
    );
}

#[test]
fn display_index_selector_uses_physical_left_to_right_order() {
    let mut reactor = test_reactor();
    let right = CGRect::new(CGPoint::new(200000., 0.), CGSize::new(1000., 1000.));
    let left = CGRect::new(CGPoint::new(100000., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![right, left], vec![
        Some(SpaceId::new(1)),
        Some(SpaceId::new(2)),
    ]));

    let selected = reactor
        .screen_for_selector(&DisplaySelector::Index(0), None)
        .expect("expected display index 0 to resolve");

    assert_eq!(selected.frame, left);
}

#[test]
fn moving_tiled_window_to_display_applies_destination_layout_after_transfer_frame() {
    let (mut apps, mut reactor) = test_context();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(SpaceId::new(1)),
        Some(SpaceId::new(2)),
    ]));
    apps.make_app_and_settle(&mut reactor, 1, make_windows(2));

    let moved = WindowId::new(1, 1);
    reactor.handle_event(Event::Command(Command::Reactor(
        ReactorCommand::MoveWindowToDisplay {
            selector: DisplaySelector::Index(1),
            window_id: Some(1),
        },
    )));

    let writes: Vec<CGRect> = apps
        .requests()
        .into_iter()
        .flat_map(|request| match request {
            Request::SetWindowFrame(wid, frame, _, _) if wid == moved => vec![frame],
            Request::SetBatchWindowFrame(frames, _, _) => frames
                .into_iter()
                .filter_map(|(wid, frame)| (wid == moved).then_some(frame))
                .collect(),
            _ => Vec::new(),
        })
        .collect();

    assert!(
        writes.len() >= 2,
        "expected transfer and tiled writes: {writes:?}"
    );
    assert!(
        writes.last().is_some_and(|frame| frame.same_as(right)),
        "the destination layout must supply the final frame: {writes:?}"
    );
    assert!(
        !writes.first().is_some_and(|frame| frame.same_as(right)),
        "the initial transfer frame should preserve the source tile size: {writes:?}"
    );
}

#[test]
fn authoritative_active_window_snapshot_reassigns_window_across_active_displays() {
    let (mut reactor, wid, wsid, space1, space2, _initial_frame, _screen2) =
        reactor_with_window_on_space1_two_displays();

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space1));
    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space1));

    reactor.reconcile_authoritative_active_window_snapshot(vec![(wsid, Some(space2))], false);

    assert_eq!(
        reactor.state.windows.window_server_space(wsid),
        Some(space2),
        "authoritative active-space membership should update the tracked native space"
    );
    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "authoritative active-space membership should reassign the window to the new display"
    );
}

#[test]
fn authoritative_active_window_snapshot_removes_missing_window_from_active_layout() {
    let (mut apps, mut reactor) = test_context();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid: pid_t = 42;
    let moved = WindowId::new(pid, 1);
    let retained = WindowId::new(pid, 2);
    let moved_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 1);
    let retained_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(space)]));
    apps.make_app_and_settle(&mut reactor, pid, make_windows(2));

    assert!(has_window_in_layout(&mut reactor, space, frame, moved));
    assert!(has_window_in_layout(&mut reactor, space, frame, retained));
    reactor.mark_test_window_visible_in_space(moved_wsid, space);
    reactor.mark_test_window_visible_in_space(retained_wsid, space);
    reactor
        .reconcile_authoritative_active_window_snapshot(vec![(retained_wsid, Some(space))], false);

    assert!(
        !has_window_in_layout(&mut reactor, space, frame, moved),
        "active-space window missing from the authoritative snapshot must be removed immediately"
    );
    assert!(
        !reactor.state.windows.is_window_visible(moved_wsid),
        "authoritative snapshot reconcile should clear visible state for missing windows"
    );
    assert!(has_window_in_layout(&mut reactor, space, frame, retained));
}

#[test]
fn authoritative_active_window_snapshot_reassigns_missing_window_to_inactive_space() {
    let (mut apps, mut reactor) = test_context();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let active_space = SpaceId::new(1);
    let inactive_space = SpaceId::new(2);
    let pid: pid_t = 43;
    let moved = WindowId::new(pid, 1);
    let retained = WindowId::new(pid, 2);
    let moved_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 1);
    let retained_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(active_space)]));
    apps.make_app_and_settle(&mut reactor, pid, make_windows(2));

    reactor.mark_test_window_visible_in_space(moved_wsid, active_space);
    reactor.mark_test_window_visible_in_space(retained_wsid, active_space);
    crate::sys::window_server::set_window_spaces_override(
        moved_wsid,
        Some(vec![inactive_space.get()]),
    );

    reactor.reconcile_authoritative_active_window_snapshot(
        vec![(retained_wsid, Some(active_space))],
        false,
    );

    crate::sys::window_server::set_window_spaces_override(moved_wsid, None);

    assert_eq!(
        reactor.assigned_space_for_window_id(moved),
        Some(inactive_space),
        "missing active-space windows should migrate to their actual inactive native space"
    );
    assert!(
        reactor.test_workspace_for_window(active_space, moved).is_none(),
        "window should no longer belong to the old active native space"
    );
    assert!(
        reactor.test_workspace_for_window(inactive_space, moved).is_some(),
        "window should now belong to the inactive native space that WindowServer reports"
    );
    assert!(
        !has_window_in_layout(&mut reactor, active_space, frame, moved),
        "window moved onto an inactive native space must be removed from the active layout"
    );
    assert!(has_window_in_layout(&mut reactor, active_space, frame, retained));
    assert_eq!(
        reactor.assigned_space_for_window_id(retained),
        Some(active_space),
        "other visible windows on the active space must remain untouched"
    );
}

#[test]
fn topology_window_delta_reassigns_missing_window_to_inactive_space() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(3);
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let active_space = SpaceId::new(1);
    let inactive_space = SpaceId::new(2);
    let pid: pid_t = 44;
    let moved = WindowId::new(pid, 1);
    let retained = WindowId::new(pid, 2);
    let moved_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 1);
    let retained_wsid = WindowServerId::new((pid as u32).saturating_mul(10_000) + 2);

    reactor.handle_event(space_state_event(vec![frame], vec![Some(active_space)]));
    apps.make_app_and_settle(&mut reactor, pid, make_windows(2));

    let preserved_workspace = reactor.test_workspace(active_space, 2);
    let expected_destination_workspace = reactor.test_workspace(inactive_space, 2);
    reactor.send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(moved));
    assert!(reactor.assign_test_window_to_workspace(active_space, moved, preserved_workspace));
    reactor.handle_test_workspace_command(active_space, &LayoutCommand::SwitchToWorkspace(2));
    reactor.send_layout_event(LayoutEvent::WindowAdded(active_space, moved));
    reactor.handle_test_workspace_command(active_space, &LayoutCommand::SwitchToWorkspace(0));

    reactor.mark_test_window_visible_in_space(moved_wsid, active_space);
    reactor.mark_test_window_visible_in_space(retained_wsid, active_space);
    crate::sys::window_server::set_window_spaces_override(
        moved_wsid,
        Some(vec![inactive_space.get()]),
    );
    crate::sys::window_server::set_space_window_list_for_space_override(
        active_space.get(),
        Some(vec![retained_wsid.as_u32()]),
    );

    reactor.handle_event(space_state_event_with(
        vec![frame],
        vec![Some(active_space)],
        |state| {
            state.has_seen_display_set = true;
            state.topology_window_delta = Some(crate::actor::spaces::TopologyWindowDelta {
                epoch: 11,
                flags: crate::sys::skylight::DisplayReconfigFlags::MOVED,
                appeared: Vec::new(),
                disappeared: vec![(moved_wsid, active_space)],
            });
        },
    ));

    crate::sys::window_server::set_window_spaces_override(moved_wsid, None);
    crate::sys::window_server::set_space_window_list_for_space_override(active_space.get(), None);

    assert_eq!(reactor.assigned_space_for_window_id(moved), Some(inactive_space));
    assert!(reactor.test_workspace_for_window(active_space, moved).is_none());
    assert_eq!(
        reactor.test_workspace_for_window(inactive_space, moved),
        Some(expected_destination_workspace)
    );
    assert!(!has_window_in_layout(&mut reactor, active_space, frame, moved));
    assert!(has_window_in_layout(&mut reactor, active_space, frame, retained));
}

#[test]
fn topology_window_delta_is_not_ignored_by_command_space_only_short_circuit() {
    let (mut reactor, wid, wsid, space1, space2, _initial_frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));

    crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![space2.get()]));
    crate::sys::window_server::set_space_window_list_for_space_override(space1.get(), Some(vec![]));
    crate::sys::window_server::set_space_window_list_for_space_override(
        space2.get(),
        Some(vec![wsid.as_u32()]),
    );

    reactor.handle_event(space_state_event_with(
        vec![screen1, screen2],
        vec![Some(space1), Some(space2)],
        |state| {
            state.has_seen_display_set = true;
            state.topology_window_delta = Some(crate::actor::spaces::TopologyWindowDelta {
                epoch: 12,
                flags: crate::sys::skylight::DisplayReconfigFlags::MOVED,
                appeared: vec![(wsid, space2)],
                disappeared: vec![(wsid, space1)],
            });
        },
    ));

    crate::sys::window_server::set_window_spaces_override(wsid, None);
    crate::sys::window_server::set_space_window_list_for_space_override(space1.get(), None);
    crate::sys::window_server::set_space_window_list_for_space_override(space2.get(), None);

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "topology delta should still be processed even when the forwarded screens snapshot is unchanged"
    );
    assert_eq!(reactor.state.windows.window_server_space(wsid), Some(space2));
}

/// During a modifier (alt-drag) resize rift writes the window's frame from
/// the pointer, and the app echoes each write with the button down. Those
/// echoes are not the user dragging the window: holding it on their account
/// left it out of every arrange while its neighbours followed the pointer.
#[test]
fn modifier_drag_echoes_do_not_hold_the_window() {
    let (mut reactor, wid, wsid, space1, _space2, initial_frame, _screen2) =
        reactor_with_window_on_space1_two_displays();
    reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space1, wid));

    reactor.handle_event(Event::MouseModifierDragBegin {
        window: wsid,
        at: CGPoint::new(
            initial_frame.origin.x + initial_frame.size.width - 5.0,
            initial_frame.origin.y + initial_frame.size.height / 2.0,
        ),
        action: crate::common::config::MouseAction::Resize,
    });
    reactor.handle_event(Event::MouseModifierDrag { dx: -40.0, dy: 0.0 });

    let mut echoed = initial_frame;
    echoed.size.width -= 40.0;
    let txid = reactor.transaction_manager.get_last_sent_txid(wsid);
    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        echoed,
        Some(txid),
        Requested(false),
        Some(crate::sys::event::MouseState::Down),
    ));

    assert_eq!(
        reactor.window_in_drag(),
        None,
        "an echo of rift's own resize is not the user dragging the window"
    );
    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));

    reactor.handle_event(Event::MouseUp);
    assert!(
        reactor.modifier_drag.is_none(),
        "the modifier drag ends with the button"
    );
}

/// The window server hands a dragged window to the display under the pointer
/// before the app reports the first frame with the button down. Acting on the
/// space change re-tiled the window on the other display mid-drag: it has to
/// wait for the drop.
#[test]
fn window_that_changes_space_with_the_button_down_is_held_until_the_drop() {
    let (mut reactor, wid, wsid, space1, space2, _initial_frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));

    crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![space2.get()]));
    crate::sys::window_server::set_space_window_list_for_space_override(space1.get(), Some(vec![]));
    crate::sys::window_server::set_space_window_list_for_space_override(
        space2.get(),
        Some(vec![wsid.as_u32()]),
    );
    crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Down));

    reactor.handle_event(space_state_event_with(
        vec![screen1, screen2],
        vec![Some(space1), Some(space2)],
        |state| {
            state.has_seen_display_set = true;
            state.topology_window_delta = Some(crate::actor::spaces::TopologyWindowDelta {
                epoch: 12,
                flags: crate::sys::skylight::DisplayReconfigFlags::MOVED,
                appeared: vec![(wsid, space2)],
                disappeared: vec![(wsid, space1)],
            });
        },
    ));

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space1),
        "a window in the user's hand keeps its tree until the drop"
    );
    assert_eq!(
        reactor.window_in_drag(),
        Some(wid),
        "the window is held for the drag"
    );

    // The button comes up without the app ever having reported the drag:
    // the window is where the window server has it.
    crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Up));
    reactor.handle_event(Event::MouseUp);

    crate::sys::event::set_mouse_state_override(None);
    crate::sys::window_server::set_window_spaces_override(wsid, None);
    crate::sys::window_server::set_space_window_list_for_space_override(space1.get(), None);
    crate::sys::window_server::set_space_window_list_for_space_override(space2.get(), None);

    assert_eq!(reactor.window_in_drag(), None);
    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(space2),
        "the drop settles the window where the window server has it"
    );
}

#[test]
fn forwarded_space_state_does_not_clear_existing_fullscreen_tracks_when_snapshot_has_none() {
    let mut reactor = test_reactor();
    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let tracked_user_space = SpaceId::new(1);
    let current_space = SpaceId::new(2);
    let fullscreen_space = SpaceId::new(0x400000001);
    let window_id = WindowId::new(42, 1);

    let tracked_workspace = reactor.test_workspace(tracked_user_space, 0);
    assert!(reactor.assign_test_window_to_workspace(
        tracked_user_space,
        window_id,
        tracked_workspace
    ));
    let _ = reactor.state.windows.suspend_window_to_native_fullscreen(
        window_id,
        Some(WindowServerId::new(1)),
        Some(tracked_user_space),
        fullscreen_space,
        NativeFullscreenTransition::Suspended,
    );

    reactor.handle_event(space_state_event_with(
        vec![frame],
        vec![Some(current_space)],
        |state| state.has_seen_display_set = true,
    ));

    assert!(
        reactor
            .state
            .windows
            .native_fullscreen_record_for_window(window_id)
            .is_some_and(|record| record.fullscreen_space == fullscreen_space),
        "empty forwarded fullscreen state must not clear existing fullscreen exit tracking"
    );
}

#[test]
fn non_active_workspace_windows_remain_hidden_even_if_frame_no_longer_matches_corner_geometry() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    let wsid = reactor.test_window_server_id(wid);
    let workspaces = reactor.test_workspace_ids(space);
    let inactive_workspace = workspaces[0];
    let active_workspace = workspaces[1];

    assert!(reactor.set_test_active_workspace(space, active_workspace));
    assert!(reactor.assign_test_window_to_workspace(space, wid, inactive_workspace));

    if let Some(window) = reactor.state.windows.window_mut(wid) {
        window.frame_monotonic = CGRect::new(CGPoint::new(200.0, 200.0), CGSize::new(400.0, 400.0));
    }

    assert_eq!(
        reactor.hidden_assigned_space_for_window_id(wid),
        Some(space),
        "workspace-hidden status should follow Rift's workspace assignment, not stale corner geometry"
    );
    assert_eq!(
        reactor.geometry_space_for_window(
            &CGRect::new(CGPoint::new(200.0, 200.0), CGSize::new(400.0, 400.0)),
            Some(wsid),
        ),
        Some(space),
        "topology changes can leave hidden windows at stale coordinates; they must still resolve to their assigned space"
    );
}

#[test]
fn display_churn_quarantines_window_frame_and_membership_events() {
    let reactor = test_reactor();
    let space = SpaceId::new(7);
    let wsid = WindowServerId::new(77);
    let _ = crate::sys::display_churn::begin(crate::sys::skylight::DisplayReconfigFlags::ADD);

    let frame_changed = reactor.should_quarantine_during_display_churn(&Event::WindowFrameChanged(
        WindowId::new(99, 1),
        CGRect::new(CGPoint::new(10., 10.), CGSize::new(500., 400.)),
        None,
        Requested(false),
        Some(MouseState::Up),
    ));
    let appeared = reactor.should_quarantine_during_display_churn(&Event::WindowServerAppeared(
        wsid,
        space,
        SpaceEventKind::User,
    ));
    let destroyed = reactor.should_quarantine_during_display_churn(&Event::WindowServerDestroyed(
        wsid,
        space,
        SpaceEventKind::User,
    ));
    let ax_invalidated = reactor
        .should_quarantine_during_display_churn(&Event::WindowDestroyed(WindowId::new(99, 77)));
    let space_created = reactor.should_quarantine_during_display_churn(&Event::SpaceCreated(space));
    let space_destroyed =
        reactor.should_quarantine_during_display_churn(&Event::SpaceDestroyed(space));

    let _ = crate::sys::display_churn::end();
    assert!(
        frame_changed,
        "WindowFrameChanged should be quarantined during churn"
    );
    assert!(
        appeared,
        "WindowServerAppeared should be quarantined during churn"
    );
    assert!(
        destroyed,
        "WindowServerDestroyed should be quarantined during churn"
    );
    assert!(
        ax_invalidated,
        "AX invalidation must be quarantined during display churn"
    );
    assert!(space_created, "SpaceCreated should be quarantined during churn");
    assert!(
        space_destroyed,
        "SpaceDestroyed should be quarantined during churn"
    );
}

#[test]
fn lifecycle_events_are_quarantined_during_sleep_and_session_inactivity() {
    let mut reactor = test_reactor();
    let space = SpaceId::new(8);

    reactor.refresh_quarantine_manager.sleeping = true;
    assert!(reactor.should_quarantine_space_lifecycle_event(&Event::SpaceCreated(space)));

    reactor.refresh_quarantine_manager.sleeping = false;
    reactor.refresh_quarantine_manager.session_inactive = true;
    assert!(reactor.should_quarantine_space_lifecycle_event(&Event::SpaceDestroyed(space)));
}

#[test]
fn normal_macos_space_switch_does_not_arm_topology_relayout() {
    let mut reactor = test_reactor();

    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1280., 800.));
    let right = CGRect::new(CGPoint::new(1280., 0.), CGSize::new(1280., 800.));

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(SpaceId::new(11)),
        Some(SpaceId::new(22)),
    ]));
    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(SpaceId::new(111)),
        Some(SpaceId::new(222)),
    ]));
    assert_eq!(
        reactor.raw_spaces_for_current_screens(),
        vec![Some(SpaceId::new(111)), Some(SpaceId::new(222))],
        "Screen state should still advance to the newly active macOS spaces"
    );
    assert!(reactor.is_space_active(SpaceId::new(111)));
    assert!(reactor.is_space_active(SpaceId::new(222)));
}

#[test]
fn fullscreen_space_in_screen_params_does_not_trigger_topology_relayout() {
    let mut reactor = test_reactor();

    let frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1280., 800.));
    let user_space = SpaceId::new(11);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let display_uuid = "11111111-1111-1111-1111-111111111111".to_string();
    let screens_for = |space: SpaceId| -> Vec<ScreenInfo> {
        vec![ScreenInfo {
            id: crate::sys::screen::ScreenId::new(0),
            frame,
            space: Some(space),
            display_uuid: display_uuid.clone(),
            name: None,
        }]
    };

    reactor.handle_event(space_state_event_from_screens(screens_for(user_space)));
    assert_eq!(
        reactor.layout_manager.layout_engine.last_space_for_display_uuid(&display_uuid),
        Some(user_space)
    );

    reactor.space_state.fullscreen_spaces.insert(fullscreen_space);
    reactor.handle_event(space_state_event_from_screens(
        screens_for(user_space)
            .into_iter()
            .map(|mut screen| {
                screen.space = None;
                screen
            })
            .collect(),
    ));
    assert_eq!(
        reactor.layout_manager.layout_engine.last_space_for_display_uuid(&display_uuid),
        Some(user_space),
        "fullscreen spaces should not replace display->user-space history"
    );

    reactor.handle_event(space_state_event_from_screens(screens_for(user_space)));
    assert_eq!(
        reactor.layout_manager.layout_engine.last_space_for_display_uuid(&display_uuid),
        Some(user_space)
    );
}

#[test]
fn fullscreen_transition_preserves_other_display_space() {
    let mut reactor = test_reactor();

    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let left_space_2 = SpaceId::new(12);
    let right_space_1 = SpaceId::new(21);
    let right_fullscreen = SpaceId::new(0x400000000 + right_space_1.get());

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space_2),
        Some(right_space_1),
    ]));
    reactor.space_state.fullscreen_spaces.insert(right_fullscreen);

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space_2),
        None,
    ]));

    assert_eq!(
        reactor.raw_spaces_for_current_screens(),
        vec![Some(left_space_2), None],
        "fullscreen transitions on one display must not accept a transient user-space change on another display"
    );
}

#[test]
fn user_space_switch_is_allowed_while_other_display_already_fullscreen() {
    let mut reactor = test_reactor();

    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let left_space_2 = SpaceId::new(12);
    let left_space_1 = SpaceId::new(11);
    let right_space_1 = SpaceId::new(21);
    let right_fullscreen = SpaceId::new(0x400000000 + right_space_1.get());

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space_2),
        Some(right_space_1),
    ]));
    reactor.space_state.fullscreen_spaces.insert(right_fullscreen);
    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space_2),
        None,
    ]));

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(left_space_1),
        None,
    ]));

    assert_eq!(
        reactor.raw_spaces_for_current_screens(),
        vec![Some(left_space_1), None],
        "Once another display is already fullscreen, user space switches on this display should still be accepted"
    );
}

#[test]
fn fullscreen_screen_params_preserves_window_layout() {
    // Regression test for #308: waking from sleep while a fullscreen video is
    // active should not wipe workspace assignments.
    let (mut apps, mut reactor) = test_context();

    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

    // Set up a display with a user space and some windows.
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(user_space)]));
    make_active_app_with_count(&mut apps, &mut reactor, 1, 3, Some(WindowId::new(1, 1)));

    // Rearrange layout so we can detect if it gets reset.
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));
    apps.simulate_until_quiet(&mut reactor);
    let layout_before = test_layout(&mut reactor, user_space, full_screen);

    // Simulate sleep/wake while fullscreen: ScreenParametersChanged arrives
    // with the fullscreen space id.
    reactor.space_state.fullscreen_spaces.insert(fullscreen_space);
    reactor.handle_event(space_state_event_from_screens(vec![ScreenInfo {
        id: crate::sys::screen::ScreenId::new(0),
        frame: full_screen,
        space: None,
        display_uuid: "test-display-0".to_string(),
        name: None,
    }]));
    apps.simulate_until_quiet(&mut reactor);

    // The fullscreen space must not become the active space for the screen.
    assert_eq!(
        reactor.space_state.screens[0].space, None,
        "fullscreen space should be nulled out, not stored as screen space"
    );

    // Return to user space (simulates exiting fullscreen).
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(user_space)]));
    apps.simulate_until_quiet(&mut reactor);

    let layout_after = test_layout(&mut reactor, user_space, full_screen);
    assert_eq!(
        layout_before, layout_after,
        "Window layout on user space must be preserved across fullscreen ScreenParametersChanged"
    );
}

fn fullscreen_startup_fixture(
    with_app_rule: bool,
    preserve_workspace: bool,
) -> (
    Reactor,
    WindowId,
    SpaceId,
    crate::model::virtual_workspace::VirtualWorkspaceId,
    crate::model::virtual_workspace::VirtualWorkspaceId,
) {
    let mut workspace_cfg = crate::common::config::VirtualWorkspaceSettings {
        default_workspace_count: 2,
        ..crate::common::config::VirtualWorkspaceSettings::default()
    };
    if with_app_rule {
        workspace_cfg.app_rules = vec![crate::common::config::AppWorkspaceRule {
            app_id: Some("com.testapp1".to_string()),
            workspace: Some(crate::common::config::WorkspaceSelector::Index(1)),
            floating: false,
            position: None,
            size: None,
            focus: false,
            manage: Some(true),
            app_name: None,
            title_regex: None,
            title_substring: None,
            ax_role: None,
            ax_subrole: None,
        }];
    }

    let mut reactor = test_reactor_with_workspace_settings(&workspace_cfg);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let pid = 1;
    let wid = WindowId::new(pid, 1);
    let wsid = WindowServerId::new(10_001);
    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());

    reactor.handle_event(fullscreen_startup_space_state(
        screen,
        "test-display-0".to_string(),
        user_space,
        fullscreen_space,
    ));
    reactor.add_test_app_with_info(pid, "com.testapp1", "TestApp1");

    let workspaces = reactor.test_workspace_ids(user_space);
    let default_workspace = workspaces[0];
    let secondary_workspace = workspaces[1];
    if preserve_workspace {
        assert!(reactor.assign_test_window_to_workspace(user_space, wid, secondary_workspace));
    }

    reactor.track_test_window_server_info(wsid, pid, screen);
    reactor.state.windows.set_window_server_space(wsid, Some(user_space));
    reactor.discover_test_windows(
        pid,
        vec![(
            wid,
            make_window_info(screen, Some(wsid), "Window", Some("com.testapp1")),
        )],
        vec![wid],
    );

    (reactor, wid, user_space, default_workspace, secondary_workspace)
}

fn rekey_window(reactor: &mut Reactor, old_wid: WindowId, new_wid: WindowId) {
    let old_info = reactor
        .state
        .windows
        .window(old_wid)
        .expect("old window should exist before rekey")
        .info
        .clone();
    reactor.discover_test_windows(
        old_wid.pid,
        vec![(new_wid, WindowInfo {
            sys_id: old_info.sys_id,
            ..old_info
        })],
        vec![new_wid],
    );
}

#[test]
fn fullscreen_startup_applies_app_rules_to_hidden_user_space_windows() {
    let (reactor, wid, user_space, _default_workspace, target_workspace) =
        fullscreen_startup_fixture(true, false);

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(user_space));
    assert_eq!(
        reactor.test_workspace_for_window(user_space, wid),
        Some(target_workspace),
        "fullscreen startup should still apply app rules to the hidden user-space window"
    );
}

#[test]
fn fullscreen_startup_discovery_preserves_existing_hidden_assignment_without_app_rules() {
    let (reactor, wid, user_space, default_workspace, secondary_workspace) =
        fullscreen_startup_fixture(false, true);

    assert_ne!(secondary_workspace, default_workspace);
    assert_eq!(
        reactor.test_workspace_for_window(user_space, wid),
        Some(secondary_workspace),
        "fullscreen startup discovery must preserve the existing hidden assignment instead of defaulting it"
    );
}

// Helper: check whether any window owned by `pid` appears in the layout tree for `space`.
fn has_window_in_layout(
    reactor: &mut Reactor,
    space: SpaceId,
    screen: CGRect,
    wid: WindowId,
) -> bool {
    let gaps = reactor.config.settings.layout.gaps.clone();
    reactor
        .layout_manager
        .layout_engine
        .calculate_layout(space, screen, &gaps, 0.0, Default::default(), Default::default())
        .iter()
        .any(|(layout_wid, _)| *layout_wid == wid)
}

fn test_layout(reactor: &mut Reactor, space: SpaceId, screen: CGRect) -> Vec<(WindowId, CGRect)> {
    let gaps = reactor.config.settings.layout.gaps.clone();
    reactor.layout_manager.layout_engine.calculate_layout(
        space,
        screen,
        &gaps,
        0.0,
        crate::common::config::HorizontalPlacement::Top,
        crate::common::config::VerticalPlacement::Right,
    )
}

fn make_active_app(
    apps: &mut Apps,
    reactor: &mut Reactor,
    pid: pid_t,
    windows: Vec<WindowInfo>,
    main_window: Option<WindowId>,
) {
    reactor.handle_events(apps.make_app_with_opts(pid, windows, main_window, true, true));
    reactor.handle_event(Event::ApplicationGloballyActivated(pid));
    apps.simulate_until_quiet(reactor);
}

fn make_active_app_with_count(
    apps: &mut Apps,
    reactor: &mut Reactor,
    pid: pid_t,
    window_count: usize,
    main_window: Option<WindowId>,
) {
    make_active_app(apps, reactor, pid, make_windows(window_count), main_window);
}

fn simulate_login_screen_refresh(apps: &mut Apps, reactor: &mut Reactor, pid: pid_t) {
    for request in apps.requests() {
        match request {
            Request::RefreshWindowInventory(_) => {
                reactor.discover_test_windows(pid, vec![], vec![])
            }
            request => {
                for event in apps.simulate_events_for_requests(vec![request]) {
                    reactor.handle_event(event);
                }
            }
        }
    }
    apps.simulate_until_quiet(reactor);
}

#[test]
fn discovery_minimize_transition_removes_window_from_layout() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    assert!(has_window_in_layout(&mut reactor, space, screen, wid));

    reactor.discover_test_windows(
        1,
        vec![(wid, WindowInfo {
            is_minimized: true,
            ..make_window(1)
        })],
        vec![],
    );

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, wid),
        "minimized window must be removed from layout when discovery reports it minimized"
    );
    assert!(
        reactor.state.windows.window(wid).is_some_and(|window| window.info.is_minimized),
        "reactor state must keep the window marked minimized"
    );
}

#[test]
fn discovery_restore_transition_readds_window_to_layout() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let mut windows = make_windows(1);
    windows[0].is_minimized = true;

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, windows);

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, wid),
        "startup-minimized window must not be inserted into layout"
    );

    reactor.discover_test_windows(1, vec![(wid, make_window(1))], vec![wid]);

    assert!(
        has_window_in_layout(&mut reactor, space, screen, wid),
        "restored window must return to layout when discovery reports it visible again"
    );
    assert!(
        reactor
            .state
            .windows
            .window(wid)
            .is_some_and(|window| !window.info.is_minimized),
        "reactor state must clear the minimized flag after restore"
    );
}

#[test]
fn discovery_manageability_loss_removes_window_from_layout() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    assert!(has_window_in_layout(&mut reactor, space, screen, wid));

    reactor.discover_test_windows(
        1,
        vec![(wid, WindowInfo {
            is_root: false,
            ..make_window(1)
        })],
        vec![wid],
    );

    assert!(
        !has_window_in_layout(&mut reactor, space, screen, wid),
        "window must be removed from layout when discovery marks it unmanageable"
    );
    assert!(
        reactor.state.windows.window(wid).is_some_and(|window| !window.is_manageable),
        "reactor state must keep the window marked unmanageable"
    );
}

#[test]
fn unfullscreen_restores_window_tracking() {
    let (mut apps, mut reactor) = test_context();

    let user_space = SpaceId::new(1);
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

    // Set up a display with a user space and some windows.
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(user_space)]));
    make_active_app_with_count(&mut apps, &mut reactor, 1, 1, Some(WindowId::new(1, 1)));

    // Record the window as fullscreened.
    let window_id = WindowId::new(1, 1);
    let _ = reactor.state.windows.suspend_window_to_native_fullscreen(
        window_id,
        Some(WindowServerId::new(1)),
        Some(user_space),
        fullscreen_space,
        NativeFullscreenTransition::Suspended,
    );

    // Transition to fullscreen space.
    reactor.handle_event(space_state_event(vec![full_screen], vec![None]));
    apps.simulate_until_quiet(&mut reactor);

    // Exit fullscreen (return to user space).
    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(user_space)]));

    // The reactor should trigger a window inventory request.
    let mut saw_get_visible_windows = false;
    for request in apps.requests() {
        if matches!(request, Request::RefreshWindowInventory(_)) {
            saw_get_visible_windows = true;
        }
    }
    assert!(
        saw_get_visible_windows,
        "Should send window inventory to app on unfullscreen"
    );

    // The fullscreen track should be removed.
    assert!(
        reactor.state.windows.native_fullscreen_record_for_window(window_id).is_none(),
        "Fullscreen track should be removed from space manager"
    );
}

#[test]
fn fullscreen_exit_space_restore_does_not_revive_stale_pre_rekey_window() {
    let (mut reactor, old_wid, wsid, user_space, _other_space, full_screen) =
        reactor_with_window_on_space1();
    let fullscreen_space = SpaceId::new(0x400000000 + user_space.get());
    let new_wid = WindowId::new(old_wid.pid, 99);

    reactor.send_layout_event(LayoutEvent::WindowAdded(user_space, old_wid));
    assert!(has_window_in_layout(
        &mut reactor,
        user_space,
        full_screen,
        old_wid
    ));

    reactor.space_state.fullscreen_spaces.insert(fullscreen_space);
    let _ = reactor.state.windows.suspend_window_to_native_fullscreen(
        old_wid,
        Some(wsid),
        Some(user_space),
        fullscreen_space,
        NativeFullscreenTransition::Suspended,
    );
    reactor.send_layout_event(LayoutEvent::WindowRemovedPreserveFloating(old_wid));

    rekey_window(&mut reactor, old_wid, new_wid);
    assert!(
        reactor.state.windows.window(old_wid).is_none(),
        "rekey should retire the old AX id before the fullscreen exit snapshot arrives"
    );

    reactor.handle_event(space_state_event(vec![full_screen], vec![Some(user_space)]));

    assert!(
        !has_window_in_layout(&mut reactor, user_space, full_screen, old_wid),
        "fullscreen exit must not recreate a stale layout-only ghost for the old AX window id"
    );
}

#[test]
fn display_churn_snapshot_ack_triggers_visible_window_refresh() {
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let (mut apps, mut reactor) = test_context();

    reactor.handle_event(space_state_event(vec![screen], vec![Some(SpaceId::new(1))]));
    apps.make_app_and_settle(&mut reactor, 1, make_windows(1));

    reactor.handle_event(Event::DisplayChurnBegin);
    let Event::SpaceStateChanged(mut snapshot) =
        space_state_event(vec![screen], vec![Some(SpaceId::new(1))])
    else {
        unreachable!("space_state_event must produce a space-state event");
    };
    snapshot.releases_display_churn_refresh_quarantine = true;
    reactor.handle_event(Event::SpaceStateChanged(snapshot));

    assert!(
        apps.requests()
            .into_iter()
            .any(|request| matches!(request, Request::RefreshWindowInventory(_))),
        "the snapshot acknowledgement should release churn and request visible windows"
    );
}

#[test]
fn display_churn_end_refresh_is_idempotent_without_topology_change() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    assert!(has_window_in_layout(&mut reactor, space, screen, wid));

    reactor.handle_event(Event::DisplayChurnEnd);
    apps.simulate_until_quiet(&mut reactor);

    assert!(
        has_window_in_layout(&mut reactor, space, screen, wid),
        "recovery refresh should preserve existing workspace membership when topology is unchanged"
    );
    assert!(
        apps.requests().is_empty(),
        "idempotent churn-end refresh should not trigger follow-up frame writes when nothing moved"
    );
}

#[test]
fn display_churn_end_refresh_preserves_non_default_workspace_without_app_rules() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    let workspaces = reactor.test_workspace_ids(space);
    let default_workspace = workspaces[0];
    let secondary_workspace = workspaces[1];

    assert!(reactor.assign_test_window_to_workspace(space, wid, secondary_workspace));
    assert!(reactor.set_test_active_workspace(space, secondary_workspace));
    reactor.discover_test_windows(1, vec![], vec![wid]);

    assert_eq!(
        reactor.test_workspace_for_window(space, wid),
        Some(secondary_workspace)
    );
    assert_ne!(secondary_workspace, default_workspace);
    assert!(has_window_in_layout(&mut reactor, space, screen, wid));

    reactor.handle_event(Event::DisplayChurnEnd);
    apps.simulate_until_quiet(&mut reactor);

    assert_eq!(
        reactor.test_workspace_for_window(space, wid),
        Some(secondary_workspace),
        "visibility refresh must preserve an existing non-default assignment when no app rule matches"
    );
    assert_eq!(
        reactor.layout_manager.layout_engine.active_workspace(space),
        Some(secondary_workspace),
        "refresh must not switch the active workspace back to default"
    );
    assert!(
        has_window_in_layout(&mut reactor, space, screen, wid),
        "window should remain in the visible layout of its non-default workspace after refresh"
    );
}

#[test]
fn session_gate_ignores_discovery_and_replays_one_refresh_after_unlock() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    let workspaces = reactor.test_workspace_ids(space);
    let secondary_workspace = workspaces[1];

    assert!(reactor.assign_test_window_to_workspace(space, wid, secondary_workspace));
    assert!(reactor.set_test_active_workspace(space, secondary_workspace));

    assert!(apps.requests().is_empty());

    reactor.handle_event(Event::SessionDidResignActive);
    reactor.discover_test_windows(1, vec![], vec![]);
    reactor.handle_event(Event::ApplicationGloballyActivated(1));

    let requests = apps.requests();
    assert!(
        requests
            .iter()
            .all(|request| !matches!(request, Request::RefreshWindowInventory(_))),
        "locked-session discovery should defer visible-window enumeration: {requests:?}"
    );
    assert!(
        requests.iter().any(
            |request| matches!(request, Request::ApplicationGloballyActivated(pid) if *pid == 1)
        ),
        "Carbon activation should still be reconciled by the app thread: {requests:?}"
    );
    assert_eq!(
        reactor.test_workspace_for_window(space, wid),
        Some(secondary_workspace),
        "ignored lock-session discovery must not reassign the window back to the default workspace"
    );

    reactor.handle_event(Event::SessionDidBecomeActive);
    assert!(
        apps.requests().is_empty(),
        "unlock should stay quarantined until the spaces actor publishes a fresh post-unlock snapshot"
    );
    let stale_snapshot = space_state_event(vec![screen], vec![Some(space)]);
    reactor.handle_event(stale_snapshot);
    assert!(
        apps.requests().is_empty(),
        "an older queued WM snapshot must not release the unlock quarantine"
    );

    let fresh_snapshot = space_state_event_with(vec![screen], vec![Some(space)], |state| {
        state.releases_lifecycle_refresh_quarantine = true
    });
    reactor.handle_event(fresh_snapshot);

    let requests = apps.requests();
    assert_eq!(
        requests
            .into_iter()
            .filter(|request| matches!(request, Request::RefreshWindowInventory(_)))
            .count(),
        1,
        "the first fresh post-unlock snapshot should flush exactly one deferred visibility refresh"
    );
}

#[test]
fn wake_gate_waits_for_fresh_space_snapshot_before_refresh() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    assert!(apps.requests().is_empty());

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::ApplicationGloballyActivated(1));

    let requests = apps.requests();
    assert!(
        requests
            .iter()
            .all(|request| !matches!(request, Request::RefreshWindowInventory(_))),
        "wake should quarantine visible-window enumeration until a fresh space snapshot: {requests:?}"
    );
    assert!(
        requests.iter().any(
            |request| matches!(request, Request::ApplicationGloballyActivated(pid) if *pid == 1)
        ),
        "Carbon activation should still be reconciled by the app thread: {requests:?}"
    );

    let stale_snapshot = space_state_event(vec![screen], vec![Some(space)]);
    reactor.handle_event(stale_snapshot);
    assert!(
        apps.requests().is_empty(),
        "an older queued WM snapshot must not release the wake quarantine"
    );

    let fresh_snapshot = space_state_event_with(vec![screen], vec![Some(space)], |state| {
        state.releases_lifecycle_refresh_quarantine = true
    });
    reactor.handle_event(fresh_snapshot);

    let requests = apps.requests();
    assert_eq!(
        requests
            .into_iter()
            .filter(|request| matches!(request, Request::RefreshWindowInventory(_)))
            .count(),
        1,
        "the first fresh post-wake snapshot should flush exactly one deferred visibility refresh"
    );
}

#[test]
fn post_wake_snapshot_replaces_an_inventory_that_never_replied() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid = 91;
    let (app_tx, mut app_rx) = actor::channel();

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.app_manager.apps.insert(pid, AppState {
        info: AppInfo {
            bundle_id: Some("com.test.wake-inventory".into()),
            localized_name: Some("Wake Inventory".into()),
        },
        handle: AppThreadHandle::new_for_test(app_tx),
    });

    reactor.request_window_inventory(pid);
    let (_, Request::RefreshWindowInventory(stale_token)) =
        app_rx.try_recv().expect("the initial inventory should be requested")
    else {
        panic!("expected a window inventory request");
    };

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SystemWoke);
    let fresh_snapshot = space_state_event_with(vec![screen], vec![Some(space)], |state| {
        state.releases_lifecycle_refresh_quarantine = true;
        state.should_force_refresh_layout = true;
    });
    reactor.handle_event(fresh_snapshot);

    let (_, Request::RefreshWindowInventory(fresh_token)) = app_rx
        .try_recv()
        .expect("wake recovery must replace an inventory that never replied")
    else {
        panic!("expected a replacement window inventory request");
    };
    assert_ne!(fresh_token.request_id, stale_token.request_id);

    let stale_window = WindowId::new(pid, 1);
    reactor.handle_event(Event::WindowsDiscovered {
        pid,
        token: stale_token,
        successful: true,
        new: vec![(
            stale_window,
            make_window_info(screen, None, "Stale Window", None),
        )],
        known_visible: vec![stale_window],
    });
    assert!(
        !reactor.state.windows.contains_window(stale_window),
        "a late reply from the abandoned request must not mutate recovery state",
    );
    assert_eq!(
        reactor.window_inventory_manager.in_flight.get(&pid),
        Some(&fresh_token),
        "a late stale reply must not clear the replacement request",
    );
    assert!(
        app_rx.try_recv().is_err(),
        "discarding the abandoned reply must not enqueue a third inventory",
    );
}

#[test]
fn ordinary_snapshot_does_not_abandon_current_inventory() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid = 92;
    let (app_tx, mut app_rx) = actor::channel();

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.app_manager.apps.insert(pid, AppState {
        info: AppInfo {
            bundle_id: Some("com.test.ordinary-inventory".into()),
            localized_name: Some("Ordinary Inventory".into()),
        },
        handle: AppThreadHandle::new_for_test(app_tx),
    });

    reactor.request_window_inventory(pid);
    let (_, Request::RefreshWindowInventory(token)) =
        app_rx.try_recv().expect("the initial inventory should be requested")
    else {
        panic!("expected a window inventory request");
    };

    let snapshot = space_state_event_with(vec![screen], vec![Some(space)], |state| {
        // Spaces marks every coherent snapshot as a display-churn acknowledgement,
        // even when no churn is active.
        state.releases_display_churn_refresh_quarantine = true
    });
    reactor.handle_event(snapshot);

    assert_eq!(
        reactor.window_inventory_manager.in_flight.get(&pid),
        Some(&token)
    );
    assert!(
        app_rx.try_recv().is_err(),
        "an ordinary snapshot must not replace an unrelated current inventory",
    );
}

#[test]
fn partial_post_wake_snapshot_preserves_manual_workspace_assignment() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let kept = WindowId::new(1, 1);
    let omitted = WindowId::new(1, 2);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));

    let secondary_workspace = reactor.test_workspace(space, 1);
    assert!(reactor.assign_test_window_to_workspace(space, omitted, secondary_workspace));

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SystemWoke);

    let mut fresh_state =
        forwarded_space_state(make_screen_snapshots(vec![screen], vec![Some(space)]));
    fresh_state.releases_lifecycle_refresh_quarantine = true;
    fresh_state
        .active_window_spaces
        .insert(WindowServerId::new(kept.idx.get()), space);
    reactor.handle_event(Event::SpaceStateChanged(fresh_state));

    assert_eq!(
        reactor.test_workspace_for_window(space, omitted),
        Some(secondary_workspace),
        "a partial recovery snapshot must not erase a manual workspace assignment"
    );

    reactor.discover_test_windows(1, vec![], vec![kept, omitted]);

    assert_eq!(
        reactor.test_workspace_for_window(space, omitted),
        Some(secondary_workspace),
        "post-wake discovery without an app rule must retain the manual workspace"
    );
}

#[test]
fn current_ax_destruction_after_quarantine_release_removes_window() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    assert!(!reactor.refreshes_blocked());
    assert!(has_window_in_layout(&mut reactor, space, screen, wid));
    let wsid = reactor.test_window_server_id(wid);

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    reactor.handle_event(Event::WindowDestroyed(wid));
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(reactor.state.windows.record(wid).is_none());
    assert!(!has_window_in_layout(&mut reactor, space, screen, wid));
}

#[test]
fn ax_destruction_removes_window_on_known_inactive_space_outside_churn() {
    let (mut reactor, wid, wsid, active_space, inactive_space, _frame) =
        reactor_with_window_on_space1();
    let inactive_workspace = reactor.test_workspace(inactive_space, 0);
    assert!(reactor.assign_test_window_to_workspace(inactive_space, wid, inactive_workspace));
    reactor.state.windows.set_window_server_space(wsid, Some(inactive_space));
    reactor.state.windows.mark_window_hidden(wsid);
    assert!(reactor.is_window_on_known_inactive_space(wid));

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    reactor.handle_event(Event::WindowDestroyed(wid));
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(reactor.state.windows.record(wid).is_none());
    assert_eq!(reactor.test_workspace_for_window(inactive_space, wid), None);
    assert_eq!(reactor.test_workspace_for_window(active_space, wid), None);
}

#[test]
fn ax_destruction_removes_already_minimized_window_outside_churn() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let wsid = reactor.test_window_server_id(wid);
    reactor.handle_event(Event::WindowMinimized(wid));
    assert!(reactor.state.windows.window(wid).unwrap().info.is_minimized);

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
    reactor.handle_event(Event::WindowDestroyed(wid));
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(reactor.state.windows.record(wid).is_none());
    assert!(!has_window_in_layout(&mut reactor, space, screen, wid));
}

#[test]
fn repeated_ordered_out_ax_replacement_does_not_accumulate_layout_ghosts() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let pid = 1;
    let middle = WindowId::new(pid, 2);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, pid, make_windows(3));
    let middle_info = reactor.state.windows.window(middle).unwrap().info.clone();
    let wsid = reactor.test_window_server_id(middle);
    assert_eq!(test_layout(&mut reactor, space, screen).len(), 3);

    for _ in 0..2 {
        crate::sys::window_server::set_window_ordered_in_override(wsid, Some(false));
        reactor.handle_event(Event::WindowDestroyed(middle));
        crate::sys::window_server::set_window_ordered_in_override(wsid, None);

        assert!(reactor.state.windows.record(middle).is_none());
        assert_eq!(
            test_layout(&mut reactor, space, screen).len(),
            2,
            "ordered-out AX destruction must remove its slot completely",
        );

        reactor.track_test_window_server_info(wsid, pid, middle_info.frame);
        reactor.mark_test_window_visible_in_space(wsid, space);
        reactor.discover_test_windows(pid, vec![(middle, middle_info.clone())], vec![
            WindowId::new(pid, 1),
            middle,
            WindowId::new(pid, 3),
        ]);
        assert_eq!(
            test_layout(&mut reactor, space, screen).len(),
            3,
            "rediscovery must restore exactly one slot",
        );
    }
}

#[test]
fn ax_destruction_removes_ordered_in_window_outside_churn() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let wsid = reactor.test_window_server_id(wid);
    assert!(!reactor.refreshes_blocked());
    assert!(has_window_in_layout(&mut reactor, space, screen, wid));

    crate::sys::window_server::set_window_ordered_in_override(wsid, Some(true));
    reactor.handle_event(Event::WindowDestroyed(wid));
    crate::sys::window_server::set_window_ordered_in_override(wsid, None);

    assert!(reactor.state.windows.record(wid).is_none());
    assert!(!has_window_in_layout(&mut reactor, space, screen, wid));
    assert!(
        apps.requests()
            .iter()
            .all(|request| !matches!(request, Request::RefreshWindowInventory(_))),
        "AX destruction outside churn should not trigger replacement-element polling",
    );
}

#[test]
fn stale_cleanup_uses_ordered_state_instead_of_cached_visibility() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let wsid = reactor.test_window_server_id(wid);
    let info = reactor
        .state
        .windows
        .get_window_server_info(wsid)
        .expect("test window should have native metadata");
    assert!(reactor.state.windows.is_window_visible(wsid));

    let snapshot_known =
        |suitable, ordered_in, still_known| window_discovery::StaleCleanupSnapshot {
            suppressed: false,
            mission_control_active: false,
            drag_active: false,
            inactive_windows: Default::default(),
            server_observations: [(wsid, window_discovery::StaleWindowObservation {
                info: Some(info),
                suitable,
                ordered_in,
                still_known,
            })]
            .into_iter()
            .collect(),
        };
    // The window server has forgotten the id unless a case says otherwise.
    let snapshot = |suitable, ordered_in| snapshot_known(suitable, ordered_in, false);

    let ordered_stale = window_discovery::identify_stale_windows(
        &reactor.state,
        wid.pid,
        &[],
        &snapshot(Some(true), Some(true)),
    );
    assert!(
        ordered_stale.is_empty(),
        "temporary AX omission must preserve an ordered-in window"
    );

    let closed_stale = window_discovery::identify_stale_windows(
        &reactor.state,
        wid.pid,
        &[],
        &snapshot(Some(true), Some(false)),
    );
    assert_eq!(
        closed_stale,
        vec![wid],
        "an ordered-out window must be retired even when cached visibility is stale",
    );

    let fullscreen_stale = window_discovery::identify_stale_windows(
        &reactor.state,
        wid.pid,
        &[],
        &snapshot_known(Some(true), Some(false), true),
    );
    assert!(
        fullscreen_stale.is_empty(),
        "a window the server still knows is away, not dead: retiring it drops the \
         user's float choice and it returns for the app rules to re-float",
    );

    let unknown_stale = window_discovery::identify_stale_windows(
        &reactor.state,
        wid.pid,
        &[],
        &snapshot(Some(true), None),
    );
    assert!(
        unknown_stale.is_empty(),
        "an unavailable ordered-state query must not remove a valid layout node",
    );

    let unknown_suitability_stale = window_discovery::identify_stale_windows(
        &reactor.state,
        wid.pid,
        &[],
        &snapshot(None, Some(true)),
    );
    assert!(
        unknown_suitability_stale.is_empty(),
        "an unavailable suitability query must not remove a valid layout node",
    );
}

#[test]
fn ax_invalidation_during_refresh_quarantine_is_deferred_without_layout_mutation() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    assert!(has_window_in_layout(&mut reactor, space, screen, wid));
    reactor.refresh_quarantine_manager.display_churn_active = true;

    reactor.handle_event(Event::WindowDestroyed(wid));

    assert!(
        reactor.state.windows.window(wid).is_some(),
        "unstable AX invalidation must not discard logical window state",
    );
    assert!(
        has_window_in_layout(&mut reactor, space, screen, wid),
        "unstable AX invalidation must not mutate layout topology",
    );
}

#[test]
fn sleep_ax_churn_preserves_modified_layout_through_recovery() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let windows = make_windows(4);
    let window_ids: Vec<_> = (1..=4).map(|idx| WindowId::new(1, idx)).collect();
    let rediscovered = window_ids.iter().copied().zip(windows.iter().cloned()).collect::<Vec<_>>();

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, windows);
    let default_layout = test_layout(&mut reactor, space, screen);
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, window_ids[1]));
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));
    let modified_layout = test_layout(&mut reactor, space, screen);
    assert_ne!(
        modified_layout, default_layout,
        "test setup must create a non-default layout"
    );

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::SessionDidResignActive);
    for wid in &window_ids {
        reactor.handle_event(Event::WindowDestroyed(*wid));
    }

    assert_eq!(
        test_layout(&mut reactor, space, screen),
        modified_layout,
        "sleep-time AX destruction must not alter layout topology or weights",
    );

    reactor.handle_event(Event::SessionDidBecomeActive);
    let mut recovered =
        forwarded_space_state(make_screen_snapshots(vec![screen], vec![Some(space)]));
    recovered.releases_lifecycle_refresh_quarantine = true;
    for wid in &window_ids {
        recovered.active_window_spaces.insert(WindowServerId::new(wid.idx.get()), space);
    }
    reactor.handle_event(Event::SpaceStateChanged(recovered));
    reactor.discover_test_windows(1, rediscovered, window_ids.clone());

    assert_eq!(
        test_layout(&mut reactor, space, screen),
        modified_layout,
        "authoritative recovery and AX rediscovery must update existing nodes in place",
    );
}

#[test]
fn clamshell_sleep_preserves_nested_layout_across_display_replacement() {
    fn without_frames(
        mut node: rift_protocol::ContainerTreeNode,
    ) -> rift_protocol::ContainerTreeNode {
        node.frame = Default::default();
        node.children = node.children.into_iter().map(without_frames).collect();
        node
    }

    fn without_frames_or_node_ids(
        mut node: rift_protocol::ContainerTreeNode,
    ) -> rift_protocol::ContainerTreeNode {
        node.frame = Default::default();
        node.node_id = 0;
        node.children = node.children.into_iter().map(without_frames_or_node_ids).collect();
        node
    }

    let (mut apps, mut reactor) = test_context();
    let external_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(3440., 1409.));
    let internal_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1728., 1083.));
    let space = SpaceId::new(1);
    let windows = make_windows(4);
    let window_ids: Vec<_> = (1..=4).map(|idx| WindowId::new(1, idx)).collect();
    let rediscovered = window_ids.iter().copied().zip(windows.iter().cloned()).collect::<Vec<_>>();

    apps.make_app_and_settle_on_screen(&mut reactor, external_screen, space, 1, windows);
    reactor.send_layout_event(LayoutEvent::WindowFocused(space, window_ids[1]));
    reactor.handle_test_layout_command(LayoutCommand::MoveNode(Direction::Up));

    let topology_before = reactor
        .query_layout_state(Some(space.get()), None)
        .expect("external-display layout state")
        .container_tree;
    assert!(
        topology_before.children.iter().any(|child| !child.children.is_empty()),
        "test setup must reproduce the nested split/stack topology from the clamshell capture",
    );

    reactor.handle_event(Event::DisplayChurnBegin);
    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SessionDidResignActive);
    for wid in &window_ids {
        reactor.handle_event(Event::WindowDestroyed(*wid));
    }

    assert_eq!(
        without_frames(
            reactor
                .query_layout_state(Some(space.get()), None)
                .expect("quarantined layout state")
                .container_tree,
        ),
        without_frames(topology_before.clone()),
        "sleep-time AX destruction must not flatten the nested layout",
    );

    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::SessionDidBecomeActive);
    let mut screens = make_screen_snapshots(vec![internal_screen], vec![Some(space)]);
    screens[0].display_uuid = "internal-display".to_string();
    let mut recovered = forwarded_space_state(screens);
    recovered.display_set_changed = true;
    recovered.topology_changed = true;
    recovered.allow_space_remap = true;
    recovered.should_force_refresh_layout = true;
    recovered.releases_lifecycle_refresh_quarantine = true;
    recovered.releases_display_churn_refresh_quarantine = true;
    recovered.resized_spaces.push((space, internal_screen.size));
    for wid in &window_ids {
        recovered.active_window_spaces.insert(WindowServerId::new(wid.idx.get()), space);
    }
    reactor.handle_event(Event::SpaceStateChanged(recovered));
    reactor.discover_test_windows(1, rediscovered, window_ids.clone());

    assert_eq!(
        without_frames_or_node_ids(
            reactor
                .query_layout_state(Some(space.get()), None)
                .expect("internal-display layout state")
                .container_tree,
        ),
        without_frames_or_node_ids(topology_before.clone()),
        "clamshell recovery must preserve container nesting, order, selection, and weights",
    );
    assert_eq!(
        test_layout(&mut reactor, space, internal_screen).len(),
        window_ids.len(),
        "every rediscovered window must occupy exactly one layout slot",
    );

    let mut screens = make_screen_snapshots(vec![external_screen], vec![Some(space)]);
    screens[0].display_uuid = "external-display".to_string();
    let mut reconnected = forwarded_space_state(screens);
    reconnected.display_set_changed = true;
    reconnected.topology_changed = true;
    reconnected.allow_space_remap = true;
    reconnected.should_force_refresh_layout = true;
    reconnected.releases_display_churn_refresh_quarantine = true;
    reconnected.resized_spaces.push((space, external_screen.size));
    for wid in &window_ids {
        reconnected
            .active_window_spaces
            .insert(WindowServerId::new(wid.idx.get()), space);
    }
    reactor.handle_event(Event::SpaceStateChanged(reconnected));

    assert_eq!(
        without_frames(
            reactor
                .query_layout_state(Some(space.get()), None)
                .expect("reconnected external-display layout state")
                .container_tree,
        ),
        without_frames(topology_before),
        "reconnecting a known display size must reactivate the exact saved layout tree",
    );
}

#[test]
fn genuine_close_during_sleep_recovery_does_not_leave_layout_ghost() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let survivor = WindowId::new(1, 1);
    let closed = WindowId::new(1, 2);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(2));
    let closed_wsid = reactor.test_window_server_id(closed);

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SessionDidResignActive);
    reactor.handle_event(Event::WindowDestroyed(closed));
    assert!(
        has_window_in_layout(&mut reactor, space, screen, closed),
        "the ambiguous AX edge must be preserved while sleep quarantine is active",
    );

    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::SessionDidBecomeActive);
    let mut recovered =
        forwarded_space_state(make_screen_snapshots(vec![screen], vec![Some(space)]));
    recovered.releases_lifecycle_refresh_quarantine = true;
    recovered
        .active_window_spaces
        .insert(WindowServerId::new(survivor.idx.get()), space);

    crate::sys::window_server::set_window_ordered_in_override(closed_wsid, Some(false));
    reactor.handle_event(Event::SpaceStateChanged(recovered));
    reactor.discover_test_windows(1, vec![], vec![survivor]);
    crate::sys::window_server::set_window_ordered_in_override(closed_wsid, None);

    assert!(reactor.state.windows.record(closed).is_none());
    assert!(!has_window_in_layout(&mut reactor, space, screen, closed));
    assert!(reactor.state.windows.contains_window(survivor));
    assert!(has_window_in_layout(&mut reactor, space, screen, survivor));
    assert_eq!(
        test_layout(&mut reactor, space, screen).len(),
        1,
        "post-sleep discovery must not retain a stale layout slot for the closed window",
    );
}

#[test]
fn last_window_close_during_sleep_recovery_does_not_leave_layout_ghost() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let closed = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let closed_wsid = reactor.test_window_server_id(closed);

    reactor.handle_event(Event::SystemWillSleep);
    reactor.handle_event(Event::SessionDidResignActive);
    reactor.handle_event(Event::WindowDestroyed(closed));
    assert!(
        has_window_in_layout(&mut reactor, space, screen, closed),
        "the ambiguous AX edge must be preserved while sleep quarantine is active",
    );

    reactor.handle_event(Event::SystemWoke);
    reactor.handle_event(Event::SessionDidBecomeActive);
    let mut recovered =
        forwarded_space_state(make_screen_snapshots(vec![screen], vec![Some(space)]));
    recovered.releases_lifecycle_refresh_quarantine = true;

    crate::sys::window_server::set_window_ordered_in_override(closed_wsid, Some(false));
    reactor.handle_event(Event::SpaceStateChanged(recovered));
    reactor.discover_test_windows(1, vec![], vec![]);
    crate::sys::window_server::set_window_ordered_in_override(closed_wsid, None);

    assert!(reactor.state.windows.record(closed).is_none());
    assert!(!has_window_in_layout(&mut reactor, space, screen, closed));
    assert!(test_layout(&mut reactor, space, screen).is_empty());
}

#[test]
fn authoritative_destruction_removes_window_server_backed_state() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));
    let wsid = reactor.test_window_server_id(wid);

    let outcome = window_workflow::handle_window_destroyed(
        &mut reactor.state,
        &reactor.transaction_manager,
        &mut reactor.drag_manager,
        window_workflow::WindowDestroyedPayload { window: wid },
    )
    .expect("authoritative destruction should be handled");
    reactor.apply_event_outcome(outcome);

    assert!(reactor.state.windows.record(wid).is_none());
    assert_eq!(reactor.state.windows.tracked_window_id(wsid), None);
    assert_eq!(reactor.state.windows.workspace_info_for_window(wid), None);
}

#[test]
fn authoritative_active_space_membership_comes_from_space_window_ids_directly() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wsid_a = WindowServerId::new(41);
    let wsid_b = WindowServerId::new(42);

    crate::sys::window_server::set_space_window_list_for_connection_override(Some(vec![
        wsid_a.as_u32(),
        wsid_b.as_u32(),
    ]));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    let snapshot = reactor.authoritative_active_space_windows();

    crate::sys::window_server::set_space_window_list_for_connection_override(None);

    let ids: Vec<_> = snapshot.into_iter().map(|(wsid, _)| wsid).collect();
    assert_eq!(
        ids,
        vec![wsid_a, wsid_b],
        "active-space membership should be built from the space's own WS ids rather than the lagging global visible-window list"
    );
}

#[test]
fn authoritative_active_space_membership_queries_each_active_space_independently() {
    let mut reactor = test_reactor();
    let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let wsid_left = WindowServerId::new(41);
    let wsid_right = WindowServerId::new(42);

    crate::sys::window_server::set_space_window_list_for_space_override(
        space1.get(),
        Some(vec![wsid_left.as_u32()]),
    );
    crate::sys::window_server::set_space_window_list_for_space_override(
        space2.get(),
        Some(vec![wsid_right.as_u32()]),
    );
    crate::sys::window_server::set_window_spaces_override(wsid_left, Some(vec![space1.get()]));
    crate::sys::window_server::set_window_spaces_override(wsid_right, Some(vec![space2.get()]));

    reactor.handle_event(space_state_event(vec![left, right], vec![
        Some(space1),
        Some(space2),
    ]));
    let mut snapshot = reactor.authoritative_active_space_windows();

    crate::sys::window_server::set_space_window_list_for_space_override(space1.get(), None);
    crate::sys::window_server::set_space_window_list_for_space_override(space2.get(), None);
    crate::sys::window_server::set_window_spaces_override(wsid_left, None);
    crate::sys::window_server::set_window_spaces_override(wsid_right, None);

    snapshot.sort_unstable_by_key(|(wsid, _)| wsid.as_u32());
    assert_eq!(
        snapshot,
        vec![(wsid_left, Some(space1)), (wsid_right, Some(space2))],
        "multi-display active-space membership should be collected per active space so stale union snapshots do not keep windows visible after topology changes"
    );
}

#[test]
fn empty_active_space_membership_during_wake_race_does_not_blank_known_active_windows() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(10001);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    reactor.mark_test_window_visible_in_space(wsid, space);

    crate::sys::window_server::set_space_window_list_for_connection_override(Some(vec![]));
    reactor.refresh_window_server_snapshot_for_active_spaces();
    crate::sys::window_server::set_space_window_list_for_connection_override(None);

    assert!(
        reactor.state.windows.is_window_visible(wsid),
        "a transient empty active-space WS-id result after wake must not blank windows we already know belong to the active space"
    );
    assert!(
        has_window_in_layout(&mut reactor, space, screen, wid),
        "preserving the visibility basis must also preserve the active workspace layout until discovery catches up"
    );
}

#[test]
fn wsid_rekey_preserves_non_default_workspace_without_app_rules() {
    let (mut apps, mut reactor) = test_context_with_workspace_count(2);
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let old_wid = WindowId::new(1, 1);
    let new_wid = WindowId::new(1, 99);

    apps.make_app_and_settle_on_screen(&mut reactor, screen, space, 1, make_windows(1));

    let workspaces = reactor.test_workspace_ids(space);
    let secondary_workspace = workspaces[1];

    assert!(reactor.assign_test_window_to_workspace(space, old_wid, secondary_workspace));
    assert!(reactor.set_test_active_workspace(space, secondary_workspace));

    rekey_window(&mut reactor, old_wid, new_wid);

    assert_eq!(
        reactor.test_workspace_for_window(space, new_wid),
        Some(secondary_workspace),
        "AX id churn for the same WindowServer window must preserve its workspace assignment"
    );
    assert_eq!(
        reactor
            .layout_manager
            .layout_engine
            .virtual_workspace_manager()
            .workspace_info_for_window_any(&reactor.state.windows, old_wid),
        None,
        "old AX window id should relinquish its assignment after rekey"
    );
}

#[test]
fn wsid_rekey_preserves_floating_membership_and_position() {
    let (mut apps, mut reactor) = test_context();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
    let space = SpaceId::new(1);
    let old_wid = WindowId::new(1, 1);
    let new_wid = WindowId::new(1, 99);
    let stored_position = CGRect::new(CGPoint::new(320., 180.), CGSize::new(240., 200.));

    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    make_active_app(&mut apps, &mut reactor, 1, make_windows(1), Some(old_wid));

    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    apps.simulate_until_quiet(&mut reactor);
    assert!(reactor.layout_manager.layout_engine.is_window_floating(old_wid));

    let active_workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(space)
        .expect("active workspace");
    reactor.layout_manager.layout_engine.store_floating_position(
        space,
        active_workspace,
        old_wid,
        stored_position,
    );

    rekey_window(&mut reactor, old_wid, new_wid);

    assert!(!reactor.layout_manager.layout_engine.is_window_floating(old_wid));
    assert!(reactor.layout_manager.layout_engine.is_window_floating(new_wid));
    assert_eq!(
        reactor.layout_manager.layout_engine.get_floating_position(
            space,
            active_workspace,
            old_wid
        ),
        None
    );
    assert_eq!(
        reactor.layout_manager.layout_engine.get_floating_position(
            space,
            active_workspace,
            new_wid
        ),
        Some(stored_position)
    );
}

#[test]
fn native_space_resolution_policy_table() {
    let mut cases = Vec::new();

    // A direct observation from the old space is stale while Rift's target is
    // still pending.
    {
        let (reactor, _wid, wsid, space1, space2, _) = reactor_with_window_moved_to_space2();
        cases.push((
            "stale origin",
            reactor.resolve_native_space(wsid, Some(space1)),
            Some(space2),
        ));
    }

    // A direct observation of the target confirms the pending move.
    {
        let (reactor, _wid, wsid, _space1, space2, _) = reactor_with_window_moved_to_space2();
        let resolved = reactor.resolve_native_space(wsid, Some(space2));
        reactor.clear_pending_target_if_confirmed_space(wsid, space2);
        cases.push(("confirmed target", resolved, Some(space2)));
    }

    // With no pending Rift move, a live WindowServer observation is an external move.
    {
        let (reactor, _wid, wsid, _space1, space2, _) = reactor_with_window_on_space1();
        crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![space2.get()]));
        let resolved = reactor.resolve_native_space(wsid, Some(space2));
        crate::sys::window_server::set_window_spaces_override(wsid, None);
        cases.push(("newer external move", resolved, Some(space2)));
    }

    // With only an accepted prior observation, a partial sample keeps it.
    {
        let (reactor, _wid, wsid, space1, _space2, _) = reactor_with_window_on_space1();
        cases.push((
            "partial observation",
            reactor.resolve_native_space(wsid, None),
            Some(space1),
        ));
    }

    // Geometry is used only when no native or prior WindowServer state exists.
    {
        let mut reactor = test_reactor();
        let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        let space2 = SpaceId::new(2);
        reactor.handle_event(space_state_event(vec![left, right], vec![
            Some(SpaceId::new(1)),
            Some(space2),
        ]));
        let frame = CGRect::new(CGPoint::new(1200., 100.), CGSize::new(400., 400.));
        cases.push((
            "geometry fallback",
            reactor.best_space_for_window(&frame, Some(WindowServerId::new(9999))),
            Some(space2),
        ));
    }

    for (case, resolved, expected) in cases {
        assert_eq!(resolved, expected, "resolver case: {case}");
    }
}

fn laid_out_frame(
    reactor: &mut Reactor,
    space: SpaceId,
    screen: CGRect,
    wid: WindowId,
) -> Option<CGRect> {
    let gaps = reactor.config.settings.layout.gaps.clone();
    reactor
        .layout_manager
        .layout_engine
        .calculate_layout_with_virtual_workspaces(
            &reactor.state.windows,
            space,
            screen,
            &gaps,
            0.0,
            Default::default(),
            Default::default(),
            |q| reactor.state.windows.window(q).map(|w| w.frame_monotonic),
            &[screen],
        )
        .into_iter()
        .find(|(w, _)| *w == wid)
        .map(|(_, f)| f)
}

#[test]
fn floating_window_toggles_to_fullscreen() {
    let (mut reactor, wid, space1, screen, _floating_frame) = reactor_with_floating_window();
    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreen);
    let laid_out = laid_out_frame(&mut reactor, space1, screen, wid).expect("window laid out");
    assert!(
        laid_out.same_as(screen),
        "expected fullscreen {screen:?}, got {laid_out:?}"
    );
}

#[test]
fn floating_window_toggle_off_restore_previous_frame() {
    let (mut reactor, wid, space1, screen, floating_frame) = reactor_with_floating_window();
    // Turn on
    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreen);
    // Turn off
    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreen);
    // The restore intent was already laid out by the command's own arrange;
    // once the window is back at the frame, a further layout owes no write.
    let restored = reactor.state.windows.window(wid).expect("window tracked").frame_monotonic;
    assert!(
        restored.same_as(floating_frame),
        "expected restore to {floating_frame:?}, got {restored:?}"
    );
    assert_eq!(
        laid_out_frame(&mut reactor, space1, screen, wid),
        None,
        "no further write once the restore has landed"
    );
}

#[test]
fn floating_window_toggles_to_fullscreen_within_gaps() {
    let (mut reactor, wid, space1, screen, _floating_frame) = reactor_with_floating_window();
    // Assymetric gaps to prevent swapped left/right or swapped width/height bugs from passing
    reactor.config.settings.layout.gaps.outer = OuterGaps {
        top: 10.,
        left: 20.,
        bottom: 30.,
        right: 40.,
    };
    reactor.handle_test_layout_command(LayoutCommand::ToggleFullscreenWithinGaps);
    let expected = CGRect::new(
        CGPoint::new(screen.origin.x + 20., screen.origin.y + 10.),
        CGSize::new(screen.size.width - 20. - 40., screen.size.height - 10. - 30.),
    );
    let laid_out = laid_out_frame(&mut reactor, space1, screen, wid).expect("window laid out");
    assert!(
        laid_out.same_as(expected),
        "expected {expected:?}, got {laid_out:?}"
    );
}

/// Two tiled windows side by side on one space, in the given layout mode.
/// Returns the reactor, the two windows, the space and the second window's frame.
fn reactor_with_two_tiled_windows(
    mode: LayoutMode,
) -> (Reactor, WindowId, WindowId, SpaceId, CGRect) {
    let settings = crate::common::config::LayoutSettings {
        mode,
        ..crate::common::config::LayoutSettings::default()
    };
    let mut reactor = Reactor::new_for_test(LayoutEngine::new(
        &crate::common::config::VirtualWorkspaceSettings::default(),
        &settings,
        None,
    ));
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let space = SpaceId::new(1);
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
    reactor.add_test_app(1);
    let workspace = reactor.test_workspace(space, 0);

    let left = WindowId::new(1, 1);
    let right = WindowId::new(1, 2);
    let left_frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(720., 900.));
    let right_frame = CGRect::new(CGPoint::new(720., 0.), CGSize::new(720., 900.));
    for (wid, wsid, frame) in [(left, 101, left_frame), (right, 102, right_frame)] {
        reactor.add_test_window(wid, WindowServerId::new(wsid), Some(space), frame);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        assert!(reactor.layout_manager.layout_engine.is_window_tiled(space, wid));
    }
    (reactor, left, right, space, right_frame)
}

#[test]
fn drag_swap_candidates_are_only_windows_in_the_layout() {
    let (mut reactor, dragged, tiled, space, frame) =
        reactor_with_two_tiled_windows(LayoutMode::Bsp);

    // Tracked on the space but never given a workspace or a place in the
    // layout, the way an app's popups and utility windows are. A window
    // without an assignment counts as being in the active workspace, which
    // used to be enough to make it a drop target.
    let unassigned = WindowId::new(1, 3);
    reactor.add_test_window(unassigned, WindowServerId::new(103), Some(space), frame);
    assert!(
        reactor.layout_manager.layout_engine.is_window_in_active_workspace(
            &reactor.state.windows,
            space,
            unassigned
        )
    );

    // Never admitted to the layout at all.
    let unmanaged = WindowId::new(1, 4);
    reactor.add_test_window_with_manageability(
        unmanaged,
        WindowServerId::new(104),
        Some(space),
        frame,
        false,
    );

    let candidates: Vec<_> = reactor
        .collect_drag_swap_candidates(dragged, space)
        .into_iter()
        .map(|(wid, _)| wid)
        .collect();
    assert_eq!(candidates, vec![tiled]);
}

#[test]
fn drop_action_only_promises_a_split_the_layout_can_make() {
    use crate::actor::drag_swap::DropAction;

    // A point well inside the target's left edge triangle.
    let (reactor, dragged, target, _space, frame) = reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let near_left_edge = CGPoint::new(frame.origin.x + 10., frame.mid().y);
    assert_eq!(
        reactor.drop_action_for(dragged, target, near_left_edge),
        Some(DropAction::Insert(Direction::Left))
    );
    assert_eq!(
        reactor.drop_action_for(dragged, target, frame.mid()),
        Some(DropAction::Swap)
    );

    // The traditional layout cannot split a window, so the drop there swaps
    // and the preview has to say so rather than draw a half.
    let (reactor, dragged, target, _space, frame) =
        reactor_with_two_tiled_windows(LayoutMode::Traditional);
    let near_left_edge = CGPoint::new(frame.origin.x + 10., frame.mid().y);
    assert_eq!(
        reactor.drop_action_for(dragged, target, near_left_edge),
        Some(DropAction::Swap)
    );
}

#[test]
fn drop_action_needs_both_windows_in_the_layout() {
    let (mut reactor, dragged, target, space, frame) =
        reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let stray = WindowId::new(1, 3);
    reactor.add_test_window(stray, WindowServerId::new(103), Some(space), frame);

    assert_eq!(reactor.drop_action_for(dragged, stray, frame.mid()), None);
    assert_eq!(reactor.drop_action_for(stray, target, frame.mid()), None);
}

/// A window dragged onto another display, with a tiled window under the
/// pointer there: the overlay previews the split on the target display, and
/// the drop performs it — the dragged window leaves its origin tree and is
/// inserted beside the target.
#[test]
fn cross_display_drag_previews_and_splits_the_target_under_the_pointer() {
    use crate::actor::drag_swap::DropAction;
    use crate::sys::window_server::set_window_spaces_override;
    // A tree layout on both displays, so the target's side can split.
    let settings = crate::common::config::LayoutSettings {
        mode: LayoutMode::Bsp,
        ..crate::common::config::LayoutSettings::default()
    };
    let mut reactor = Reactor::new_for_test(LayoutEngine::new(
        &crate::common::config::VirtualWorkspaceSettings::default(),
        &settings,
        None,
    ));
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let screen2 = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let initial_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    reactor.handle_event(space_state_event(vec![screen1, screen2], vec![
        Some(space1),
        Some(space2),
    ]));
    reactor.add_test_app(1);
    reactor.config.settings.ui.drop_overlay.enabled = true;

    let dragged = WindowId::new(1, 1);
    reactor.add_test_window(dragged, WindowServerId::new(121), Some(space1), initial_frame);
    let space1_workspace = reactor.test_workspace(space1, 0);
    assert!(reactor.assign_test_window_to_workspace(space1, dragged, space1_workspace));
    reactor.send_layout_event(LayoutEvent::WindowAdded(space1, dragged));

    // A tiled window on the second display, filling it.
    let target = WindowId::new(1, 2);
    let target_frame = screen2;
    reactor.add_test_window(target, WindowServerId::new(122), Some(space2), target_frame);
    let space2_workspace = reactor.test_workspace(space2, 0);
    assert!(reactor.assign_test_window_to_workspace(space2, target, space2_workspace));
    reactor.send_layout_event(LayoutEvent::WindowAdded(space2, target));

    // The drop resolves the target's space through `window_spaces`, which falls
    // through to the *live* window server for any id without an override. These
    // two ids exist on the developer's machine as often as not, so without this
    // the drop lands in whatever real space the host reports -- a different one
    // per run, and never `space2`.
    set_window_spaces_override(WindowServerId::new(121), Some(vec![space1.get()]));
    set_window_spaces_override(WindowServerId::new(122), Some(vec![space2.get()]));

    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: dragged,
            last_frame: initial_frame,
            origin_space: Some(space1),
            settled_space: Some(space1),
            layout_dirty: true,
        },
    };

    // Pointer near the target's left edge on display 2, the dragged window
    // barely over the seam: the overlay promises the split.
    let cursor = CGPoint::new(target_frame.origin.x + 20., target_frame.mid().y);
    // Centred on the target display: mid-cross the drag-swap manager's noted
    // origin frame lies there too, which is exactly the case that used to
    // lose the origin hint on every other sample.
    let dragged_frame = CGRect::new(
        CGPoint::new(target_frame.origin.x + 40., 100.),
        initial_frame.size,
    );
    reactor.evaluate_drop_target(dragged, dragged_frame, Some(cursor));
    assert_eq!(reactor.get_pending_drag_swap(), Some((dragged, target)));
    assert!(
        reactor.drag_manager.drop_overlay_shown,
        "an edge drop on another display's window is previewed"
    );
    // The next samples arrive with the state already PendingSwap; the
    // preview must hold, not blink off on every other evaluation.
    for _ in 0..3 {
        reactor.evaluate_drop_target(dragged, dragged_frame, Some(cursor));
        assert_eq!(
            reactor.get_pending_drag_swap(),
            Some((dragged, target)),
            "the pending drop survives consecutive evaluations"
        );
        assert!(
            reactor.drag_manager.drop_overlay_shown,
            "the overlay stays up across consecutive evaluations"
        );
    }
    assert_eq!(
        reactor.drop_action_for(dragged, target, cursor),
        Some(DropAction::Insert(Direction::Left))
    );

    // The middle of the target is not dead across displays: a swap between
    // two trees does not exist, so every point over the target is an edge
    // zone and the overlay never blinks out mid-window.
    reactor.evaluate_drop_target(dragged, dragged_frame, Some(target_frame.mid()));
    assert!(matches!(
        reactor.drop_action_for(dragged, target, target_frame.mid()),
        Some(DropAction::Insert(_))
    ));
    assert!(
        reactor.drag_manager.drop_overlay_shown,
        "the overlay stays up over the middle of a cross-display target"
    );

    // On the left edge the drop performs the promised split.
    reactor.evaluate_drop_target(dragged, dragged_frame, Some(cursor));
    assert!(reactor.drag_manager.drop_overlay_shown);
    crate::sys::window_server::set_cursor_location_override(Some(cursor));
    reactor.handle_event(Event::MouseUp);
    crate::sys::window_server::set_cursor_location_override(None);

    let engine = &reactor.layout_manager.layout_engine;
    assert!(
        engine.is_window_tiled(space2, dragged),
        "the dragged window is tiled beside the target"
    );
    assert!(engine.is_window_tiled(space2, target));
    assert!(
        !engine.is_window_tiled(space1, dragged),
        "the dragged window left its origin tree"
    );
    assert_eq!(reactor.assigned_space_for_window_id(dragged), Some(space2));
    assert!(!reactor.drag_manager.drop_overlay_shown);

    for wsid in [121, 122] {
        set_window_spaces_override(WindowServerId::new(wsid), None);
    }
}

/// A drag crossing a display seam makes the window server emit a space
/// snapshot per flip, and each one used to answer with a full every-app AX
/// census — ~11 discovery sweeps a second for the whole drag. The sweep is
/// deferred while a drag is in flight and flushed once, at the drop.
#[test]
fn discovery_sweeps_are_deferred_while_a_drag_is_in_flight() {
    let mut reactor = test_reactor();
    let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let space = SpaceId::new(1);
    reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

    let pid: pid_t = 71;
    let (app_tx, mut app_rx) = crate::actor::channel();
    reactor.app_manager.apps.insert(pid, AppState {
        info: AppInfo {
            bundle_id: Some("com.test.sweep".to_string()),
            localized_name: Some("Sweep".to_string()),
        },
        handle: AppThreadHandle::new_for_test(app_tx),
    });

    let wid = WindowId::new(pid, 1);
    let frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    reactor.add_test_window(wid, WindowServerId::new(711), Some(space), frame);
    while app_rx.try_recv().is_ok() {}

    // No drag: a sweep goes out immediately.
    reactor.request_window_inventories();
    assert!(
        matches!(app_rx.try_recv(), Ok((_, Request::RefreshWindowInventory(_)))),
        "with no drag in flight the sweep is immediate"
    );
    // One inventory is in flight per app until it is answered, and nothing
    // answers here: retire it so the post-drop sweep is not merely queued
    // behind it.
    reactor.forget_window_inventory(pid);

    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: wid,
            last_frame: frame,
            origin_space: Some(space),
            settled_space: Some(space),
            layout_dirty: true,
        },
    };
    reactor.request_window_inventories();
    reactor.request_window_inventories();
    assert!(
        app_rx.try_recv().is_err(),
        "sweeps are deferred while the drag is in flight"
    );

    crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Up));
    reactor.handle_event(Event::MouseUp);
    crate::sys::event::set_mouse_state_override(None);
    assert!(
        matches!(app_rx.try_recv(), Ok((_, Request::RefreshWindowInventory(_)))),
        "the drop flushes the one sweep that is owed"
    );
    assert!(
        app_rx.try_recv().is_err(),
        "deferred sweeps collapse into a single census"
    );
}

#[test]
fn drop_overlay_is_taken_down_when_the_drag_ends_without_a_drop() {
    let (mut reactor, dragged, target, space, _frame) =
        reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let frame = reactor.state.windows.window(dragged).unwrap().frame_monotonic;
    reactor.drag_manager.drag_state = DragState::PendingSwap {
        session: DragSession {
            window: dragged,
            last_frame: frame,
            origin_space: Some(space),
            settled_space: Some(space),
            layout_dirty: true,
        },
        target,
    };
    reactor.drag_manager.drop_overlay_shown = true;

    // The target going away ends the pending drop inside a workflow that
    // knows nothing about the overlay; the overlay still has to come down.
    reactor.handle_event(Event::WindowDestroyed(target));
    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));
    assert!(!reactor.drag_manager.drop_overlay_shown);
}

#[test]
fn drop_overlay_stays_while_the_drop_is_still_pending() {
    let (mut reactor, dragged, target, space, _frame) =
        reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let frame = reactor.state.windows.window(dragged).unwrap().frame_monotonic;
    reactor.drag_manager.drag_state = DragState::PendingSwap {
        session: DragSession {
            window: dragged,
            last_frame: frame,
            origin_space: Some(space),
            settled_space: Some(space),
            layout_dirty: true,
        },
        target,
    };
    reactor.drag_manager.drop_overlay_shown = true;

    // An unrelated event must not take it down.
    reactor.handle_event(Event::WindowTitleChanged(target, "renamed".into()));
    assert!(matches!(
        reactor.drag_manager.drag_state,
        DragState::PendingSwap { .. }
    ));
    assert!(reactor.drag_manager.drop_overlay_shown);
}

/// Two tiled windows, one above the other, with a drag session open on the
/// top one. Returns the reactor, the two windows, the space and the bottom
/// window's frame.
fn reactor_dragging_top_of_two_stacked_windows() -> (Reactor, WindowId, WindowId, SpaceId, CGRect) {
    let (mut reactor, top, bottom, space, _) = reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let top_frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 450.));
    let bottom_frame = CGRect::new(CGPoint::new(0., 450.), CGSize::new(1440., 450.));
    reactor.state.windows.window_mut(top).unwrap().frame_monotonic = top_frame;
    reactor.state.windows.window_mut(bottom).unwrap().frame_monotonic = bottom_frame;
    // Off by default; these tests are about when it is shown.
    reactor.config.settings.ui.drop_overlay.enabled = true;
    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: top,
            last_frame: top_frame,
            origin_space: Some(space),
            settled_space: Some(space),
            layout_dirty: true,
        },
    };
    (reactor, top, bottom, space, bottom_frame)
}

#[test]
fn drop_target_is_the_tiled_window_under_the_pointer_regardless_of_overlap() {
    use crate::actor::drag_swap::DropAction;
    let (mut reactor, top, bottom, _space, bottom_frame) =
        reactor_dragging_top_of_two_stacked_windows();

    // The pointer is on the bottom window's left edge, with the full-width
    // dragged window hanging mostly off the left of the screen: almost no
    // overlap, which used to mean no target and no overlay.
    let cursor = CGPoint::new(bottom_frame.origin.x + 20., bottom_frame.mid().y);
    let dragged_frame = CGRect::new(CGPoint::new(-1300., 430.), CGSize::new(1440., 450.));
    reactor.evaluate_drop_target(top, dragged_frame, Some(cursor));

    assert_eq!(reactor.get_pending_drag_swap(), Some((top, bottom)));
    assert!(reactor.drag_manager.drop_overlay_shown);
    assert_eq!(
        reactor.drop_action_for(top, bottom, cursor),
        Some(DropAction::Insert(Direction::Left))
    );
}

/// A stack gives every member the container's whole rect and shows one of
/// them at a time, so trading two of them leaves the screen exactly as it
/// was. The pointer, being over all of them at once, always found one to
/// swap with: the overlay drew a screen-sized promise for the length of the
/// drag and the drop kept none of it. Under the stack layout that is every
/// other window on the space, so the drag now finds nothing at all.
#[test]
fn a_stack_member_is_not_a_drop_target() {
    let (mut reactor, dragged, other, space, _) = reactor_with_two_tiled_windows(LayoutMode::Stack);
    let whole = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    for wid in [dragged, other] {
        reactor.state.windows.window_mut(wid).unwrap().frame_monotonic = whole;
    }
    reactor.config.settings.ui.drop_overlay.enabled = true;
    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: dragged,
            last_frame: whole,
            origin_space: Some(space),
            settled_space: Some(space),
            layout_dirty: true,
        },
    };

    assert!(
        reactor
            .layout_manager
            .layout_engine
            .windows_share_a_stack(space, dragged, other),
        "the stack layout keeps every window on the space in one stack"
    );
    assert!(
        reactor.collect_drag_swap_candidates(dragged, space).is_empty(),
        "a stack sibling is nothing to swap with"
    );

    reactor.evaluate_drop_target(dragged, whole, Some(whole.mid()));
    assert_eq!(reactor.get_pending_drag_swap(), None);
    assert!(
        !reactor.drag_manager.drop_overlay_shown,
        "no overlay where the drop would rearrange nothing"
    );

    // Windows that merely sit side by side are not in a stack, and dropping
    // one on the other still means something.
    let (tiled, left, right, bsp_space, _) = reactor_with_two_tiled_windows(LayoutMode::Bsp);
    assert!(!tiled.layout_manager.layout_engine.windows_share_a_stack(bsp_space, left, right));
}

#[test]
fn no_drop_target_while_the_pointer_is_over_the_dragged_windows_own_slot() {
    let (mut reactor, top, bottom, _space, bottom_frame) =
        reactor_dragging_top_of_two_stacked_windows();

    // Nudged down so it overlaps the bottom window heavily, but the pointer
    // is still in the top slot: that is not a drop on anything.
    let cursor = CGPoint::new(720., 100.);
    let dragged_frame = CGRect::new(CGPoint::new(0., 300.), CGSize::new(1440., 450.));
    assert!(dragged_frame.intersection(&bottom_frame).size.height > 250.);
    reactor.evaluate_drop_target(top, dragged_frame, Some(cursor));

    assert_eq!(reactor.get_pending_drag_swap(), None);
    assert!(matches!(
        reactor.drag_manager.drag_state,
        DragState::Active { .. }
    ));
    assert!(!reactor.drag_manager.drop_overlay_shown);
    let _ = bottom;
}

#[test]
fn leaving_the_target_clears_the_pending_drop_and_the_overlay() {
    let (mut reactor, top, bottom, _space, bottom_frame) =
        reactor_dragging_top_of_two_stacked_windows();
    let dragged_frame = CGRect::new(CGPoint::new(0., 300.), CGSize::new(1440., 450.));

    reactor.evaluate_drop_target(top, dragged_frame, Some(bottom_frame.mid()));
    assert_eq!(reactor.get_pending_drag_swap(), Some((top, bottom)));
    assert!(reactor.drag_manager.drop_overlay_shown);

    reactor.evaluate_drop_target(top, dragged_frame, Some(CGPoint::new(720., 100.)));
    assert_eq!(reactor.get_pending_drag_swap(), None);
    assert!(matches!(
        reactor.drag_manager.drag_state,
        DragState::Active { .. }
    ));
    assert!(!reactor.drag_manager.drop_overlay_shown);
}

#[test]
fn pointer_samples_during_a_drag_re_evaluate_the_drop_target() {
    let (mut reactor, top, bottom, _space, bottom_frame) =
        reactor_dragging_top_of_two_stacked_windows();
    let mid = bottom_frame.mid();
    reactor.handle_event(Event::MouseDragged { x: mid.x, y: mid.y });
    assert_eq!(reactor.get_pending_drag_swap(), Some((top, bottom)));
    assert!(reactor.drag_manager.drop_overlay_shown);

    reactor.handle_event(Event::MouseDragged { x: 720., y: 100. });
    assert_eq!(reactor.get_pending_drag_swap(), None);
    assert!(!reactor.drag_manager.drop_overlay_shown);
}

/// Pointer samples straddling a zone boundary must not flap the preview:
/// the shown action follows the pointer into a new zone only after it has
/// stayed there for a beat (`ZoneCandidate::DWELL`).
#[test]
fn zone_boundary_wobble_does_not_flap_the_preview() {
    use crate::actor::drag_swap::DropAction;
    let (mut reactor, top, bottom, _space, bottom_frame) =
        reactor_dragging_top_of_two_stacked_windows();

    // Firmly in the bottom window's left edge zone.
    let left = CGPoint::new(bottom_frame.origin.x + 20., bottom_frame.mid().y);
    reactor.handle_event(Event::MouseDragged { x: left.x, y: left.y });
    assert_eq!(reactor.get_pending_drag_swap(), Some((top, bottom)));
    let shown = reactor.drag_manager.drop_preview_cache.expect("preview shown");
    assert_eq!(shown.action, DropAction::Insert(Direction::Left));

    // A sample on the other side of the boundary (the centre, which swaps)
    // arrives an instant later: the preview must not follow it yet.
    let mid = bottom_frame.mid();
    reactor.handle_event(Event::MouseDragged { x: mid.x, y: mid.y });
    let still = reactor.drag_manager.drop_preview_cache.expect("preview kept");
    assert_eq!(
        still.action,
        DropAction::Insert(Direction::Left),
        "a single sample across the boundary must not move the preview"
    );

    // And the drop performs what the overlay shows, not the raw zone under
    // the wobbling pointer.
    crate::sys::window_server::set_cursor_location_override(Some(mid));
    reactor.handle_event(Event::MouseUp);
    crate::sys::window_server::set_cursor_location_override(None);
    let engine = &reactor.layout_manager.layout_engine;
    assert!(engine.is_window_tiled(_space, top));
    assert!(engine.is_window_tiled(_space, bottom));
}

#[test]
fn split_preview_is_where_the_window_lands_not_half_of_the_target() {
    let (reactor, left, right, space, _) = reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let screen = reactor.space_state.screen_by_space(space).unwrap().frame;
    let preview = |target: WindowId, direction: Direction| {
        reactor.preview_insert_frame(target, direction, right)
    };

    // Dropping the right window on the left one's right edge yields the
    // layout it started from: the preview must be the right half of the
    // screen, which is where the window already is — not the right half of
    // the left window.
    let same_place = preview(left, Direction::Right).expect("bsp can split");
    assert!(
        same_place.origin.x >= screen.mid().x - 1.0 && same_place.size.width > 600.,
        "expected the right half of the screen, got {same_place:?}"
    );

    // Dropping it below the left window instead splits the whole screen
    // top/bottom: full width, bottom half.
    let below = preview(left, Direction::Down).expect("bsp can split");
    assert!(below.size.width > 1200., "full width, got {below:?}");
    assert!(
        below.origin.y >= screen.mid().y - 1.0,
        "bottom half, got {below:?}"
    );

    // The simulation must not have touched the real tree.
    assert!(reactor.layout_manager.layout_engine.is_window_tiled(space, left));
    assert!(reactor.layout_manager.layout_engine.is_window_tiled(space, right));
}

mod mouse_follows_focus {
    use test_log::test;

    use super::*;

    fn two_apps_focused_on_first() -> (Apps, Reactor, WindowId, WindowId) {
        let (mut apps, mut reactor) = test_context();
        reactor.config.settings.mouse_follows_focus = true;
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        let a = WindowId::new(1, 1);
        let b = WindowId::new(2, 1);
        crate::sys::window_server::set_cursor_location_override(Some(CGPoint::new(-5000., -5000.)));
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(a), true, true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_events(apps.make_app_with_opts(2, make_windows(1), Some(b), false, true));
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(reactor.main_window(), Some(a));
        reactor.test_mouse_warps.clear();
        (apps, reactor, a, b)
    }

    /// Focus that changes because of a click — into a window, on another
    /// display's menu bar, to dismiss a popover — is the pointer's own doing.
    /// The pointer stays where the user put it, even far from the window.
    #[test]
    fn focus_change_right_after_a_click_does_not_warp() {
        let (_apps, mut reactor, _a, b) = two_apps_focused_on_first();
        reactor.handle_event(Event::MouseUp);
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.main_window(), Some(b));
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "a focus change that follows a click does not move the pointer"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// A click followed by a key press — triple-click a paragraph, cmd-tab
    /// to the other app — is a focus change the keyboard made. The click was
    /// merely recent, and the pointer goes where focus went.
    #[test]
    fn focus_change_from_the_keyboard_right_after_a_click_warps() {
        let (_apps, mut reactor, _a, b) = two_apps_focused_on_first();
        reactor.handle_event(Event::MouseUp);
        crate::sys::event::set_key_pressed_since_mouse_up_override(Some(true));
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.main_window(), Some(b));
        let b_center = reactor.live_frame_for(b).unwrap().mid();
        assert_eq!(
            reactor.test_mouse_warps,
            vec![b_center],
            "a cmd-tab right after a click still takes the pointer along"
        );
        crate::sys::event::set_key_pressed_since_mouse_up_override(None);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// A click on a Dock tile is a click, but it is the one that means "take
    /// me there": the pointer is on the Dock, and the window it summons is
    /// somewhere else entirely.
    #[test]
    fn focus_change_from_a_dock_click_warps() {
        let (_apps, mut reactor, _a, b) = two_apps_focused_on_first();
        crate::sys::window_server::set_pointer_over_dock_override(true);
        reactor.handle_event(Event::MouseUp);
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.main_window(), Some(b));
        let b_center = reactor.live_frame_for(b).unwrap().mid();
        assert_eq!(
            reactor.test_mouse_warps,
            vec![b_center],
            "a Dock click takes the pointer to the window it summoned"
        );
        crate::sys::window_server::set_pointer_over_dock_override(false);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// Clicking the tile of the app that is already in front changes no
    /// focus at all, so nothing else in the reactor would move the pointer.
    /// The click still means "take me there".
    #[test]
    fn a_dock_click_on_the_app_already_in_front_warps() {
        let (_apps, mut reactor, a, _b) = two_apps_focused_on_first();
        reactor.app_manager.apps.get_mut(&1).unwrap().info.localized_name = Some("One".to_string());
        crate::sys::window_server::set_pointer_over_dock_override(true);
        crate::sys::axuielement::set_dock_application_tile_override(Some("One"));
        crate::sys::event::set_last_mouse_up_was_left_override(Some(true));

        reactor.handle_event(Event::MouseUp);

        let a_center = reactor.live_frame_for(a).unwrap().mid();
        assert_eq!(
            reactor.test_mouse_warps,
            vec![a_center],
            "the tile of the front app still sends the pointer to its window"
        );
        crate::sys::event::set_last_mouse_up_was_left_override(None);
        crate::sys::axuielement::set_dock_application_tile_override(None);
        crate::sys::window_server::set_pointer_over_dock_override(false);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// A stack, the Trash, and the right button all leave something open
    /// under the pointer. None of them is a request to go anywhere.
    #[test]
    fn a_dock_click_that_opens_something_under_the_pointer_does_not_warp() {
        let (_apps, mut reactor, _a, _b) = two_apps_focused_on_first();
        reactor.app_manager.apps.get_mut(&1).unwrap().info.localized_name = Some("One".to_string());
        crate::sys::window_server::set_pointer_over_dock_override(true);
        crate::sys::event::set_last_mouse_up_was_left_override(Some(true));

        // The Downloads stack: not an application tile, so no title at all.
        crate::sys::axuielement::set_dock_application_tile_override(None);
        reactor.handle_event(Event::MouseUp);
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "a stack opens a fan where the pointer is"
        );

        // The front app's own tile, right-clicked: a menu, not a request.
        crate::sys::axuielement::set_dock_application_tile_override(Some("One"));
        crate::sys::event::set_last_mouse_up_was_left_override(Some(false));
        reactor.handle_event(Event::MouseUp);
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "the right button opens a menu the pointer has to stay in"
        );

        crate::sys::event::set_last_mouse_up_was_left_override(None);
        crate::sys::axuielement::set_dock_application_tile_override(None);
        crate::sys::window_server::set_pointer_over_dock_override(false);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// Dragging a file onto a tile activates the app the same way, with the
    /// button still down. Warping then would drop the file on the way.
    #[test]
    fn a_drag_onto_a_dock_tile_does_not_warp() {
        let (_apps, mut reactor, _a, b) = two_apps_focused_on_first();
        crate::sys::window_server::set_pointer_over_dock_override(true);
        crate::sys::event::set_mouse_state_override(Some(crate::sys::event::MouseState::Down));
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.main_window(), Some(b));
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "a drag held over a Dock tile keeps the pointer"
        );
        crate::sys::event::set_mouse_state_override(None);
        crate::sys::window_server::set_pointer_over_dock_override(false);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// Focus that leaves for something rift has no window for (a status-item
    /// popover, a menu) and comes straight back is the user finishing with
    /// that, not choosing the window again: no warp.
    #[test]
    fn focus_returning_from_a_windowless_app_does_not_warp() {
        let (mut apps, mut reactor, a, _b) = two_apps_focused_on_first();
        // A status-item app: activated, no windows at all.
        reactor.handle_events(apps.make_app_with_opts(3, Vec::new(), None, false, true));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(3));
        reactor.handle_event(Event::ApplicationActivated(3, Quiet::No));
        assert_eq!(reactor.main_window(), None);
        reactor.test_mouse_warps.clear();

        // The popover closes; focus falls back to where it was.
        reactor.handle_event(Event::ApplicationDeactivated(3));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));
        assert_eq!(reactor.main_window(), Some(a));
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "focus coming back from a windowless app does not move the pointer"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// A window that gains focus by any route — here cmd-tab between apps,
    /// nothing rift did — pulls the pointer with it, floating or not, unless
    /// the pointer is already inside it.
    #[test]
    fn focus_change_not_caused_by_rift_warps_the_pointer_even_for_floating_windows() {
        let (mut apps, mut reactor) = test_context();
        reactor.config.settings.mouse_follows_focus = true;
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));

        let a = WindowId::new(1, 1);
        let b = WindowId::new(2, 1);
        let far_away = CGPoint::new(-5000., -5000.);
        crate::sys::window_server::set_cursor_location_override(Some(far_away));
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(a), true, true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_events(apps.make_app_with_opts(2, make_windows(1), Some(b), false, true));
        apps.simulate_until_quiet(&mut reactor);
        // Both float: the pointer must follow regardless of window state.
        reactor.layout_manager.layout_engine.mark_window_floating(a);
        reactor.layout_manager.layout_engine.mark_window_floating(b);
        assert_eq!(reactor.main_window(), Some(a));
        reactor.test_mouse_warps.clear();

        // cmd-tab to app 2.
        reactor.handle_event(Event::ApplicationDeactivated(1));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationActivated(2, Quiet::No));
        assert_eq!(reactor.main_window(), Some(b));
        // The warp aims at the window server's frame, not rift's record.
        let b_center = reactor.live_frame_for(b).unwrap().mid();
        assert_eq!(
            reactor.test_mouse_warps,
            vec![b_center],
            "the pointer follows to b"
        );

        // Back to app 1 by clicking in `a`: the pointer is already there.
        let a_center = reactor.live_frame_for(a).unwrap().mid();
        crate::sys::window_server::set_cursor_location_override(Some(a_center));
        reactor.test_mouse_warps.clear();
        reactor.handle_event(Event::ApplicationDeactivated(2));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));
        assert_eq!(reactor.main_window(), Some(a));
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "no warp when the pointer is already in the window"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// Two displays, the pointer and the key window on the left one. Windows
    /// on both, the right one's remembered as that space's last focus.
    fn two_displays_focused_on_left() -> (Apps, Reactor, actor::Receiver<raise_manager::Event>) {
        let (mut apps, mut reactor) = test_context();
        let (raise_manager_tx, mut raise_manager_rx) = actor::channel();
        reactor.communication_manager.raise_manager_tx = raise_manager_tx;
        reactor.config.settings.mouse_follows_focus = true;
        // What the window server shows on each space, so the test does not
        // ask the real one: window 1 on space 1, window 2 on space 2, nothing
        // on space 3. The harness numbers window-server ids pid × 10000 + index.
        use crate::sys::window_server::set_space_window_list_for_space_override as show;
        show(1, Some(vec![10001]));
        show(2, Some(vec![10002]));
        show(3, Some(vec![]));
        let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(space_state_event_with(
            vec![left, right],
            vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            |state| state.has_seen_display_set = true,
        ));
        crate::sys::window_server::set_cursor_location_override(Some(left.mid()));

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            windows,
            Some(WindowId::new(1, 1)),
            true,
            true,
        ));
        apps.simulate_until_quiet(&mut reactor);
        reactor.send_layout_event(LayoutEvent::WindowFocused(SpaceId::new(2), WindowId::new(1, 2)));
        reactor.send_layout_event(LayoutEvent::WindowFocused(SpaceId::new(1), WindowId::new(1, 1)));
        assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));
        while raise_manager_rx.try_recv().is_ok() {}
        reactor.test_mouse_warps.clear();
        (apps, reactor, raise_manager_rx)
    }

    /// The right display switches to `space`; the left keeps the pointer,
    /// the key window and the command space.
    fn switch_right_display_to(reactor: &mut Reactor, space: SpaceId) {
        let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(space_state_event_with(
            vec![left, right],
            vec![Some(SpaceId::new(1)), Some(space)],
            |state| {
                state.has_seen_display_set = true;
                state.command_space = Some(SpaceId::new(1));
                state.menu_bar_space = Some(SpaceId::new(1));
            },
        ));
    }

    /// A space switch on the other display activates nothing, so there is no
    /// focus change to follow. The switch is finished here: the space's last
    /// window is focused, and the pointer goes to it.
    #[test]
    fn a_space_switch_on_the_other_display_focuses_its_last_window_and_warps_there() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        let b = WindowId::new(1, 2);

        switch_right_display_to(&mut reactor, SpaceId::new(3));
        while raise_manager_rx.try_recv().is_ok() {}
        reactor.test_mouse_warps.clear();
        switch_right_display_to(&mut reactor, SpaceId::new(2));
        let msg = raise_manager_rx.try_recv().expect("the switch focuses the space's window").1;
        let raise_manager::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = msg else {
            panic!("unexpected raise manager event: {msg:?}");
        };
        let b_center = reactor.live_frame_for(b).unwrap().mid();
        assert_eq!(
            focus_window,
            Some((b, Some(b_center))),
            "the window last used on the space is focused, with the pointer warped onto it"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// When the space has nothing rift knows of, the pointer still crosses
    /// over: to the middle of that display.
    #[test]
    fn a_space_switch_on_the_other_display_warps_to_its_centre_when_the_space_is_empty() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));

        switch_right_display_to(&mut reactor, SpaceId::new(3));

        assert_eq!(reactor.test_mouse_warps, vec![right.mid()]);
        assert!(
            raise_manager_rx.try_recv().is_err(),
            "nothing on the space, so nothing to focus"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// The pointer is already on the display that switched: a swipe there, or
    /// rift's own gesture fallback, which parks the pointer first. Nothing
    /// to bring over, and the focus stays with whatever macOS decides.
    #[test]
    fn a_space_switch_under_the_pointer_is_left_alone() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        crate::sys::window_server::set_cursor_location_override(Some(right.mid()));

        switch_right_display_to(&mut reactor, SpaceId::new(3));

        assert!(reactor.test_mouse_warps.is_empty());
        assert!(raise_manager_rx.try_recv().is_err());
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// A switch on the display that holds the key window is macOS's to
    /// finish: it activates a window on the new space, and that activation
    /// is followed like any other. Doing it here too would fight it.
    #[test]
    fn a_space_switch_on_the_key_windows_display_is_left_to_macos() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        crate::sys::window_server::set_cursor_location_override(Some(right.mid()));
        let left = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

        reactor.handle_event(space_state_event_with(
            vec![left, right],
            vec![Some(SpaceId::new(3)), Some(SpaceId::new(2))],
            |state| state.has_seen_display_set = true,
        ));

        assert!(reactor.test_mouse_warps.is_empty());
        assert!(raise_manager_rx.try_recv().is_err());
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// With the setting off, a cross-display switch changes nothing about
    /// where the pointer or the focus is.
    #[test]
    fn a_space_switch_on_the_other_display_does_nothing_when_the_setting_is_off() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        reactor.config.settings.mouse_follows_focus = false;

        switch_right_display_to(&mut reactor, SpaceId::new(3));

        assert!(reactor.test_mouse_warps.is_empty());
        assert!(raise_manager_rx.try_recv().is_err());
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// `switch-to-space` naming a space the other display is already showing
    /// — the one desktop a laptop keeps beside an external, say — switches
    /// nothing at all, so no space change ever arrives to be followed.
    /// The command finishes here instead: the space's window is focused and
    /// the pointer goes to it.
    #[test]
    fn switching_to_a_space_the_other_display_already_shows_arrives_on_it() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();
        let b = WindowId::new(1, 2);

        let outcome = reactor.arrive_on_already_shown_space(SpaceId::new(2), None);
        reactor.apply_event_outcome(outcome);

        let msg = raise_manager_rx.try_recv().expect("the command focuses the space's window").1;
        let raise_manager::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = msg else {
            panic!("unexpected raise manager event: {msg:?}");
        };
        let b_center = reactor.live_frame_for(b).unwrap().mid();
        assert_eq!(
            focus_window,
            Some((b, Some(b_center))),
            "the window last used on the space is focused, with the pointer warped onto it"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// The same command aimed at the space the user is already on has
    /// nothing to do: macOS is showing it on the display they are looking at.
    #[test]
    fn switching_to_the_space_already_under_the_user_arrives_nowhere() {
        let (_apps, mut reactor, mut raise_manager_rx) = two_displays_focused_on_left();

        let outcome = reactor.arrive_on_already_shown_space(SpaceId::new(1), None);
        reactor.apply_event_outcome(outcome);

        assert!(reactor.test_mouse_warps.is_empty());
        assert!(raise_manager_rx.try_recv().is_err());
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// An already-shown space with nothing on it takes the pointer to the
    /// middle of its display, the same as a switch that did change one.
    #[test]
    fn arriving_on_an_already_shown_empty_space_warps_to_its_display() {
        let (_apps, mut reactor, _raise_manager_rx) = two_displays_focused_on_left();
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        switch_right_display_to(&mut reactor, SpaceId::new(3));
        reactor.test_mouse_warps.clear();

        let outcome = reactor.arrive_on_already_shown_space(SpaceId::new(3), None);
        reactor.apply_event_outcome(outcome);

        assert_eq!(reactor.test_mouse_warps, vec![right.mid()]);
        crate::sys::window_server::set_cursor_location_override(None);
    }

    /// That centre warp is the pointer following focus, so it obeys the
    /// setting and stays put when the pointer is on that display already.
    #[test]
    fn arriving_on_an_already_shown_empty_space_leaves_the_pointer_alone() {
        let (_apps, mut reactor, _raise_manager_rx) = two_displays_focused_on_left();
        let right = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        switch_right_display_to(&mut reactor, SpaceId::new(3));
        reactor.test_mouse_warps.clear();
        reactor.config.settings.mouse_follows_focus = false;

        let outcome = reactor.arrive_on_already_shown_space(SpaceId::new(3), None);
        reactor.apply_event_outcome(outcome);
        assert!(reactor.test_mouse_warps.is_empty(), "the setting is off");

        reactor.config.settings.mouse_follows_focus = true;
        crate::sys::window_server::set_cursor_location_override(Some(right.mid()));
        let outcome = reactor.arrive_on_already_shown_space(SpaceId::new(3), None);
        reactor.apply_event_outcome(outcome);
        assert!(
            reactor.test_mouse_warps.is_empty(),
            "the pointer is on that display already"
        );
        crate::sys::window_server::set_cursor_location_override(None);
    }
}

mod floating_placement {
    use test_log::test;

    use super::*;

    /// A floating window with no stored frame yet — just launched, or its
    /// workspace was recreated — is laid out where it actually is, not in
    /// the middle of the screen.
    #[test]
    fn a_float_without_a_stored_frame_stays_where_it_is() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let frame = CGRect::new(CGPoint::new(1000., 600.), CGSize::new(300., 200.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), frame);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        reactor.layout_manager.layout_engine.remove_floating_position(wid);

        let gaps = reactor.config.settings.layout.gaps.clone();
        let placed = reactor
            .layout_manager
            .layout_engine
            .calculate_layout_with_virtual_workspaces(
                &reactor.state.windows,
                space,
                screen,
                &gaps,
                0.0,
                Default::default(),
                Default::default(),
                |q| reactor.state.windows.window(q).map(|w| w.frame_monotonic),
                &[screen],
            )
            .into_iter()
            .find(|(w, _)| *w == wid)
            .map(|(_, f)| f);
        assert_eq!(
            placed, None,
            "no write is owed: the window simply stays where it is (the old behaviour centred it)"
        );
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(frame),
            "the live frame seeded the store"
        );
    }

    /// A visible float is laid out at its live frame; the stored frame is
    /// only used when the float is parked off-screen.
    #[test]
    fn a_visible_float_is_laid_out_where_it_is_not_where_it_was_remembered() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let live = CGRect::new(CGPoint::new(1000., 600.), CGSize::new(300., 200.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), live);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        let remembered = CGRect::new(CGPoint::new(570., 350.), CGSize::new(300., 200.));
        // Remembered from an earlier workspace switch: no placement intent.
        reactor
            .layout_manager
            .layout_engine
            .store_floating_window_positions(space, &[(wid, remembered)]);
        let _ = workspace;

        let gaps = reactor.config.settings.layout.gaps.clone();
        let lay_out = |reactor: &mut Reactor, frame_of: &dyn Fn(WindowId) -> Option<CGRect>| {
            reactor
                .layout_manager
                .layout_engine
                .calculate_layout_with_virtual_workspaces(
                    &reactor.state.windows,
                    space,
                    screen,
                    &gaps,
                    0.0,
                    Default::default(),
                    Default::default(),
                    frame_of,
                    &[screen],
                )
                .into_iter()
                .find(|(w, _)| *w == wid)
                .map(|(_, f)| f)
        };
        assert_eq!(
            lay_out(&mut reactor, &|_| Some(live)),
            None,
            "visible: the live frame wins and following it is not a write"
        );
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(live),
            "the remembered frame followed the live one"
        );

        // Parked off-screen: the remembered frame is what brings it back.
        let parked = CGRect::new(CGPoint::new(-9000., -9000.), CGSize::new(300., 200.));
        assert_eq!(
            lay_out(&mut reactor, &|_| Some(parked)),
            Some(live),
            "hidden: the remembered frame (updated to the live one above) is used"
        );
    }

    /// A placement rift intends is laid out once; when the app answers with
    /// a different frame (it refused the size), the intent is spent and the
    /// window is left where the app put it — not re-placed forever.
    #[test]
    fn a_placement_the_app_answers_differently_is_not_reissued() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let before = CGRect::new(CGPoint::new(1000., 600.), CGSize::new(300., 200.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), before);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));

        let wanted = CGRect::new(CGPoint::new(420., 250.), CGSize::new(600., 400.));
        reactor
            .layout_manager
            .layout_engine
            .store_floating_position(space, workspace, wid, wanted);

        let gaps = reactor.config.settings.layout.gaps.clone();
        let lay_out = |reactor: &mut Reactor, live: CGRect| {
            reactor
                .layout_manager
                .layout_engine
                .calculate_layout_with_virtual_workspaces(
                    &reactor.state.windows,
                    space,
                    screen,
                    &gaps,
                    0.0,
                    Default::default(),
                    Default::default(),
                    |_| Some(live),
                    &[screen],
                )
                .into_iter()
                .find(|(w, _)| *w == wid)
                .map(|(_, f)| f)
        };
        assert_eq!(
            lay_out(&mut reactor, before),
            Some(wanted),
            "the intent is laid out"
        );
        assert_eq!(
            lay_out(&mut reactor, before),
            Some(wanted),
            "and held while the app has not answered"
        );

        // The app answered, but with its own idea of the size. The intent
        // is spent; following the answer is bookkeeping, not a write.
        let answered = CGRect::new(CGPoint::new(420., 250.), CGSize::new(900., 400.));
        assert_eq!(lay_out(&mut reactor, answered), None, "the answer stands");
        let moved = CGRect::new(CGPoint::new(50., 50.), CGSize::new(900., 400.));
        assert_eq!(
            lay_out(&mut reactor, moved),
            None,
            "and so does wherever it goes next"
        );
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(moved),
            "the stored frame followed the window"
        );
    }

    /// A float moved by something other than a drag keeps its new place:
    /// the stored frame follows the move instead of pulling it back.
    #[test]
    fn a_float_moved_without_a_drag_is_not_pulled_back_to_its_stored_frame() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let stored = CGRect::new(CGPoint::new(570., 350.), CGSize::new(300., 200.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), stored);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        reactor
            .layout_manager
            .layout_engine
            .store_floating_position(space, workspace, wid, stored);

        // The app moves its own window; no mouse involved.
        let moved = CGRect::new(CGPoint::new(1000., 600.), CGSize::new(300., 200.));
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            moved,
            None,
            Requested(false),
            Some(MouseState::Up),
        ));

        let gaps = reactor.config.settings.layout.gaps.clone();
        let placed = reactor
            .layout_manager
            .layout_engine
            .calculate_layout_with_virtual_workspaces(
                &reactor.state.windows,
                space,
                screen,
                &gaps,
                0.0,
                Default::default(),
                Default::default(),
                |q| reactor.state.windows.window(q).map(|w| w.frame_monotonic),
                &[screen],
            )
            .into_iter()
            .find(|(w, _)| *w == wid)
            .map(|(_, f)| f);
        assert_eq!(placed, None, "following the move is not a write");
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(moved),
            "the stored frame followed the move"
        );
    }

    /// A drop is an observation, not an intent. Storing the drop frame with
    /// a placement intent made the drop-arrange lay the float at the frame
    /// the *server* reported at release — stale during a tab-bar drag,
    /// whose AX reports outrun the server frame — snapping the window back
    /// out of the user's hands (snapping.trace).
    #[test]
    fn a_drop_follows_the_float_instead_of_replacing_it() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let drag_origin = CGRect::new(CGPoint::new(570., 350.), CGSize::new(300., 200.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), drag_origin);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));

        // The drag: the session's last noted frame lags at the origin (the
        // server never saw the tab-drag move), while the app's own reports
        // carried the window to where the user dropped it.
        reactor.ensure_active_drag(wid, &drag_origin);
        let dropped_at = CGRect::new(CGPoint::new(1000., 600.), CGSize::new(300., 200.));
        if let Some(window) = reactor.state.windows.window_mut(wid) {
            window.frame_monotonic = dropped_at;
        }

        let (visible_spaces, visible_space_centers) = reactor.visible_spaces_for_layout(true);
        crate::actor::reactor::events::drag::handle_mouse_up(
            &mut reactor.state,
            &mut reactor.layout_manager,
            &mut reactor.drag_manager,
            crate::actor::reactor::events::drag::MouseUpPayload {
                pending_swap: None,
                drop_action: None,
                swap_space: Some(space),
                final_space: Some(space),
                visible_spaces,
                visible_space_centers,
                screens: vec![],
                pointer: None,
            },
        )
        .unwrap();

        // The drop-arrange lays the float where it actually is, not at the
        // stale frame the drop happened to store.
        let gaps = reactor.config.settings.layout.gaps.clone();
        let placed = reactor
            .layout_manager
            .layout_engine
            .calculate_layout_with_virtual_workspaces(
                &reactor.state.windows,
                space,
                screen,
                &gaps,
                0.0,
                Default::default(),
                Default::default(),
                |q| reactor.state.windows.window(q).map(|w| w.frame_monotonic),
                &[screen],
            )
            .into_iter()
            .find(|(w, _)| *w == wid)
            .map(|(_, f)| f);
        assert_eq!(
            placed, None,
            "no write is owed: the float is already where the user left it"
        );
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(dropped_at),
            "and the stored position followed"
        );
    }

    /// A float dropped straddling the display seam is finished onto the
    /// display under the pointer. macOS does not let a programmatically
    /// moved window rest across the seam ("Displays have Separate Spaces")
    /// and relocates it to a display of its own choosing — the "random"
    /// snap on releasing a tab-bar drag near the seam (snap4/snap5.trace).
    #[test]
    fn a_seam_straddling_drop_is_finished_onto_the_pointer_display() {
        let mut reactor = test_reactor();
        let top = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let bottom = CGRect::new(CGPoint::new(100., 900.), CGSize::new(1512., 948.));
        let space = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        reactor.handle_event(space_state_event(vec![top, bottom], vec![
            Some(space),
            Some(space2),
        ]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        // 650..1050 across the seam at y=900, centre still on the top display.
        let straddling = CGRect::new(CGPoint::new(300., 650.), CGSize::new(600., 400.));
        reactor.add_test_window(wid, WindowServerId::new(101), Some(space), straddling);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        reactor.ensure_active_drag(wid, &straddling);

        let (visible_spaces, visible_space_centers) = reactor.visible_spaces_for_layout(true);
        let outcome = crate::actor::reactor::events::drag::handle_mouse_up(
            &mut reactor.state,
            &mut reactor.layout_manager,
            &mut reactor.drag_manager,
            crate::actor::reactor::events::drag::MouseUpPayload {
                pending_swap: None,
                drop_action: None,
                swap_space: Some(space),
                final_space: Some(space),
                visible_spaces,
                visible_space_centers,
                screens: vec![top, bottom],
                pointer: Some(CGPoint::new(500., 750.)),
            },
        )
        .unwrap();

        // macOS may allow the overhang (a server drag rests clipped at the
        // seam), so the drop is watched, not corrected: no write, the
        // stored position is the drop itself, and only a later relocation
        // triggers the deterministic placement.
        assert!(
            outcome.pre_layout_window_frame_writes.is_empty(),
            "a straddling drop is left where the user put it"
        );
        let finish = reactor.drag_manager.seam_finish.expect("the drop is watched");
        assert_eq!(finish.window, wid);
        assert!(
            finish.fitted.is_none(),
            "no correction until a relocation is seen"
        );
        assert_eq!(
            reactor
                .layout_manager
                .layout_engine
                .get_floating_position(space, workspace, wid),
            Some(straddling),
            "the stored position follows the drop"
        );
        // The correction, when needed, lands fully on the pointer's display.
        assert_eq!(
            crate::actor::reactor::events::drag::seam_fitted(
                &[top, bottom],
                Some(CGPoint::new(500., 750.)),
                straddling,
            ),
            Some(CGRect::new(CGPoint::new(300., 500.), straddling.size)),
        );
    }
}

mod admission {
    use test_log::test;

    use super::*;

    /// Focus on a window rift has never seen (its creation was missed) must
    /// not be rewritten onto an admitted sibling: sa+T on a fresh Preview
    /// document toggled the *previous* document, because the unknown window
    /// was "rooted" to it and the discovery that would have admitted the
    /// real window was skipped (preview-toggle.trace).
    #[test]
    fn focus_on_an_untracked_window_never_remaps_to_a_sibling() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let sibling = WindowId::new(1, 1);
        let frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(1200., 700.));
        reactor.add_test_window(sibling, WindowServerId::new(101), Some(space), frame);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, sibling, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, sibling));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::ApplicationActivated(1, Quiet::No));

        // The window server resolves focus to a window rift never tracked.
        let unknown = WindowId::new(1, 999);
        reactor.handle_event(Event::WindowServerFocusChanged(unknown, space));
        assert_eq!(
            reactor.main_window(),
            Some(unknown),
            "focus stays on the unknown window, not an admitted sibling"
        );

        // A focused-window command is a no-op, never a toggle of the sibling.
        reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
        assert!(
            !reactor.layout_manager.layout_engine.is_window_floating(sibling),
            "the admitted sibling must not be toggled in the unknown window's stead"
        );
    }

    /// A non-standard AX window (an app's panel) dragged with the mouse
    /// must not end up in a layout when the drag ends.
    #[test]
    fn a_drag_of_an_unmanageable_window_does_not_add_it_to_the_layout() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let panel = WindowId::new(1, 1);
        let frame = CGRect::new(CGPoint::new(300., 800.), CGSize::new(2000., 150.));
        reactor.add_test_window_with_manageability(
            panel,
            WindowServerId::new(101),
            Some(space),
            frame,
            false,
        );

        // The app reports the drag on the panel element.
        let moved = CGRect::new(CGPoint::new(320., 780.), CGSize::new(2000., 150.));
        reactor.handle_event(Event::WindowFrameChanged(
            panel,
            moved,
            None,
            Requested(false),
            Some(MouseState::Down),
        ));
        reactor.handle_event(Event::MouseUp);

        assert!(!has_window_in_layout(&mut reactor, space, screen, panel));
        assert!(!reactor.test_active_workspace_windows(space).contains(&panel));
        assert!(!reactor.layout_manager.layout_engine.is_window_floating(panel));

        // Nor by a plain WindowAdded from any other path.
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, panel));
        assert!(!has_window_in_layout(&mut reactor, space, screen, panel));
    }
}

mod child_window_focus {
    use test_log::test;

    use super::*;

    /// Focus reported on an app's non-admitted child window (Lightroom's
    /// filmstrip) counts as focus on the real window, so commands aimed at
    /// the focused window — tiling it with the toggle — work.
    #[test]
    fn focus_on_a_child_window_targets_the_real_window_for_commands() {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let main = WindowId::new(1, 1);
        let child = WindowId::new(1, 2);
        let main_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(1200., 700.));
        let child_frame = CGRect::new(CGPoint::new(100., 650.), CGSize::new(1200., 150.));
        reactor.add_test_window(main, WindowServerId::new(101), Some(space), main_frame);
        reactor.add_test_window_with_manageability(
            child,
            WindowServerId::new(102),
            Some(space),
            child_frame,
            false,
        );
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, main, workspace));
        // Floating, like everything under a float-by-default catch-all.
        reactor.layout_manager.layout_engine.mark_window_floating(main);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, main));
        assert!(!has_window_in_layout(&mut reactor, space, screen, main));

        // Focus from the Dock: the window server reports the child.
        reactor.handle_event(Event::WindowServerFocusChanged(child, space));
        assert_eq!(reactor.layout_manager.layout_engine.focused_window(), Some(main));

        // The toggle acts on the real window: it tiles.
        let _ = reactor.layout_manager.layout_engine.handle_command(
            &mut reactor.state.windows,
            Some(space),
            &[space],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::ToggleWindowFloating,
        );
        assert!(!reactor.layout_manager.layout_engine.is_window_floating(main));
        assert!(has_window_in_layout(&mut reactor, space, screen, main));
        assert!(!has_window_in_layout(&mut reactor, space, screen, child));
    }
}

mod fullscreen_slots {
    use test_log::test;

    use super::*;

    fn bsp_reactor_with_three_tiled()
    -> (Reactor, CGRect, SpaceId, [WindowId; 3], [WindowServerId; 3]) {
        let mut reactor = Reactor::new_for_test(LayoutEngine::new(
            &crate::common::config::VirtualWorkspaceSettings::default(),
            &crate::common::config::LayoutSettings {
                mode: LayoutMode::Bsp,
                ..crate::common::config::LayoutSettings::default()
            },
            None,
        ));
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let workspace = reactor.test_workspace(space, 0);
        let mut wids = [WindowId::new(1, 1); 3];
        let mut wsids = [WindowServerId::new(0); 3];
        for i in 0..3 {
            let wid = WindowId::new(1, i as u32 + 1);
            let wsid = WindowServerId::new(100 + i as u32 + 1);
            reactor.add_test_window(
                wid,
                wsid,
                Some(space),
                CGRect::new(CGPoint::new(10., 10.), CGSize::new(600., 400.)),
            );
            reactor.state.windows.window_mut(wid).unwrap().info.bundle_id =
                Some("com.test.app".to_string());
            assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
            reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
            wids[i] = wid;
            wsids[i] = wsid;
        }
        (reactor, screen, space, wids, wsids)
    }

    fn fullscreen_space(space: SpaceId) -> SpaceId { SpaceId::new(0x400000000 + space.get()) }

    /// Where `a` sits relative to `b`: sign of the x and y offsets.
    fn relation(layout: &[(WindowId, CGRect)], a: WindowId, b: WindowId) -> (i8, i8) {
        let frame =
            |w: WindowId| layout.iter().find(|(wid, _)| *wid == w).map(|(_, f)| *f).unwrap();
        let (fa, fb) = (frame(a), frame(b));
        (
            (fa.origin.x - fb.origin.x).signum() as i8,
            (fa.origin.y - fb.origin.y).signum() as i8,
        )
    }

    #[test]
    fn exit_puts_the_window_back_exactly_when_nothing_changed() {
        let (mut reactor, screen, space, [w1, w2, w3], [_, wsid2, _]) =
            bsp_reactor_with_three_tiled();
        // Make the tree non-default: w3 moves next to w1, and a split is resized.
        reactor.send_layout_event(LayoutEvent::WindowFocused(space, w3));
        let _ = reactor.layout_manager.layout_engine.handle_command(
            &mut reactor.state.windows,
            Some(space),
            &[space],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::MoveNode(Direction::Left),
        );
        reactor.send_layout_event(LayoutEvent::WindowFocused(space, w2));
        let _ = reactor.layout_manager.layout_engine.handle_command(
            &mut reactor.state.windows,
            Some(space),
            &[space],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::ResizeWindowGrow(crate::layout_engine::ResizeOrientation::Horizontal),
        );
        let before = test_layout(&mut reactor, space, screen);
        assert_eq!(before.len(), 3);

        window_server_appeared(
            &mut reactor,
            wsid2,
            fullscreen_space(space),
            SpaceEventKind::Fullscreen,
        );
        assert!(!has_window_in_layout(&mut reactor, space, screen, w2));
        assert_eq!(test_layout(&mut reactor, space, screen).len(), 2);

        window_server_appeared(&mut reactor, wsid2, space, SpaceEventKind::User);
        assert_eq!(
            test_layout(&mut reactor, space, screen),
            before,
            "structure, order and ratios must come back exactly"
        );
        let _ = (w1, w3);
    }

    #[test]
    fn exit_re_anchors_beside_the_old_neighbour_when_the_tree_changed() {
        let (mut reactor, screen, space, [w1, w2, w3], [_, wsid2, _]) =
            bsp_reactor_with_three_tiled();
        reactor.send_layout_event(LayoutEvent::WindowFocused(space, w3));
        let _ = reactor.layout_manager.layout_engine.handle_command(
            &mut reactor.state.windows,
            Some(space),
            &[space],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::MoveNode(Direction::Left),
        );
        let before = test_layout(&mut reactor, space, screen);
        let slot = reactor.layout_manager.layout_engine.slot_of(space, w2).expect("w2 has a slot");
        let neighbour = slot.anchor;
        let relation_before = relation(&before, w2, neighbour);

        window_server_appeared(
            &mut reactor,
            wsid2,
            fullscreen_space(space),
            SpaceEventKind::Fullscreen,
        );
        // Meanwhile the user closes a window that is not the neighbour.
        let closed = if neighbour == w1 { w3 } else { w1 };
        reactor.handle_event(Event::WindowDestroyed(closed));
        assert_eq!(test_layout(&mut reactor, space, screen).len(), 1);

        window_server_appeared(&mut reactor, wsid2, space, SpaceEventKind::User);
        let after = test_layout(&mut reactor, space, screen);
        assert_eq!(
            after.len(),
            2,
            "the closed window stays closed; the edit is kept"
        );
        assert!(after.iter().any(|(wid, _)| *wid == w2));
        assert_eq!(
            relation(&after, w2, neighbour),
            relation_before,
            "the window returns to the same side of its old neighbour"
        );
    }
}

#[test]
fn a_drop_on_a_target_lands_in_the_targets_space_however_the_window_hangs_over() {
    // Two displays stacked vertically, each with its own space.
    let mut reactor = Reactor::new_for_test(LayoutEngine::new(
        &crate::common::config::VirtualWorkspaceSettings::default(),
        &crate::common::config::LayoutSettings {
            mode: LayoutMode::Bsp,
            ..crate::common::config::LayoutSettings::default()
        },
        None,
    ));
    let top = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let bottom = CGRect::new(CGPoint::new(0., 900.), CGSize::new(1440., 900.));
    let (space1, space2) = (SpaceId::new(1), SpaceId::new(2));
    reactor.handle_event(space_state_event(vec![top, bottom], vec![
        Some(space1),
        Some(space2),
    ]));
    reactor.add_test_app(1);
    let workspace = reactor.test_workspace(space1, 0);
    let _ = reactor.test_workspace_ids(space2);

    let left = WindowId::new(1, 1);
    let dragged = WindowId::new(1, 2);
    let left_frame = CGRect::new(CGPoint::new(0., 0.), CGSize::new(720., 900.));
    let right_frame = CGRect::new(CGPoint::new(720., 0.), CGSize::new(720., 900.));
    for (wid, wsid, frame) in [(left, 101, left_frame), (dragged, 102, right_frame)] {
        reactor.add_test_window(wid, WindowServerId::new(wsid), Some(space1), frame);
        assert!(reactor.assign_test_window_to_workspace(space1, wid, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
    }

    // Dropped low on the top display, the window straddles the seam and the
    // drag believes it has settled on the lower display.
    let hanging = CGRect::new(CGPoint::new(100., 700.), CGSize::new(720., 900.));
    reactor.state.windows.window_mut(dragged).unwrap().frame_monotonic = hanging;
    reactor.drag_manager.drag_state = DragState::PendingSwap {
        session: DragSession {
            window: dragged,
            last_frame: hanging,
            origin_space: Some(space1),
            settled_space: Some(space2),
            layout_dirty: true,
        },
        target: left,
    };
    reactor.handle_event(Event::MouseUp);

    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));
    let check = |reactor: &Reactor, when: &str| {
        let engine = &reactor.layout_manager.layout_engine;
        assert!(
            engine.is_window_tiled(space1, dragged),
            "{when}: still tiled where it was dropped"
        );
        assert!(
            !engine.is_window_tiled(space2, dragged),
            "{when}: not moved to the display it hung over"
        );
        assert_eq!(
            reactor.assigned_space_for_window_id(dragged),
            Some(space1),
            "{when}"
        );
    };
    check(&reactor, "after the drop");

    // A beat later the window server reports what macOS did at the moment of
    // release: it handed the window to the lower display's space, and a live
    // query agrees because the arrange has not moved the frame yet. That
    // report used to win, tearing the window out of the tree it was just
    // dropped into.
    let dragged_wsid = WindowServerId::new(102);
    crate::sys::window_server::set_window_spaces_override(dragged_wsid, Some(vec![space2.get()]));
    reactor.handle_event(Event::WindowServerAppeared(
        dragged_wsid,
        space2,
        SpaceEventKind::User,
    ));
    check(&reactor, "after the window server's stale report");

    // Once the frame has landed the window server agrees, and the pin lets go.
    crate::sys::window_server::set_window_spaces_override(dragged_wsid, Some(vec![space1.get()]));
    reactor.handle_event(Event::WindowServerAppeared(
        dragged_wsid,
        space1,
        SpaceEventKind::User,
    ));
    assert!(
        reactor.drag_manager.drop_pin.is_none(),
        "pin released once the frame landed"
    );
    check(&reactor, "after the frame landed");
    crate::sys::window_server::set_window_spaces_override(dragged_wsid, None);
}

#[test]
fn a_window_that_refuses_its_size_is_given_at_least_that_size() {
    let (mut reactor, left, right, space, _) = reactor_with_two_tiled_windows(LayoutMode::Bsp);
    let screen = reactor.space_state.screen_by_space(space).unwrap().frame;
    let gaps = reactor.config.settings.layout.gaps.effective_for_display(None);
    let widths = |reactor: &mut Reactor| -> (f64, f64) {
        let frames: std::collections::HashMap<_, _> = reactor
            .layout_manager
            .layout_engine
            .calculate_layout(space, screen, &gaps, 0.0, Default::default(), Default::default())
            .into_iter()
            .collect();
        (frames[&left].size.width, frames[&right].size.width)
    };
    let (l0, r0) = widths(&mut reactor);
    assert!((l0 - r0).abs() < 1.0, "starts even: {l0} vs {r0}");

    // Asked for its half, it came back a thousand wide.
    let learnt = reactor.layout_manager.layout_engine.note_observed_min_size(
        right,
        CGSize::new(r0, 900.),
        CGSize::new(1000., 900.),
    );
    assert!(learnt);
    let (l1, r1) = widths(&mut reactor);
    assert!(r1 >= 999.0, "slot grows to the refused size: {r1}");
    assert!(l1 < l0, "the neighbour gives way: {l1}");

    // Nothing new to learn from the same refusal, so no further arranges.
    assert!(!reactor.layout_manager.layout_engine.note_observed_min_size(
        right,
        CGSize::new(r1, 900.),
        CGSize::new(1000., 900.),
    ));

    // Seen at 600 wide, the minimum was wrong and comes down.
    reactor
        .layout_manager
        .layout_engine
        .relax_observed_min_size(right, CGSize::new(600., 900.));
    let (l2, r2) = widths(&mut reactor);
    assert!((l2 - r2).abs() < 1.0, "even again: {l2} vs {r2}");
}

// ---------------------------------------------------------------------------
// Display archive: a display's layout survives the display going away.
// ---------------------------------------------------------------------------

mod display_archive {
    use test_log::test;

    use super::*;
    use crate::actor::spaces::TopologyWindowDelta;
    use crate::sys::scripting_addition::test_hooks as sa;
    use crate::sys::skylight::DisplayReconfigFlags;
    use crate::sys::window_server::{
        set_space_window_list_for_space_override, set_window_spaces_override,
    };

    const DISPLAY2: &str = "test-display-1";

    fn space1() -> SpaceId { SpaceId::new(1) }
    fn space2() -> SpaceId { SpaceId::new(2) }
    /// The space macOS hands the second display when it comes back.
    fn space2_returned() -> SpaceId { SpaceId::new(7) }
    /// A spare desktop of the second display that is never shown.
    fn space2_extra() -> SpaceId { SpaceId::new(9) }
    /// The fresh space macOS gives the first display once its own was destroyed.
    fn space1_returned() -> SpaceId { SpaceId::new(11) }

    fn screen1() -> CGRect { CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.)) }

    fn screen2() -> CGRect { CGRect::new(CGPoint::new(1440., 0.), CGSize::new(2560., 1440.)) }

    struct Fixture {
        reactor: Reactor,
        /// The window on the display that survives.
        survivor: WindowId,
        /// The windows on the display that departs, in tree order.
        exiled: Vec<WindowId>,
        exiled_wsids: Vec<WindowServerId>,
        /// `test_layout` of the departing display before anything happened.
        layout_before: Vec<(WindowId, CGRect)>,
    }

    /// Two displays; a browser alone on the first, three windows in a
    /// non-default arrangement on the second.
    fn fixture() -> Fixture {
        let mut reactor = test_reactor();
        // These tests were written for float mode; the ones for the default
        // `spaces` mode set it themselves.
        reactor.config.settings.displaced_windows = crate::common::config::DisplacedWindows::Float;
        // The second display also has a spare desktop, `space2_extra`, that is
        // never shown: rift lists workspaces for it but has no layout state.
        reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2())],
            |state| {
                state.display_space_ids.insert("test-display-0".to_string(), vec![space1()]);
                state
                    .display_space_ids
                    .insert(DISPLAY2.to_string(), vec![space2(), space2_extra()]);
                state.last_user_space_by_display.insert("test-display-0".to_string(), space1());
                state.last_user_space_by_display.insert(DISPLAY2.to_string(), space2());
            },
        ));
        reactor.add_test_app(1);
        let _ = reactor
            .layout_manager
            .layout_engine
            .virtual_workspace_manager_mut()
            .list_workspaces(space2_extra());

        let place = |reactor: &mut Reactor, idx: u32, space: SpaceId, frame: CGRect| {
            let wid = WindowId::new(1, idx);
            let wsid = WindowServerId::new(100 + idx);
            reactor.add_test_window(wid, wsid, Some(space), frame);
            // Real windows carry a bundle id, which is what lets a restore
            // recognise a window after another tree has resized it.
            reactor.state.windows.window_mut(wid).unwrap().info.bundle_id =
                Some("com.test.app".to_string());
            let workspace = reactor.test_workspace(space, 0);
            assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
            reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
            (wid, wsid)
        };
        let (survivor, _) = place(
            &mut reactor,
            1,
            space1(),
            CGRect::new(CGPoint::new(10., 10.), CGSize::new(800., 600.)),
        );
        let mut exiled = Vec::new();
        let mut exiled_wsids = Vec::new();
        for idx in 2..=4 {
            let (wid, wsid) = place(
                &mut reactor,
                idx,
                space2(),
                CGRect::new(CGPoint::new(1500., 10.), CGSize::new(800., 600.)),
            );
            exiled.push(wid);
            exiled_wsids.push(wsid);
        }
        let default_layout = test_layout(&mut reactor, space2(), screen2());
        reactor.send_layout_event(LayoutEvent::WindowFocused(space2(), exiled[2]));
        // Straight to the engine: the fixture has no main window for the
        // reactor to route a command through.
        let _ = reactor.layout_manager.layout_engine.handle_command(
            &mut reactor.state.windows,
            Some(space2()),
            &[space1(), space2()],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::MoveNode(Direction::Left),
        );
        let layout_before = test_layout(&mut reactor, space2(), screen2());
        assert_ne!(
            default_layout, layout_before,
            "fixture must build a non-default tree"
        );
        assert_eq!(layout_before.len(), 3);

        Fixture {
            reactor,
            survivor,
            exiled,
            exiled_wsids,
            layout_before,
        }
    }

    fn set_window_spaces(wsids: &[WindowServerId], space: SpaceId) {
        for wsid in wsids {
            set_window_spaces_override(*wsid, Some(vec![space.get()]));
        }
    }

    fn clear_overrides(wsids: &[WindowServerId]) {
        for wsid in wsids {
            set_window_spaces_override(*wsid, None);
        }
        for space in [space1(), space2(), space2_returned()] {
            set_space_window_list_for_space_override(space.get(), None);
        }
    }

    fn topology_event(
        screens: Vec<CGRect>,
        spaces: Vec<Option<SpaceId>>,
        moved: Vec<(WindowServerId, SpaceId, SpaceId)>,
        window_spaces: Vec<(WindowServerId, SpaceId)>,
    ) -> Event {
        space_state_event_with(screens, spaces, move |state| {
            state.has_seen_display_set = true;
            state.display_set_changed = true;
            state.topology_changed = true;
            state.allow_space_remap = true;
            state.should_force_refresh_layout = true;
            state.topology_window_delta = Some(TopologyWindowDelta {
                epoch: 1,
                flags: DisplayReconfigFlags::REMOVE | DisplayReconfigFlags::ADD,
                appeared: moved.iter().map(|(wsid, _, to)| (*wsid, *to)).collect(),
                disappeared: moved.iter().map(|(wsid, from, _)| (*wsid, *from)).collect(),
            });
            for (wsid, space) in window_spaces {
                state.active_window_spaces.insert(wsid, space);
            }
        })
    }

    /// The second display goes away and macOS drops its windows on the first.
    fn unplug(f: &mut Fixture) {
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&f.exiled_wsids, space1());
        set_space_window_list_for_space_override(
            space1().get(),
            Some(
                std::iter::once(survivor_wsid)
                    .chain(f.exiled_wsids.iter().copied())
                    .map(|wsid| wsid.as_u32())
                    .collect(),
            ),
        );
        let moved = f.exiled_wsids.iter().map(|wsid| (*wsid, space2(), space1())).collect();
        let window_spaces = std::iter::once((survivor_wsid, space1()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space1())))
            .collect();
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(space1())],
            moved,
            window_spaces,
        ));
    }

    /// The second display comes back with a new space. Its windows are still
    /// on the first display until the scripting addition has moved them.
    fn replug(f: &mut Fixture) {
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let window_spaces = std::iter::once((survivor_wsid, space1()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space1())))
            .collect();
        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2_returned())],
            Vec::new(),
            window_spaces,
        ));
    }

    /// The window server reports the exiled windows on the returned display.
    fn windows_land(f: &mut Fixture) {
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&f.exiled_wsids, space2_returned());
        set_space_window_list_for_space_override(
            space1().get(),
            Some(vec![survivor_wsid.as_u32()]),
        );
        set_space_window_list_for_space_override(
            space2_returned().get(),
            Some(f.exiled_wsids.iter().map(|wsid| wsid.as_u32()).collect()),
        );
        let moved =
            f.exiled_wsids.iter().map(|wsid| (*wsid, space1(), space2_returned())).collect();
        let window_spaces = std::iter::once((survivor_wsid, space1()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2_returned())))
            .collect();
        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2_returned())],
            moved,
            window_spaces,
        ));
    }

    #[test]
    fn unplugged_display_windows_float_over_the_survivor_without_reshaping_it() {
        let mut f = fixture();
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        assert_eq!(survivor_layout.len(), 1);

        unplug(&mut f);

        for wid in &f.exiled {
            assert_eq!(f.reactor.assigned_space_for_window_id(*wid), Some(space1()));
            assert!(
                f.reactor.layout_manager.layout_engine.is_window_floating(*wid),
                "{wid:?} should float on the surviving display"
            );
            assert!(!has_window_in_layout(&mut f.reactor, space1(), screen1(), *wid));
            assert_eq!(f.reactor.state.windows.user_floating(*wid), Some(true));
        }
        assert_eq!(
            test_layout(&mut f.reactor, space1(), screen1()),
            survivor_layout,
            "the surviving display's tree must be untouched"
        );
        assert!(f.reactor.display_archive.has(DISPLAY2));
        clear_overrides(&f.exiled_wsids);
    }

    #[test]
    fn returning_display_gets_its_windows_and_layout_back_on_its_new_space() {
        let mut f = fixture();
        sa::set_available(true);
        unplug(&mut f);
        assert!(sa::window_moves().is_empty(), "an ordinary unplug moves nothing");
        replug(&mut f);

        assert!(
            f.reactor.display_archive.is_homing(DISPLAY2),
            "windows are still on the survivor, so the restore must wait for them"
        );
        let expected_moves: Vec<(u32, u64)> = f
            .exiled_wsids
            .iter()
            .map(|wsid| (wsid.as_u32(), space2_returned().get()))
            .collect();
        let mut moves = sa::window_moves();
        moves.sort_unstable();
        assert_eq!(moves, expected_moves, "every exiled window is sent home");
        assert!(
            !has_window_in_layout(&mut f.reactor, space2_returned(), screen2(), f.exiled[0]),
            "nothing has landed yet"
        );

        windows_land(&mut f);

        assert!(
            f.reactor.display_archive.is_empty(),
            "restore should have completed"
        );
        for wid in &f.exiled {
            assert_eq!(
                f.reactor.assigned_space_for_window_id(*wid),
                Some(space2_returned())
            );
            assert!(!f.reactor.layout_manager.layout_engine.is_window_floating(*wid));
            assert!(has_window_in_layout(
                &mut f.reactor,
                space2_returned(),
                screen2(),
                *wid
            ));
            assert!(!has_window_in_layout(&mut f.reactor, space1(), screen1(), *wid));
            assert_eq!(
                f.reactor.state.windows.user_floating(*wid),
                Some(false),
                "restored tiled windows are pinned tiled against a catch-all floating rule"
            );
        }
        assert_eq!(
            test_layout(&mut f.reactor, space2_returned(), screen2()),
            f.layout_before,
            "the tree must come back exactly as it was, on the new space id"
        );
        assert_eq!(test_layout(&mut f.reactor, space1(), screen1()).len(), 1);
        assert_eq!(
            f.reactor.layout_manager.layout_engine.last_space_for_display_uuid(DISPLAY2),
            Some(space2_returned())
        );
        assert!(
            !f.reactor
                .layout_manager
                .layout_engine
                .virtual_workspace_manager()
                .initialized_spaces()
                .contains(&space2()),
            "the dead space's workspaces must be moved, not leaked"
        );
        clear_overrides(&f.exiled_wsids);
        sa::set_available(false);
    }

    #[test]
    fn display_returning_on_its_other_desktop_is_switched_back_to_the_layouts_desktop() {
        let mut f = fixture();
        sa::set_available(true);
        unplug(&mut f);
        // Back, but showing its spare desktop; the one the layout lived on is
        // still there behind it.
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let window_spaces: Vec<_> = std::iter::once((survivor_wsid, space1()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space1())))
            .collect();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2_extra())],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state
                    .display_space_ids
                    .insert(DISPLAY2.to_string(), vec![space2_extra(), space2()]);
                for (wsid, space) in window_spaces {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert!(f.reactor.display_archive.is_homing(DISPLAY2));
        let mut moves = sa::window_moves();
        moves.sort_unstable();
        let expected: Vec<(u32, u64)> =
            f.exiled_wsids.iter().map(|wsid| (wsid.as_u32(), space2().get())).collect();
        assert_eq!(
            moves, expected,
            "windows go to the desktop the layout was built on"
        );
        assert_eq!(
            sa::space_focuses(),
            vec![space2().get()],
            "and the display is switched to it"
        );
        clear_overrides(&f.exiled_wsids);
        sa::set_available(false);
    }

    #[test]
    fn a_displaced_window_the_user_handles_is_adopted_by_the_display_it_is_on() {
        let mut f = fixture();
        sa::set_available(true);
        unplug(&mut f);
        let adopted = f.exiled[1];
        let adopted_wsid = f.exiled_wsids[1];

        // A deliberate swap with the survivor's window: intent about the
        // displaced window, on the display it is now shown on.
        f.reactor.handle_event(Event::Command(crate::model::reactor::Command::Layout(
            LayoutCommand::SwapWindows(adopted.into(), f.survivor.into()),
        )));

        replug(&mut f);
        let mut moves = sa::window_moves();
        moves.sort_unstable();
        let expected: Vec<(u32, u64)> = f
            .exiled_wsids
            .iter()
            .filter(|wsid| **wsid != adopted_wsid)
            .map(|wsid| (wsid.as_u32(), space2_returned().get()))
            .collect();
        assert_eq!(moves, expected, "the adopted window is not sent back");

        // The others land.
        let others: Vec<WindowServerId> =
            f.exiled_wsids.iter().copied().filter(|wsid| *wsid != adopted_wsid).collect();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&others, space2_returned());
        set_space_window_list_for_space_override(
            space1().get(),
            Some(vec![survivor_wsid.as_u32(), adopted_wsid.as_u32()]),
        );
        set_space_window_list_for_space_override(
            space2_returned().get(),
            Some(others.iter().map(|wsid| wsid.as_u32()).collect()),
        );
        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2_returned())],
            others.iter().map(|wsid| (*wsid, space1(), space2_returned())).collect(),
            std::iter::once((survivor_wsid, space1()))
                .chain(std::iter::once((adopted_wsid, space1())))
                .chain(others.iter().map(|wsid| (*wsid, space2_returned())))
                .collect(),
        ));

        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(adopted), Some(space1()));
        assert!(!has_window_in_layout(
            &mut f.reactor,
            space2_returned(),
            screen2(),
            adopted
        ));
        let restored = test_layout(&mut f.reactor, space2_returned(), screen2());
        assert_eq!(
            restored.len(),
            2,
            "the restored tree has a slot for each window that came home"
        );
        assert!(restored.iter().all(|(wid, _)| *wid != adopted));
        clear_overrides(&f.exiled_wsids);
        sa::set_available(false);
    }

    #[test]
    fn a_shown_space_without_a_layout_is_exposed_even_when_its_size_did_not_change() {
        let mut f = fixture();
        // The aftermath of a remap: space 2's workspaces now live under
        // another id, and space 2 itself has none.
        f.reactor.layout_manager.layout_engine.remap_space(
            &mut f.reactor.state.windows,
            space2(),
            SpaceId::new(99),
        );
        assert!(!f.reactor.layout_manager.layout_engine.has_active_layout(space2()));

        // The same screens again, same sizes: nothing "resized".
        f.reactor.handle_event(space_state_event(vec![screen1(), screen2()], vec![
            Some(space1()),
            Some(space2()),
        ]));
        assert!(
            f.reactor.layout_manager.layout_engine.has_active_layout(space2()),
            "a shown space must always have a layout to tile into"
        );
    }

    #[test]
    fn homing_does_not_remap_a_space_another_display_still_lists() {
        let mut f = fixture();
        sa::set_available(true);
        unplug(&mut f);
        // The second display returns on a fresh space, but its old space is
        // now one of the first display's desktops.
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let window_spaces: Vec<_> = std::iter::once((survivor_wsid, space1()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space1())))
            .collect();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2_returned())],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space1(), space2()]);
                state.display_space_ids.insert(DISPLAY2.to_string(), vec![space2_returned()]);
                for (wsid, space) in window_spaces {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert!(
            f.reactor
                .layout_manager
                .layout_engine
                .virtual_workspace_manager()
                .initialized_spaces()
                .contains(&space2()),
            "space 2 still belongs to a display and keeps its workspaces"
        );
        windows_land(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            test_layout(&mut f.reactor, space2_returned(), screen2()),
            f.layout_before
        );
        clear_overrides(&f.exiled_wsids);
        sa::set_available(false);
    }

    #[test]
    fn tile_mode_clusters_the_departed_windows_into_the_survivor() {
        let mut f = fixture();
        f.reactor.config.settings.displaced_windows = crate::common::config::DisplacedWindows::Tile;

        unplug(&mut f);

        for wid in &f.exiled {
            assert!(!f.reactor.layout_manager.layout_engine.is_window_floating(*wid));
            assert!(has_window_in_layout(&mut f.reactor, space1(), screen1(), *wid));
        }
        assert_eq!(test_layout(&mut f.reactor, space1(), screen1()).len(), 4);
        clear_overrides(&f.exiled_wsids);
    }

    #[test]
    fn spaces_mode_is_the_default() {
        assert_eq!(
            crate::common::config::DisplacedWindows::default(),
            crate::common::config::DisplacedWindows::Spaces
        );
    }

    // ---- spaces mode: one record at departure, one reconciliation at return.
    // Each test scripts what the window server does and asserts only the
    // end state: every window on the desktop the record has for it, or where
    // the user put it, desktops on their own display, layouts back.

    fn spaces_fixture() -> Fixture {
        let mut f = fixture();
        f.reactor.config.settings.displaced_windows =
            crate::common::config::DisplacedWindows::Spaces;
        sa::set_available(true);
        f
    }

    fn managed(displays: Vec<(&str, Vec<SpaceId>)>) {
        crate::sys::screen::set_managed_display_spaces_override(Some(
            displays
                .into_iter()
                .map(|(uuid, spaces)| crate::sys::screen::ManagedDisplaySpaces {
                    display_uuid: uuid.to_string(),
                    spaces,
                })
                .collect(),
        ));
    }

    fn spaces_cleanup(f: &Fixture, extra: &[WindowServerId]) {
        set_window_spaces_override(f.reactor.test_window_server_id(f.survivor), None);
        for wsid in extra {
            set_window_spaces_override(*wsid, None);
        }
        clear_overrides(&f.exiled_wsids);
        crate::sys::screen::set_managed_display_spaces_override(None);
        sa::set_next_created_space(None);
        sa::set_available(false);
    }

    /// The main display departs: macOS hands its desktops to the survivor,
    /// destroys the survivor's own desktop and merges its window in among
    /// the visitors. `windows` is where the window server has every window
    /// as the change is seen.
    /// `live` is what the window server lists for the survivor once the
    /// addition has made whatever it makes.
    fn takeover(
        f: &mut Fixture,
        survivor_desktops: Vec<SpaceId>,
        live: Vec<SpaceId>,
        windows: Vec<(WindowServerId, SpaceId)>,
    ) {
        managed(vec![("test-display-0", live)]);
        let mut screens = make_screen_snapshots(vec![screen1()], vec![Some(space2())]);
        screens[0].display_uuid = "test-display-0".to_string();
        for (wsid, space) in &windows {
            set_window_spaces(&[*wsid], *space);
        }
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state.display_space_ids.insert("test-display-0".to_string(), survivor_desktops);
                for (wsid, space) in windows {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
    }

    /// The main display returns. `laptop` and `lg` are what the window
    /// server lists for each, `shown` what each shows, `windows` where it
    /// has every window.
    fn replug_after_takeover(
        f: &mut Fixture,
        laptop: Vec<SpaceId>,
        lg: Vec<SpaceId>,
        shown: (SpaceId, SpaceId),
        windows: Vec<(WindowServerId, SpaceId)>,
    ) {
        managed(vec![("test-display-0", laptop.clone()), (DISPLAY2, lg.clone())]);
        for (wsid, space) in &windows {
            set_window_spaces(&[*wsid], *space);
        }
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(shown.0), Some(shown.1)],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state.display_space_ids.insert("test-display-0".to_string(), laptop);
                state.display_space_ids.insert(DISPLAY2.to_string(), lg);
                for (wsid, space) in windows {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
    }

    /// The window server reports every window where `windows` says.
    fn land(f: &mut Fixture, shown: (SpaceId, SpaceId), windows: Vec<(WindowServerId, SpaceId)>) {
        let before: Vec<(WindowServerId, Option<SpaceId>)> = windows
            .iter()
            .map(|(wsid, _)| {
                let wid = f.reactor.state.windows.tracked_window_id(*wsid);
                (
                    *wsid,
                    wid.and_then(|wid| f.reactor.assigned_space_for_window_id(wid)),
                )
            })
            .collect();
        for (wsid, space) in &windows {
            set_window_spaces(&[*wsid], *space);
        }
        let moved = before
            .into_iter()
            .zip(windows.iter())
            .filter_map(|((wsid, from), (_, to))| {
                from.filter(|from| from != to).map(|from| (wsid, from, *to))
            })
            .collect();
        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(shown.0), Some(shown.1)],
            moved,
            windows,
        ));
    }

    fn sorted_moves() -> Vec<(u32, u64)> {
        let mut moves = sa::window_moves();
        moves.sort_unstable();
        moves
    }

    #[test]
    fn spaces_mode_puts_a_destroyed_departed_desktop_back_on_replug() {
        let mut f = spaces_fixture();
        // The second display goes and macOS destroys its desktop outright,
        // dumping its windows on the survivor.
        unplug(&mut f);
        assert!(
            sa::window_moves().is_empty(),
            "nothing is moved while the display is away"
        );
        assert!(f.reactor.display_archive.record().is_some());

        // It comes back with a fresh desktop.
        managed(vec![
            ("test-display-0", vec![space1()]),
            (DISPLAY2, vec![space2_returned()]),
        ]);
        replug(&mut f);
        let expected: Vec<(u32, u64)> = f
            .exiled_wsids
            .iter()
            .map(|wsid| (wsid.as_u32(), space2_returned().get()))
            .collect();
        assert_eq!(
            sorted_moves(),
            expected,
            "every exiled window is sent to the fresh desktop"
        );

        windows_land(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        for wid in &f.exiled {
            assert_eq!(
                f.reactor.assigned_space_for_window_id(*wid),
                Some(space2_returned())
            );
        }
        assert_eq!(
            f.reactor.assigned_space_for_window_id(f.survivor),
            Some(space1())
        );
        assert_eq!(
            test_layout(&mut f.reactor, space2_returned(), screen2()),
            f.layout_before,
            "the departed display's layout is back on its fresh desktop"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn spaces_mode_puts_the_survivors_window_back_after_a_takeover() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        let made = SpaceId::new(23);
        sa::set_next_created_space(Some(made.get()));

        // The churn merges the survivor's window into the main space before
        // the display change is seen, and rift follows it.
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra(), made],
            merged.clone(),
        );

        // Settled: a desktop made for the survivor's window, put first with
        // the visitors walked behind it, the window sent there, and the
        // survivor switched to it, since that is what it was showing.
        assert_eq!(
            sa::space_creations(),
            vec![space2_extra().get()],
            "made after the last visitor"
        );
        assert_eq!(sa::window_moves(), vec![(survivor_wsid.as_u32(), made.get())]);
        assert_eq!(
            sa::space_moves(),
            vec![
                (space2().get(), made.get()),
                (space2_extra().get(), space2().get())
            ],
            "the visitors are walked behind the made desktop, in order"
        );
        assert_eq!(sa::space_focuses(), vec![made.get()]);
        for wid in &f.exiled {
            assert_eq!(
                f.reactor.assigned_space_for_window_id(*wid),
                Some(space2()),
                "the visitors keep their desktop"
            );
        }
        // The window arrives; the destroyed desktop's tree is put back on
        // the made one.
        set_window_spaces(&[survivor_wsid], made);
        let on_made: Vec<_> = std::iter::once((survivor_wsid, made))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(made)],
            vec![(survivor_wsid, space2(), made)],
            on_made,
        ));
        assert!(
            f.reactor.display_archive.record().is_some(),
            "the record lives on while the display is away"
        );
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(made));
        assert_eq!(
            test_layout(&mut f.reactor, made, screen1()),
            survivor_layout,
            "the survivor's layout is on its made desktop"
        );

        // Back: macOS takes the made desktop along with the main display's
        // and gives the survivor a fresh one; the main display comes back
        // showing a different desktop than it left on — the made one, first
        // in the list it took along — and the switch back that rift orders
        // has not landed by the time the return is over.
        let fresh = SpaceId::new(13);
        crate::sys::screen::set_current_space_override(DISPLAY2, Some(made.get()));
        let moves_before = sa::window_moves().len();
        let focuses_before = sa::space_focuses().len();
        let space_moves_before = sa::space_moves().len();
        let back: Vec<_> = std::iter::once((survivor_wsid, made))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra(), made],
            (fresh, space2_extra()),
            back,
        );
        assert_eq!(
            &sa::window_moves()[moves_before..],
            &[(survivor_wsid.as_u32(), fresh.get())],
            "only the survivor's window moves, to the fresh desktop"
        );
        assert_eq!(
            sa::space_moves().len(),
            space_moves_before,
            "the made desktop is not mistaken for one of the user's"
        );
        assert!(
            sa::space_focuses()[focuses_before..].contains(&space2().get()),
            "the main display is switched back to the desktop it was showing"
        );

        let landed: Vec<_> = std::iter::once((survivor_wsid, fresh))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        land(&mut f, (fresh, space2()), landed);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(fresh));
        assert_eq!(
            test_layout(&mut f.reactor, fresh, screen1()),
            survivor_layout,
            "the destroyed desktop's layout is on the fresh one"
        );
        assert_eq!(
            test_layout(&mut f.reactor, space2(), screen2()),
            f.layout_before,
            "the main display's layout is intact"
        );
        assert!(
            sa::space_destroys().is_empty(),
            "the made desktop is not destroyed while the main display still shows it"
        );
        // The switch lands; the next space change retires it.
        crate::sys::screen::set_current_space_override(DISPLAY2, None);
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(fresh), Some(space2())],
            |state| {
                state.has_seen_display_set = true;
            },
        ));
        assert_eq!(
            sa::space_destroys(),
            vec![made.get()],
            "the made desktop is destroyed once nothing shows it"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn spaces_mode_restores_the_layout_of_a_window_macos_put_back_itself() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra()],
            merged,
        );

        // Back: the window is already on the fresh desktop, and rift has
        // already filed it under a workspace it made for that desktop.
        let fresh = SpaceId::new(13);
        let workspace = f.reactor.test_workspace(fresh, 0);
        assert!(f.reactor.assign_test_window_to_workspace(fresh, f.survivor, workspace));
        let back: Vec<_> = std::iter::once((survivor_wsid, fresh))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra()],
            (fresh, space2()),
            back.clone(),
        );
        assert!(sa::window_moves().is_empty(), "nothing needs moving");
        if !f.reactor.display_archive.is_empty() {
            land(&mut f, (fresh, space2()), back);
        }
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(fresh));
        assert_eq!(
            test_layout(&mut f.reactor, fresh, screen1()),
            survivor_layout,
            "the destroyed desktop's layout is restored around the window macOS put back"
        );
        spaces_cleanup(&f, &[]);
    }

    /// A desktop the user rearranges while the display is away keeps the
    /// arrangement when the display is back, scaled to that screen, while
    /// an untouched desktop gets its tree from the record. The command puts
    /// the tree from the departure back on demand.
    #[test]
    fn spaces_mode_keeps_a_desktop_rearranged_while_away_and_can_put_it_back() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra()],
            merged,
        );

        // Shown on the laptop, the main display's desktop is rearranged:
        // its first window is moved right, past the next of its own.
        let survivor = f.survivor;
        let order = move |layout: &[(WindowId, CGRect)]| -> Vec<WindowId> {
            layout.iter().map(|(wid, _)| *wid).filter(|wid| *wid != survivor).collect()
        };
        let first = order(&f.layout_before)[0];
        f.reactor.send_layout_event(LayoutEvent::WindowFocused(space2(), first));
        let _ = f.reactor.layout_manager.layout_engine.handle_command(
            &mut f.reactor.state.windows,
            Some(space2()),
            &[space2()],
            &crate::common::collections::HashMap::default(),
            LayoutCommand::MoveNode(Direction::Right),
        );
        let rearranged = order(&test_layout(&mut f.reactor, space2(), screen2()));
        assert_ne!(
            rearranged,
            order(&f.layout_before),
            "the rearrangement must change the order"
        );

        let fresh = SpaceId::new(13);
        let now: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra()],
            (fresh, space2()),
            now,
        );
        let landed: Vec<_> = std::iter::once((survivor_wsid, fresh))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        land(&mut f, (fresh, space2()), landed);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            order(&test_layout(&mut f.reactor, space2(), screen2())),
            rearranged,
            "the rearranged desktop goes back to its display as the user left it"
        );

        // On demand, the tree from the departure is put back.
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(fresh), Some(space2())],
            |state| {
                state.has_seen_display_set = true;
                state.command_space = Some(space2());
            },
        ));
        f.reactor.handle_event(Event::Command(super::Command::Reactor(
            super::ReactorCommand::RestoreDepartureLayout,
        )));
        assert_eq!(
            test_layout(&mut f.reactor, space2(), screen2()),
            f.layout_before,
            "the departure's layout is back"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn spaces_mode_leaves_a_window_where_the_user_put_it_while_away() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra()],
            merged,
        );

        // Well after the churn, the user drags one of the visitors to the
        // other visiting desktop.
        let moved = f.exiled[0];
        let moved_wsid = f.exiled_wsids[0];
        f.reactor
            .display_archive
            .record_mut()
            .unwrap()
            .backdate(std::time::Duration::from_secs(30));
        set_window_spaces(&[moved_wsid], space2_extra());
        f.reactor.handle_event(Event::WindowServerAppeared(
            moved_wsid,
            space2_extra(),
            SpaceEventKind::User,
        ));
        assert_eq!(
            f.reactor.display_archive.record().unwrap().recorded_desktop(moved),
            Some(space2_extra()),
            "the record follows the user"
        );

        let fresh = SpaceId::new(13);
        let now: Vec<_> = [(survivor_wsid, space2()), (moved_wsid, space2_extra())]
            .into_iter()
            .chain(f.exiled_wsids[1..].iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra()],
            (fresh, space2()),
            now.clone(),
        );
        assert_eq!(
            sa::window_moves(),
            vec![(survivor_wsid.as_u32(), fresh.get())],
            "the window the user moved is left where it is"
        );

        let landed: Vec<_> = now
            .into_iter()
            .map(|(wsid, space)| {
                if wsid == survivor_wsid {
                    (wsid, fresh)
                } else {
                    (wsid, space)
                }
            })
            .collect();
        land(&mut f, (fresh, space2()), landed);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            f.reactor.assigned_space_for_window_id(moved),
            Some(space2_extra())
        );
        spaces_cleanup(&f, &[]);
    }

    /// With two desktops on the survivor, macOS destroys the *first* and
    /// carries the second along; on replug it lists the fresh one first,
    /// may bring the display back showing the kept one, sends the kept one
    /// along with the main display, dumps the kept desktop's window on one
    /// of the main display's, and puts the destroyed desktop's window on
    /// the fresh one itself. All of it is put right.
    #[test]
    fn spaces_mode_puts_a_kept_desktop_and_its_window_back_however_macos_scattered_them() {
        let mut f = spaces_fixture();
        let extra = SpaceId::new(15);
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(extra), Some(space2())],
            |state| {
                state.has_seen_display_set = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space1(), extra]);
                state
                    .display_space_ids
                    .insert(DISPLAY2.to_string(), vec![space2(), space2_extra()]);
                state.last_user_space_by_display.insert("test-display-0".to_string(), extra);
            },
        ));
        let kept_window = WindowId::new(1, 9);
        let kept_wsid = WindowServerId::new(109);
        f.reactor.add_test_window(
            kept_window,
            kept_wsid,
            Some(extra),
            CGRect::new(CGPoint::new(20., 20.), CGSize::new(600., 400.)),
        );
        let kept_workspace = f.reactor.test_workspace(extra, 0);
        assert!(f.reactor.assign_test_window_to_workspace(extra, kept_window, kept_workspace));
        f.reactor.send_layout_event(LayoutEvent::WindowAdded(extra, kept_window));

        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = [(survivor_wsid, space2()), (kept_wsid, extra)]
            .into_iter()
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra(), extra],
            vec![space2(), space2_extra(), extra],
            merged,
        );
        assert!(
            sa::window_moves().is_empty(),
            "the survivor's window stays put while the display is away"
        );
        assert_eq!(
            sa::space_moves(),
            vec![
                (space2().get(), extra.get()),
                (space2_extra().get(), space2().get())
            ],
            "the kept desktop is put ahead of the visitors"
        );
        let space_moves_before = sa::space_moves().len();

        let fresh = SpaceId::new(17);
        let scattered: Vec<_> = [(survivor_wsid, fresh), (kept_wsid, space2_extra())]
            .into_iter()
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra(), extra],
            (fresh, space2()),
            scattered,
        );
        assert_eq!(
            &sa::space_moves()[space_moves_before..],
            &[(extra.get(), fresh.get())],
            "the kept desktop comes back behind the fresh one"
        );
        assert_eq!(
            sa::window_moves(),
            vec![(kept_wsid.as_u32(), extra.get())],
            "the dumped window goes back; the one macOS put on the fresh desktop is not sent again"
        );
        assert!(
            sa::space_focuses().contains(&extra.get()),
            "the survivor is switched back to the desktop it was showing"
        );

        let landed: Vec<_> = [(survivor_wsid, fresh), (kept_wsid, extra)]
            .into_iter()
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        land(&mut f, (fresh, space2()), landed);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(fresh));
        assert_eq!(f.reactor.assigned_space_for_window_id(kept_window), Some(extra));
        assert_eq!(test_layout(&mut f.reactor, fresh, screen1()), survivor_layout);
        assert!(has_window_in_layout(
            &mut f.reactor,
            extra,
            screen1(),
            kept_window
        ));
        assert_eq!(test_layout(&mut f.reactor, space2(), screen2()), f.layout_before);
        spaces_cleanup(&f, &[kept_wsid]);
    }

    #[test]
    fn spaces_mode_brings_a_desktop_made_while_away_back_to_the_survivor() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra()],
            merged,
        );

        // While away the user makes a desktop and puts a visitor on it.
        let made = SpaceId::new(21);
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(made)],
            |state| {
                state.has_seen_display_set = true;
                state.display_space_ids.insert("test-display-0".to_string(), vec![
                    space2(),
                    space2_extra(),
                    made,
                ]);
            },
        ));
        let moved = f.exiled[0];
        let moved_wsid = f.exiled_wsids[0];
        f.reactor
            .display_archive
            .record_mut()
            .unwrap()
            .backdate(std::time::Duration::from_secs(30));
        set_window_spaces(&[moved_wsid], made);
        f.reactor.handle_event(Event::WindowServerAppeared(
            moved_wsid,
            made,
            SpaceEventKind::User,
        ));

        // Back: macOS files the made desktop with the main display's.
        let fresh = SpaceId::new(13);
        let now: Vec<_> = [(survivor_wsid, space2()), (moved_wsid, made)]
            .into_iter()
            .chain(f.exiled_wsids[1..].iter().map(|wsid| (*wsid, space2())))
            .collect();
        replug_after_takeover(
            &mut f,
            vec![fresh],
            vec![space2(), space2_extra(), made],
            (fresh, space2()),
            now.clone(),
        );
        assert_eq!(
            sa::space_moves(),
            vec![(made.get(), fresh.get())],
            "the made desktop is the survivor's"
        );
        assert_eq!(
            sa::window_moves(),
            vec![(survivor_wsid.as_u32(), fresh.get())],
            "the window on it stays on it"
        );

        let landed: Vec<_> = now
            .into_iter()
            .map(|(wsid, space)| {
                if wsid == survivor_wsid {
                    (wsid, fresh)
                } else {
                    (wsid, space)
                }
            })
            .collect();
        land(&mut f, (fresh, space2()), landed);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(moved), Some(made));
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn display_that_blinks_back_with_the_same_space_is_left_alone() {
        let mut f = fixture();
        // Off screen, but its windows never moved.
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(space1())],
            Vec::new(),
            vec![(f.reactor.test_window_server_id(f.survivor), space1())],
        ));
        assert!(f.reactor.display_archive.has(DISPLAY2));

        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(space1()), Some(space2())],
            Vec::new(),
            std::iter::once((f.reactor.test_window_server_id(f.survivor), space1()))
                .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
                .collect(),
        ));

        assert!(f.reactor.display_archive.is_empty());
        for wid in &f.exiled {
            assert!(!f.reactor.layout_manager.layout_engine.is_window_floating(*wid));
            assert_eq!(f.reactor.state.windows.user_floating(*wid), Some(false));
        }
        assert_eq!(test_layout(&mut f.reactor, space2(), screen2()), f.layout_before);
    }

    #[test]
    fn main_display_space_taken_over_by_the_survivor_parks_windows_on_its_own_space() {
        let mut f = fixture();
        sa::set_available(true);
        // The second display was the main one: macOS migrates its space onto
        // the first display and parks the first display's own space behind it.
        let mut screens = make_screen_snapshots(vec![screen1()], vec![Some(space2())]);
        screens[0].display_uuid = "test-display-0".to_string();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                // The survivor inherits every desktop the departed display
                // had, listed first, and has already been handed the
                // taken-over one as its "last user space".
                state.display_space_ids.insert("test-display-0".to_string(), vec![
                    space2(),
                    space2_extra(),
                    space1(),
                ]);
                state.last_user_space_by_display.insert("test-display-0".to_string(), space2());
            },
        ));

        assert!(f.reactor.display_archive.has(DISPLAY2));
        // The departed display's windows were sent to the survivor's own
        // space, as floats, and the survivor was switched back to it.
        let expected_moves: Vec<(u32, u64)> =
            f.exiled_wsids.iter().map(|wsid| (wsid.as_u32(), space1().get())).collect();
        let mut moves = sa::window_moves();
        moves.sort_unstable();
        assert_eq!(moves, expected_moves);
        assert_eq!(sa::space_focuses(), vec![space1().get()]);
        for wid in &f.exiled {
            assert!(f.reactor.layout_manager.layout_engine.is_window_floating(*wid));
            assert_eq!(f.reactor.state.windows.user_floating(*wid), Some(true));
        }
        sa::set_available(false);
    }

    #[test]
    fn takeover_that_destroys_the_survivors_space_brings_its_windows_back_on_replug() {
        let mut f = fixture();
        sa::set_available(true);
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());

        // The window server moves the survivor's window into the main space
        // *before* the display change is seen, and rift follows it.
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        assert_eq!(
            f.reactor.assigned_space_for_window_id(f.survivor),
            Some(space2())
        );
        assert_eq!(
            test_layout(&mut f.reactor, space1(), screen1()).len(),
            0,
            "the survivor's tree has already been emptied by the time the display change arrives"
        );

        // The second (main) display departs; its space lands on the first
        // display, whose own space macOS destroys, merging its window in.
        let mut screens = make_screen_snapshots(vec![screen1()], vec![Some(space2())]);
        screens[0].display_uuid = "test-display-0".to_string();
        let merged: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in merged {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert!(f.reactor.display_archive.has(DISPLAY2));
        assert!(
            f.reactor.display_archive.has("test-display-0"),
            "the survivor's destroyed space is archived too"
        );
        assert!(
            sa::window_moves().is_empty(),
            "nowhere to park, so nothing moves yet"
        );
        assert!(
            !f.reactor.display_archive.is_homing("test-display-0"),
            "the survivor waits for the other display"
        );

        // Replug: the main display takes its space back, and the first
        // display gets a fresh one. Every window is still on the main space.
        let all_on_main: Vec<_> = std::iter::once((survivor_wsid, space2()))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
            .collect();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen2()],
            vec![Some(space1_returned()), Some(space2())],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space1_returned()]);
                state
                    .display_space_ids
                    .insert(DISPLAY2.to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in all_on_main {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert_eq!(
            sa::window_moves(),
            vec![(survivor_wsid.as_u32(), space1_returned().get())],
            "only the survivor's own window is sent to its new space"
        );
        assert!(
            !f.reactor.display_archive.has(DISPLAY2),
            "the main display came back intact"
        );

        // It lands.
        set_window_spaces(&[survivor_wsid], space1_returned());
        f.reactor.handle_event(topology_event(
            vec![screen1(), screen2()],
            vec![Some(space1_returned()), Some(space2())],
            vec![(survivor_wsid, space2(), space1_returned())],
            std::iter::once((survivor_wsid, space1_returned()))
                .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, space2())))
                .collect(),
        ));
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            f.reactor.assigned_space_for_window_id(f.survivor),
            Some(space1_returned())
        );
        assert!(has_window_in_layout(
            &mut f.reactor,
            space1_returned(),
            screen1(),
            f.survivor
        ));
        assert_eq!(
            test_layout(&mut f.reactor, space1_returned(), screen1()),
            survivor_layout
        );
        assert_eq!(test_layout(&mut f.reactor, space2(), screen2()), f.layout_before);
        set_window_spaces_override(survivor_wsid, None);
        clear_overrides(&f.exiled_wsids);
        sa::set_available(false);
    }

    #[test]
    fn takeover_without_the_scripting_addition_is_left_as_macos_made_it() {
        let mut f = fixture();
        sa::set_available(false);
        let mut screens = make_screen_snapshots(vec![screen1()], vec![Some(space2())]);
        screens[0].display_uuid = "test-display-0".to_string();
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                // The survivor inherits every desktop the departed display
                // had, listed first, and has already been handed the
                // taken-over one as its "last user space".
                state.display_space_ids.insert("test-display-0".to_string(), vec![
                    space2(),
                    space2_extra(),
                    space1(),
                ]);
                state.last_user_space_by_display.insert("test-display-0".to_string(), space2());
            },
        ));
        assert!(f.reactor.display_archive.has(DISPLAY2));
        assert!(sa::window_moves().is_empty());
        for wid in &f.exiled {
            assert!(!f.reactor.layout_manager.layout_engine.is_window_floating(*wid));
            assert_eq!(f.reactor.state.windows.user_floating(*wid), None);
        }
        assert_eq!(test_layout(&mut f.reactor, space2(), screen1()).len(), 3);
    }

    #[test]
    fn disabled_setting_keeps_the_old_behaviour() {
        let mut f = fixture();
        f.reactor.config.settings.restore_display_layouts = false;

        unplug(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        for wid in &f.exiled {
            assert!(has_window_in_layout(&mut f.reactor, space1(), screen1(), *wid));
        }

        replug(&mut f);
        windows_land(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        for wid in &f.exiled {
            assert_eq!(
                f.reactor.assigned_space_for_window_id(*wid),
                Some(space2_returned())
            );
        }
        clear_overrides(&f.exiled_wsids);
    }

    // ---- the record is about the displays that were there when it was
    // taken, from the last whole report of them, and settles the survivor
    // as often as a departure costs it a desktop.

    const DISPLAY3: &str = "test-display-2";
    /// What the window server files the desktops under while a lid is
    /// closing: a managed display that is no screen.
    const PSEUDO: &str = "78D9F424-0000-0000-0000-000000000000";

    fn space3() -> SpaceId { SpaceId::new(21) }

    fn screen3() -> CGRect { CGRect::new(CGPoint::new(-1920., 0.), CGSize::new(1920., 1080.)) }

    /// Every window where the survivor's is on `survivor` and the exiled
    /// ones are on `exiled`.
    fn everyone(f: &Fixture, survivor: SpaceId, exiled: SpaceId) -> Vec<(WindowServerId, SpaceId)> {
        std::iter::once((f.reactor.test_window_server_id(f.survivor), survivor))
            .chain(f.exiled_wsids.iter().map(|wsid| (*wsid, exiled)))
            .collect()
    }

    /// The main display departs and takes the survivor's desktop with it;
    /// rift makes the survivor `made` and its window lands there.
    fn takeover_and_settle(f: &mut Fixture, made: SpaceId) {
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        sa::set_next_created_space(Some(made.get()));
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged = everyone(f, space2(), space2());
        takeover(
            f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra(), made],
            merged,
        );
        assert_eq!(sa::window_moves(), vec![(survivor_wsid.as_u32(), made.get())]);
        set_window_spaces(&[survivor_wsid], made);
        let on_made = everyone(f, made, space2());
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(made)],
            vec![(survivor_wsid, space2(), made)],
            on_made,
        ));
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(made));
    }

    /// A third display comes, with a desktop of its own. `survivor` is what
    /// the survivor lists and shows first.
    fn third_display_arrives(f: &mut Fixture, survivor: Vec<SpaceId>) {
        let shown = survivor[0];
        managed(vec![
            ("test-display-0", survivor.clone()),
            (DISPLAY3, vec![space3()]),
        ]);
        let mut screens =
            make_screen_snapshots(vec![screen1(), screen3()], vec![Some(shown), Some(space3())]);
        screens[1].display_uuid = DISPLAY3.to_string();
        let windows = everyone(f, shown, space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1(), screen3()],
            vec![Some(shown), Some(space3())],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state.display_space_ids.insert("test-display-0".to_string(), survivor);
                state.display_space_ids.insert(DISPLAY3.to_string(), vec![space3()]);
                for (wsid, space) in windows {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
    }

    /// The third display goes; its desktop stays with the survivor.
    /// `survivor` is what the survivor lists after, `windows` where the
    /// window server has every window.
    fn third_display_departs(
        f: &mut Fixture,
        survivor: Vec<SpaceId>,
        live: Vec<SpaceId>,
        windows: Vec<(WindowServerId, SpaceId)>,
    ) {
        managed(vec![("test-display-0", live)]);
        for (wsid, space) in &windows {
            set_window_spaces(&[*wsid], *space);
        }
        let shown = survivor[0];
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(shown)],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state.display_space_ids.insert("test-display-0".to_string(), survivor);
                for (wsid, space) in windows {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
    }

    #[test]
    fn a_display_that_comes_and_goes_while_another_is_away_is_not_waited_for() {
        let mut f = spaces_fixture();
        unplug(&mut f);
        assert!(f.reactor.display_archive.record().is_some());

        third_display_arrives(&mut f, vec![space1()]);
        let windows = everyone(&f, space1(), space1());
        third_display_departs(
            &mut f,
            vec![space1(), space3()],
            vec![space1(), space3()],
            windows,
        );
        let record = f.reactor.display_archive.record().expect("the record stands");
        assert_eq!(
            record.display_uuids(),
            vec!["test-display-0", DISPLAY2],
            "the visitor is no part of the record"
        );
        assert!(sa::window_moves().is_empty());

        // The second display is back with a fresh desktop: reconciled at
        // once, the visitor never having been waited for.
        managed(vec![
            ("test-display-0", vec![space1(), space3()]),
            (DISPLAY2, vec![space2_returned()]),
        ]);
        replug(&mut f);
        let expected: Vec<(u32, u64)> = f
            .exiled_wsids
            .iter()
            .map(|wsid| (wsid.as_u32(), space2_returned().get()))
            .collect();
        assert_eq!(
            sorted_moves(),
            expected,
            "every exiled window is sent to the fresh desktop"
        );
        windows_land(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            test_layout(&mut f.reactor, space2_returned(), screen2()),
            f.layout_before
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn the_survivor_is_settled_again_when_a_later_departure_destroys_its_made_desktop() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        let made = SpaceId::new(23);
        takeover_and_settle(&mut f, made);

        third_display_arrives(&mut f, vec![made, space2(), space2_extra()]);
        assert_eq!(sa::space_creations().len(), 1, "nothing to settle on an arrival");

        // It goes: macOS destroys the made desktop, first in the survivor's
        // list, merges its window into the third display's desktop and
        // hands that desktop to the survivor.
        let again = SpaceId::new(27);
        sa::set_next_created_space(Some(again.get()));
        let windows = everyone(&f, space3(), space2());
        third_display_departs(
            &mut f,
            vec![space3(), space2(), space2_extra()],
            vec![space3(), space2(), space2_extra(), again],
            windows,
        );
        assert_eq!(
            sa::space_creations(),
            vec![space2_extra().get(), space2_extra().get()],
            "the survivor gets another desktop, after the last visitor"
        );
        assert_eq!(
            sa::window_moves().last(),
            Some(&(survivor_wsid.as_u32(), again.get())),
            "its window is sent there"
        );
        let record = f.reactor.display_archive.record().expect("the record stands");
        assert_eq!(record.made_desktops(), vec![again]);

        set_window_spaces(&[survivor_wsid], again);
        let on_again = everyone(&f, again, space2());
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(again)],
            vec![(survivor_wsid, space3(), again)],
            on_again,
        ));
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(again));
        assert_eq!(
            test_layout(&mut f.reactor, again, screen1()),
            survivor_layout,
            "the survivor's layout follows its window"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn the_record_is_taken_from_the_last_whole_display_set() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());

        // The lid closes. The window server merges the survivor's window
        // into the main display's desktop, and reports the displays half
        // way: the built-in showing nothing, every desktop filed under a
        // managed display that is no screen at all.
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let mut screens =
            make_screen_snapshots(vec![screen2(), screen1()], vec![Some(space2()), None]);
        screens[0].display_uuid = DISPLAY2.to_string();
        screens[1].display_uuid = "test-display-0".to_string();
        let merged = everyone(&f, space2(), space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen2(), screen1()],
            vec![Some(space2()), None],
            move |state| {
                state.screens = screens;
                state.has_seen_display_set = true;
                state.display_space_ids.clear();
                state
                    .display_space_ids
                    .insert(PSEUDO.to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in merged {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert!(
            f.reactor.display_archive.record().is_none(),
            "no display has departed yet"
        );

        // Minutes pass asleep; the main display is unplugged meanwhile.
        f.reactor
            .display_archive
            .backdate_pre_churn(std::time::Duration::from_secs(600));

        // Awake, the built-in is alone with the main display's desktops;
        // its own is gone.
        let made = SpaceId::new(23);
        sa::set_next_created_space(Some(made.get()));
        managed(vec![("test-display-0", vec![space2(), space2_extra(), made])]);
        let merged = everyone(&f, space2(), space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.has_seen_display_set = true;
                state.display_set_changed = true;
                state.topology_changed = true;
                state.should_force_refresh_layout = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in merged {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        let record = f.reactor.display_archive.record().expect("the departure is seen at wake");
        assert_eq!(
            record.display_uuids(),
            vec!["test-display-0", DISPLAY2],
            "recorded from the displays as last reported whole"
        );
        assert_eq!(
            record.recorded_desktop(f.survivor),
            Some(space1()),
            "recorded from the trees as they were before the lid closed"
        );
        assert_eq!(sa::space_creations(), vec![space2_extra().get()]);
        assert_eq!(sa::window_moves(), vec![(survivor_wsid.as_u32(), made.get())]);
        assert_eq!(sa::space_moves(), vec![
            (space2().get(), made.get()),
            (space2_extra().get(), space2().get())
        ]);
        assert_eq!(sa::space_focuses(), vec![made.get()]);

        set_window_spaces(&[survivor_wsid], made);
        let on_made = everyone(&f, made, space2());
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(made)],
            vec![(survivor_wsid, space2(), made)],
            on_made,
        ));
        assert_eq!(
            test_layout(&mut f.reactor, made, screen1()),
            survivor_layout,
            "the survivor's layout is on its made desktop"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn a_made_desktop_that_nothing_replaces_stays_with_its_windows() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        let made = SpaceId::new(23);
        takeover_and_settle(&mut f, made);

        // The main display is back, and macOS mints nothing for the
        // survivor: the made desktop is all it has.
        let moves_before = sa::window_moves().len();
        let windows = everyone(&f, made, space2());
        replug_after_takeover(
            &mut f,
            vec![made],
            vec![space2(), space2_extra()],
            (made, space2()),
            windows,
        );
        assert!(f.reactor.display_archive.is_empty(), "the record is done");
        assert_eq!(sa::window_moves().len(), moves_before, "nothing moves");
        assert!(
            f.reactor.display_archive.retiring.is_empty(),
            "the made desktop is not up for destruction"
        );
        assert!(sa::space_destroys().is_empty());
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(made));
        assert_eq!(
            test_layout(&mut f.reactor, made, screen1()),
            survivor_layout,
            "the survivor's layout stays on it"
        );
        assert_eq!(
            test_layout(&mut f.reactor, space2(), screen2()),
            f.layout_before,
            "the main display's layout is intact"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn a_layout_mode_switched_while_the_display_was_away_is_kept() {
        let mut f = spaces_fixture();
        unplug(&mut f);
        assert_ne!(
            f.reactor.layout_manager.layout_engine.layout_mode_at(space1()),
            "stack"
        );

        // While the display is away, the user stacks the survivor's desktop.
        let response = f.reactor.layout_manager.layout_engine.handle_virtual_workspace_command(
            &mut f.reactor.state.windows,
            space1(),
            &LayoutCommand::SetWorkspaceLayout {
                workspace: None,
                mode: LayoutMode::Stack,
            },
        );
        assert!(response.changed);
        assert_eq!(
            f.reactor.layout_manager.layout_engine.layout_mode_at(space1()),
            "stack"
        );

        managed(vec![
            ("test-display-0", vec![space1()]),
            (DISPLAY2, vec![space2_returned()]),
        ]);
        replug(&mut f);
        windows_land(&mut f);
        assert!(f.reactor.display_archive.is_empty());
        assert_eq!(
            f.reactor.layout_manager.layout_engine.layout_mode_at(space1()),
            "stack",
            "the return does not put the departure-time mode back"
        );
        assert_eq!(
            test_layout(&mut f.reactor, space2_returned(), screen2()),
            f.layout_before,
            "the departed display's own layout is still restored"
        );
        spaces_cleanup(&f, &[]);
    }

    #[test]
    fn a_settle_that_sleep_cut_short_is_done_again_at_wake() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let survivor_layout = test_layout(&mut f.reactor, space1(), screen1());
        let made = SpaceId::new(23);
        sa::set_next_created_space(Some(made.get()));
        set_window_spaces(&[survivor_wsid], space2());
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let merged = everyone(&f, space2(), space2());
        takeover(
            &mut f,
            vec![space2(), space2_extra()],
            vec![space2(), space2_extra(), made],
            merged,
        );
        assert_eq!(sa::window_moves(), vec![(survivor_wsid.as_u32(), made.get())]);

        // The lid closes before Dock moves the window. The deadline passes
        // with it still on the visitors' desktop; then the Mac wakes.
        f.reactor.handle_event(Event::DisplayHomingDeadline(
            super::display_record::RECORD_DEADLINE_KEY.to_string(),
        ));
        assert!(f.reactor.display_archive.record().is_some());
        f.reactor.handle_event(Event::SystemWoke);

        // The first whole report after the wake changes nothing about the
        // displays; the window is still where the window server merged it.
        managed(vec![("test-display-0", vec![made, space2(), space2_extra()])]);
        let on_visitors = everyone(&f, space2(), space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(made)],
            move |state| {
                state.has_seen_display_set = true;
                state.display_space_ids.insert("test-display-0".to_string(), vec![
                    made,
                    space2(),
                    space2_extra(),
                ]);
                for (wsid, space) in on_visitors {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert_eq!(
            sa::window_moves(),
            vec![
                (survivor_wsid.as_u32(), made.get()),
                (survivor_wsid.as_u32(), made.get())
            ],
            "the window is sent to the made desktop again"
        );
        assert_eq!(
            sa::space_creations().len(),
            1,
            "the made desktop is not made again"
        );

        set_window_spaces(&[survivor_wsid], made);
        let on_made = everyone(&f, made, space2());
        f.reactor.handle_event(topology_event(
            vec![screen1()],
            vec![Some(made)],
            vec![(survivor_wsid, space2(), made)],
            on_made,
        ));
        assert_eq!(f.reactor.assigned_space_for_window_id(f.survivor), Some(made));
        assert_eq!(
            test_layout(&mut f.reactor, made, screen1()),
            survivor_layout,
            "the survivor's layout is on its made desktop"
        );
        spaces_cleanup(&f, &[]);
    }

    /// The user destroys the desktop rift made — `destroy_space`, Mission
    /// Control — and the window server drops its window on a visitor
    /// desktop. The record forgets both, so the wake that re-checks the
    /// settle has nothing to do: no desktop made again, no window sent
    /// anywhere, no desktop reordered. (Seen 2026-09-06: every wake made a
    /// desktop, pulled Discord onto it and walked the desktops around.)
    #[test]
    fn a_made_desktop_the_user_destroys_is_forgotten_and_not_made_again_at_wake() {
        let mut f = spaces_fixture();
        let survivor_wsid = f.reactor.test_window_server_id(f.survivor);
        let made = SpaceId::new(23);
        takeover_and_settle(&mut f, made);
        assert_eq!(sa::space_creations().len(), 1);
        let space_moves = sa::space_moves().len();

        // Same displays, no reshuffle: the made desktop is simply gone from
        // the list, and its window is where the window server dropped it.
        f.reactor
            .display_archive
            .record_mut()
            .unwrap()
            .backdate(std::time::Duration::from_secs(30));
        set_window_spaces(&[survivor_wsid], space2());
        managed(vec![("test-display-0", vec![space2(), space2_extra()])]);
        let dropped = everyone(&f, space2(), space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.has_seen_display_set = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in dropped {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        let record = f.reactor.display_archive.record().expect("the record stands");
        assert!(
            record.made_desktops().is_empty(),
            "the made desktop is forgotten"
        );
        f.reactor.handle_event(Event::WindowServerAppeared(
            survivor_wsid,
            space2(),
            SpaceEventKind::User,
        ));
        let record = f.reactor.display_archive.record().expect("the record stands");
        assert_eq!(
            record.recorded_desktop(f.survivor),
            Some(space2()),
            "the window is filed where it turned up"
        );

        // The Mac sleeps and wakes; the first whole report changes nothing.
        f.reactor.handle_event(Event::SystemWoke);
        let dropped = everyone(&f, space2(), space2());
        f.reactor.handle_event(space_state_event_with(
            vec![screen1()],
            vec![Some(space2())],
            move |state| {
                state.has_seen_display_set = true;
                state
                    .display_space_ids
                    .insert("test-display-0".to_string(), vec![space2(), space2_extra()]);
                for (wsid, space) in dropped {
                    state.active_window_spaces.insert(wsid, space);
                }
            },
        ));
        assert_eq!(
            sa::space_creations().len(),
            1,
            "no desktop is made again at wake"
        );
        assert_eq!(
            sa::window_moves().len(),
            1,
            "the window stays where the user left it"
        );
        assert_eq!(
            sa::space_moves().len(),
            space_moves,
            "no desktop is reordered at wake"
        );
        assert!(
            f.reactor.display_archive.record().is_some(),
            "the record still waits for the display"
        );
        spaces_cleanup(&f, &[]);
    }
}

/// A window on a desktop that is not being shown cannot change desktops by
/// moving. After a display change its frame can land on the other display,
/// and a frame report read as a cross-space move pulled it into that
/// display's visible tree beside the windows really there, until discovery
/// put it back: a ghost tile for a moment.
#[test]
fn a_frame_report_for_a_window_on_a_hidden_desktop_does_not_move_it_into_the_visible_tree() {
    let mut reactor = test_reactor();
    let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
    let screen2 = CGRect::new(CGPoint::new(1440., 0.), CGSize::new(1440., 900.));
    let space1 = SpaceId::new(1);
    let space2 = SpaceId::new(2);
    let hidden = SpaceId::new(5);
    reactor.handle_event(space_state_event_with(
        vec![screen1, screen2],
        vec![Some(space1), Some(space2)],
        move |state| {
            state.has_seen_display_set = true;
            state
                .display_space_ids
                .insert("test-display-0".to_string(), vec![space1, hidden]);
            state.display_space_ids.insert("test-display-1".to_string(), vec![space2]);
        },
    ));
    reactor.add_test_app(1);
    let wid = WindowId::new(1, 1);
    let wsid = WindowServerId::new(121);
    // Last seen with a frame on the second display, now on the first
    // display's hidden desktop.
    let old_frame = CGRect::new(CGPoint::new(1500., 100.), CGSize::new(800., 600.));
    reactor.add_test_window(wid, wsid, Some(hidden), old_frame);
    let workspace = reactor.test_workspace(hidden, 0);
    assert!(reactor.assign_test_window_to_workspace(hidden, wid, workspace));
    crate::sys::window_server::set_window_spaces_override(wsid, Some(vec![hidden.get()]));

    let new_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(800., 600.));
    reactor.handle_event(Event::WindowFrameChanged(
        wid,
        new_frame,
        None,
        Requested(false),
        Some(MouseState::Up),
    ));

    assert_eq!(
        reactor.assigned_space_for_window_id(wid),
        Some(hidden),
        "the window stays on the hidden desktop the window server has it on"
    );
    assert!(
        !reactor.layout_manager.layout_engine.is_window_tiled(space1, wid),
        "it is not tiled into the visible desktop's tree"
    );
    crate::sys::window_server::set_window_spaces_override(wsid, None);
}

/// Same drag, but with the session's origin space known — the normal case.
/// The float must arrive on the other display still floating, not tiled.
#[test]
fn cross_display_drag_of_a_float_keeps_it_floating() {
    let (mut reactor, wid, _wsid, space1, space2, initial_frame, screen2) =
        reactor_with_window_on_space1_two_displays();
    let source_workspace = reactor
        .layout_manager
        .layout_engine
        .active_workspace(space1)
        .expect("source workspace");

    reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
    reactor.send_layout_event(LayoutEvent::WindowFocused(space1, wid));
    reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
    assert!(reactor.layout_manager.layout_engine.is_window_floating(wid));
    reactor.layout_manager.layout_engine.store_floating_position(
        space1,
        source_workspace,
        wid,
        initial_frame,
    );

    let moved_frame = CGRect::new(
        CGPoint::new(screen2.origin.x + 120.0, initial_frame.origin.y),
        initial_frame.size,
    );
    reactor.drag_manager.drag_state = DragState::Active {
        session: DragSession {
            window: wid,
            last_frame: moved_frame,
            origin_space: Some(space1),
            settled_space: Some(space2),
            layout_dirty: true,
        },
    };

    // The full path: the reactor resolves the drop space from where the
    // pointer lets go and applies the resulting layout events.
    crate::sys::window_server::set_cursor_location_override(Some(CGPoint::new(
        screen2.origin.x + 200.0,
        screen2.origin.y + 100.0,
    )));
    reactor.handle_event(Event::MouseUp);
    crate::sys::window_server::set_cursor_location_override(None);
    assert!(matches!(reactor.drag_manager.drag_state, DragState::Inactive));

    assert_eq!(reactor.assigned_space_for_window_id(wid), Some(space2));
    assert!(
        reactor.layout_manager.layout_engine.is_window_floating(wid),
        "a dragged float must still be floating after crossing displays"
    );
    assert!(!reactor.layout_manager.layout_engine.is_window_tiled(space2, wid));
    assert!(!reactor.layout_manager.layout_engine.is_window_tiled(space1, wid));
}

mod unmanaged_focus {
    use test_log::test;

    use super::*;

    fn reactor_with_tiled_window() -> (Reactor, CGRect, SpaceId, WindowId) {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let main = WindowId::new(1, 1);
        reactor.add_test_window(
            main,
            WindowServerId::new(101),
            Some(space),
            CGRect::new(CGPoint::new(100., 100.), CGSize::new(1200., 700.)),
        );
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, main, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, main));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::WindowServerFocusChanged(main, space));
        assert!(has_window_in_layout(&mut reactor, space, screen, main));
        assert_eq!(reactor.layout_manager.layout_engine.focused_window(), Some(main));
        (reactor, screen, space, main)
    }

    /// Premiere in front: its main window is tracked but not admitted. The
    /// toggle must not reach back to the last managed window.
    #[test]
    fn toggle_floating_with_an_unmanaged_window_in_front_does_nothing() {
        let (mut reactor, screen, space, main) = reactor_with_tiled_window();
        reactor.add_test_app(2);
        let unmanaged = WindowId::new(2, 1);
        reactor.add_test_window_with_manageability(
            unmanaged,
            WindowServerId::new(201),
            Some(space),
            CGRect::new(CGPoint::new(200., 150.), CGSize::new(1000., 600.)),
            false,
        );
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::WindowServerFocusChanged(unmanaged, space));

        reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));

        assert!(!reactor.layout_manager.layout_engine.is_window_floating(main));
        assert!(has_window_in_layout(&mut reactor, space, screen, main));
        assert!(!reactor.layout_manager.layout_engine.is_window_floating(unmanaged));
        assert!(!has_window_in_layout(&mut reactor, space, screen, unmanaged));
    }

    /// An app in front whose window rift never resolved at all (no AX main
    /// window, nothing under the window server focus) is treated the same.
    #[test]
    fn toggle_floating_with_a_windowless_foreign_app_in_front_does_nothing() {
        let (mut reactor, screen, space, main) = reactor_with_tiled_window();
        reactor.add_test_app(2);
        reactor.handle_event(Event::WindowDestroyed(main));
        // `main` is still in the layout for this test's purpose: re-add it so
        // only the focus bookkeeping is reset, not the window itself.
        reactor.add_test_window(
            main,
            WindowServerId::new(101),
            Some(space),
            CGRect::new(CGPoint::new(100., 100.), CGSize::new(1200., 700.)),
        );
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, main, workspace));
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, main));
        reactor.send_layout_event(LayoutEvent::WindowFocused(space, main));
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        assert_eq!(reactor.main_window(), None);

        reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));

        assert!(!reactor.layout_manager.layout_engine.is_window_floating(main));
        assert!(has_window_in_layout(&mut reactor, space, screen, main));
    }

    /// Premiere from the Dock: the tracker learns its window through AX
    /// (`AXFocusedWindow`) but the engine never gets a `WindowFocused` for it,
    /// so its focus record still points at the last managed window. The
    /// toggle must act on Premiere and leave that window alone.
    #[test]
    fn toggle_floating_acts_on_the_admitted_window_in_front_not_the_engines_stale_focus() {
        let (mut reactor, screen, space, main) = reactor_with_tiled_window();
        reactor.add_test_app(2);
        reactor.main_window_tracker.register_app_for_test(2);
        let premiere = WindowId::new(2, 1);
        reactor.add_test_window(
            premiere,
            WindowServerId::new(201),
            Some(space),
            CGRect::new(CGPoint::new(200., 150.), CGSize::new(1000., 600.)),
        );
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, premiere, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(premiere);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, premiere));
        // Engine focus is still on `main` from the helper.
        assert_eq!(reactor.layout_manager.layout_engine.focused_window(), Some(main));

        // Dock activation: global activation plus the AX main-window
        // resolution, no window-server focus event.
        reactor.handle_event(Event::ApplicationGloballyActivated(2));
        reactor.handle_event(Event::ApplicationMainWindowChanged(2, Some(premiere), Quiet::No));
        assert_eq!(reactor.main_window(), Some(premiere));
        assert_eq!(
            reactor.layout_manager.layout_engine.focused_window(),
            Some(main),
            "precondition: the engine's focus record is stale"
        );

        reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));

        assert!(!reactor.layout_manager.layout_engine.is_window_floating(premiere));
        assert!(has_window_in_layout(&mut reactor, space, screen, premiere));
        assert!(!reactor.layout_manager.layout_engine.is_window_floating(main));
        assert!(has_window_in_layout(&mut reactor, space, screen, main));
    }

    /// The managed window in front still toggles.
    #[test]
    fn toggle_floating_with_the_managed_window_in_front_still_works() {
        let (mut reactor, screen, space, main) = reactor_with_tiled_window();
        reactor.handle_event(Event::Command(Command::Layout(
            LayoutCommand::ToggleWindowFloating,
        )));
        assert!(reactor.layout_manager.layout_engine.is_window_floating(main));
        assert!(!has_window_in_layout(&mut reactor, space, screen, main));
    }
}

mod float_frames_are_never_invented {
    use test_log::test;

    use super::*;

    fn reactor_with_float(frame: CGRect) -> (Reactor, WindowId, WindowServerId, SpaceId, CGRect) {
        let mut reactor = test_reactor();
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        let space = SpaceId::new(1);
        reactor.handle_event(space_state_event(vec![screen], vec![Some(space)]));
        reactor.add_test_app(1);
        let wid = WindowId::new(1, 1);
        let wsid = WindowServerId::new(101);
        reactor.add_test_window(wid, wsid, Some(space), frame);
        let workspace = reactor.test_workspace(space, 0);
        assert!(reactor.assign_test_window_to_workspace(space, wid, workspace));
        reactor.layout_manager.layout_engine.mark_window_floating(wid);
        reactor.send_layout_event(LayoutEvent::WindowAdded(space, wid));
        (reactor, wid, wsid, space, screen)
    }

    fn laid_out(
        reactor: &mut Reactor,
        space: SpaceId,
        screen: CGRect,
        wid: WindowId,
    ) -> Option<CGRect> {
        let gaps = reactor.config.settings.layout.gaps.clone();
        reactor
            .layout_manager
            .layout_engine
            .calculate_layout_with_virtual_workspaces(
                &reactor.state.windows,
                space,
                screen,
                &gaps,
                0.0,
                Default::default(),
                Default::default(),
                |w| reactor.state.windows.window(w).map(|s| s.frame_monotonic),
                &[screen],
            )
            .into_iter()
            .find(|(w, _)| *w == wid)
            .map(|(_, f)| f)
    }

    /// Preview's Open panel: a float whose only known frame is empty (the
    /// window server has not laid it out yet) gets no frame from rift at
    /// all — it used to be centred at 0x0.
    #[test]
    fn a_float_with_no_usable_frame_is_left_alone() {
        let empty = CGRect::new(CGPoint::new(0., 0.), CGSize::new(0., 0.));
        let (mut reactor, wid, _wsid, space, screen) = reactor_with_float(empty);
        assert_eq!(laid_out(&mut reactor, space, screen, wid), None);
    }

    /// An empty frame from the window server is the absence of a frame.
    #[test]
    fn an_empty_window_server_frame_is_not_adopted() {
        let at = CGRect::new(CGPoint::new(100., 100.), CGSize::new(300., 200.));
        let (mut reactor, wid, wsid, _space, _screen) = reactor_with_float(at);
        crate::sys::window_server::set_live_frame_override(
            wsid,
            Some(CGRect::new(CGPoint::new(0., 0.), CGSize::new(0., 0.))),
        );
        reactor.refresh_floating_frames_from_window_server();
        assert!(reactor.state.windows.window(wid).unwrap().frame_monotonic.same_as(at));

        let moved = CGRect::new(CGPoint::new(400., 300.), CGSize::new(300., 200.));
        crate::sys::window_server::set_live_frame_override(wsid, Some(moved));
        reactor.refresh_floating_frames_from_window_server();
        assert!(reactor.state.windows.window(wid).unwrap().frame_monotonic.same_as(moved));
        crate::sys::window_server::set_live_frame_override(wsid, None);
    }

    /// The user tiled it; leaving a workspace does not hand it back to a
    /// floating rule.
    #[test]
    fn a_users_tile_choice_survives_removal_from_a_workspace() {
        let at = CGRect::new(CGPoint::new(100., 100.), CGSize::new(300., 200.));
        let (mut reactor, wid, _wsid, space, _screen) = reactor_with_float(at);
        reactor.send_layout_event(LayoutEvent::WindowFocused(space, wid));
        reactor.handle_test_layout_command(LayoutCommand::ToggleWindowFloating);
        assert!(!reactor.layout_manager.layout_engine.is_window_floating(wid));
        assert_eq!(reactor.state.windows.user_floating(wid), Some(false));

        reactor.send_layout_event(LayoutEvent::WindowRemoved(wid));
        assert_eq!(
            reactor.state.windows.user_floating(wid),
            Some(false),
            "the user's choice is about the window, not the workspace"
        );
    }
}

mod removal_scrubs_every_tree {
    use test_log::test;

    use super::*;

    /// A drop on another display reassigns the window before the removal
    /// runs. The removal must still take it out of the tree it is actually
    /// in, or it ends up tiled on both displays and bounces between them.
    #[test]
    fn a_window_reassigned_before_removal_leaves_its_old_tree() {
        let (mut reactor, wid, _wsid, space1, space2, _frame, screen2) =
            reactor_with_window_on_space1_two_displays();
        reactor.send_layout_event(LayoutEvent::WindowAdded(space1, wid));
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1440., 900.));
        assert!(has_window_in_layout(&mut reactor, space1, screen1, wid));

        // The store now says space2, as after a drop there; the tree has not
        // been told yet.
        let target = reactor.layout_manager.layout_engine.active_workspace(space2).unwrap();
        assert!(
            reactor
                .layout_manager
                .layout_engine
                .virtual_workspace_manager_mut()
                .assign_window_to_workspace(&mut reactor.state.windows, space2, wid, target)
        );
        reactor.send_layout_event(LayoutEvent::WindowRemoved(wid));
        assert!(!has_window_in_layout(&mut reactor, space1, screen1, wid));

        reactor.send_layout_event(LayoutEvent::WindowAdded(space2, wid));
        assert!(has_window_in_layout(&mut reactor, space2, screen2, wid));
        assert!(!has_window_in_layout(&mut reactor, space1, screen1, wid));
    }
}

/// A CLI that answers "Command executed successfully" to a command that did
/// nothing sends you looking anywhere but at the scripting addition, which is
/// the one thing that would have fixed it. The three commands that need the
/// addition report why they could not run instead.
#[test]
fn space_commands_report_that_they_need_the_scripting_addition() {
    use crate::model::reactor::Command;
    use crate::sys::scripting_addition::test_hooks as sa;

    let mut reactor = test_reactor();
    sa::set_available(false);

    let error = reactor
        .handle_ipc_command(Command::Reactor(ReactorCommand::DestroySpace))
        .expect_err("destroying a space without the addition cannot succeed");
    assert!(
        error.contains("scripting addition") && error.contains("rift sa load"),
        "the message has to name the addition and the command that loads it: {error:?}"
    );
}

/// The other silent path: a command whose target does not exist at all. It
/// used to return the same success as one that worked.
#[test]
fn moving_a_window_to_a_space_that_does_not_exist_reports_it() {
    use crate::model::reactor::Command;

    let mut reactor = test_reactor();
    let error = reactor
        .handle_ipc_command(Command::Reactor(ReactorCommand::MoveWindowToSpace {
            index: 9999,
            follow: false,
        }))
        .expect_err("there is no macOS space 9999");
    assert!(
        error.contains("9999"),
        "the message names the space asked for: {error:?}"
    );
}
