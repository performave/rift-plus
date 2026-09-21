# Handoff: cross-display drags, floats, and the trace replay harness

Branch: `feat/display-layout-restore`. Everything below is committed in `3646b88` on top of
`15a4651` (one commit; the suggested split was not done). Rounds 2–6 are the 2026-08-30 afternoon
passes, each driven by a recording.
Experiments that were abandoned live on `wip/drag-freeze-experiments` (reference only).

## What was wrong, in one sentence

Rift keeps several independent records of the same fact (which tree a window is in,
which workspace it is assigned to, whether it floats, where it was last seen, which window
is focused, which display it is on) and reconciles them with heuristics; every bug in this
round was two of those records disagreeing, and rift acting on the stale one.

## Fixes on this branch (each has a test; most have a recorded fixture)

| Symptom | Root cause | Fix | Where |
|---|---|---|---|
| sa+T from the Dock tiled the *previous* window | Premiere has no `AXMainWindow`; engine's private `focused_window` stayed on the last managed window | `AXFocusedWindow` fallback; reactor aligns engine focus to the resolved front window before focused-window commands; unmanaged front window ⇒ command is a no-op | `sys/axuielement.rs`, `actor/app.rs::read_main_window`, `reactor.rs::sync_layout_focus_for_command` / `focused_window_is_unmanaged` |
| Preview Open panel snapped/resized | window server reports a new panel as 0×0; float layout centred it | empty frames never adopted; float layout never invents a frame (no centring fallback) | `reactor.rs::refresh_floating_frames_from_window_server`, `engine.rs` float layout |
| Cross-display drop of a tile → flicker between displays | drop reassigned the window *before* the deferred `WindowRemoved`, which removed from the wrong tree ⇒ tiled on both spaces | removal scrubs the window from every tree | `engine.rs::remove_window_layout_membership` |
| sa+T on a float left it tiled + floating (floats "snap back") | toggle removed from the *pointer's* display's tree, not the window's | toggle acts on the window's own space/tree, removes from all trees | `engine.rs` `ToggleWindowFloating` |
| User's tile choice lost on cross-space move | `clear_rule_metadata` wiped `user_floating` | only rule metadata is cleared | `model/window_store.rs` |
| Drag not seen at all ⇒ no drop resolution, stale frame stored at release | transaction gate discarded button-down frame reports trailing rift's last write | button-down reports bypass the gate; window is *held* from the first such report until `MouseUp` | `events/window.rs::classify_window_frame_change`, `reactor.rs` `WindowFrameChanged`/`MouseUp`, `managers.rs::DragManager::held_window` |
| Float jumped to a remembered frame at drop | `pending_float_placement` was per window; a stale stored frame on another workspace became "intended" | placement bound to (space, workspace); drop stores the live frame | `engine.rs::PendingPlacement`, `events/drag.rs` |
| Mid-drag, window pulled into a second tree | discovery `sync_tiled_windows_for_app` re-adds from the store's assignment | engine knows the frozen window; one-tree law enforced on every discovery add; reassign/topology/rule passes skip the held window | `engine.rs::sync_tiled_windows_for_app`, `reactor.rs::window_in_drag` guards |
| `sa+N` / `Alt+N` only addressed the active display's spaces | `space_at_index` was per display | global Mission Control numbering; cross-display switch via scripting addition, pointer-warp + swipe fallback | `sys/space_switch.rs`, `sys/screen.rs` |

Config: `~/dotfiles/rift/.config/rift/config.toml` (and the live copy) gained
`{ app_id = "com.adobe.PremierePro.26", ax_role = "AXLayoutArea", manage = true, floating = true }`
— Premiere's document window is `AXLayoutArea`, so the standard-window heuristic never admits it.

## Round 2: the flicker persisted, and the harness had said "clean"

The fixtures replayed with 0 violations while the cross-display flicker still happened on build
75040. Reading the `user_drag3` replay report by hand (state trajectory + live writes) showed two
mechanisms the invariants could not see:

1. **Mid-drag re-tile before the app reports the drag.** The window server hands a window to the
   display under the pointer as soon as the user grabs it; `SpaceStateChanged` arrives (with the
   window listed on the other space) ~30 ms *before* the app's first frame report with the button
   down, so `held_window` was not yet set and `reconcile_authoritative_active_window_snapshot` →
   `reassign_window_to_authoritative_space…` re-tiled the window under the cursor. The same
   snapshot's `WindowsDiscovered` then re-added it from the primary (window-server-id) loop of
   `emit_layout_events`, which skipped the frozen window only in the AX-fallback loop.
2. **Post-drop chase.** After a cross-display drop rift writes the new display's frame; Premiere
   takes 100–300 ms to apply it; meanwhile the server still reports the old space;
   `resolve_native_space` asked the server again (`live`), got the same stale answer, and believed
   it over the pending write — re-tiling on the old display, whose write landed just as the first
   one did, and so on: five alternating writes in 700 ms.

Fixes (uncommitted, on top of the table above):

| Where | Change |
|---|---|
| `reactor.rs::hold_if_dragged_across_spaces` (called from the reassign funnel) | a window that changes space while the button is down (`get_mouse_state()`) and is tiled on an active space is *held* (`held_window`) instead of reassigned; the drop resolves it |
| `reactor.rs::settle_held_window` (MouseUp) | a held window whose drag the app never reported is reassigned to where the server has it once the button is up |
| `events/window_discovery.rs::emit_layout_events` | the frozen window is skipped in the primary loop too |
| `model/tx_store.rs` + `resolve_native_space` | `TxRecord.sent_at`; a pending frame write younger than `DropPin::HOLD` (1.5 s) is believed over a contradicting space report — except when the report is rift's own homing move landing (`display_archive.homing_destination`) or rift moved the window with the scripting addition (`note_window_sent_to_space` clears the target) |

Harness changes (`replay.rs`) — verified by reverting the fixes: the old code now fails with 23
violations across the drag fixtures, the fixed code with 0 before divergence:

- Invariant 1 is judged at button release: writes since the first `MouseDragged` (or first
  button-down report) are collected, and at `MouseUp` any write to a window the user turned out to
  be moving (reported Down, or the reactor's session) is a violation.
- Invariant 4 no longer consults where the server has the window (the server's report lags rift's
  writes — following it *is* the bounce): two writes of one window to different displays within
  1.5 s with no button release and no command in between is a violation.
- `mouse_state` answers are synthesised from the event stream (`MouseDragged` ⇒ down, `MouseUp`/
  `MouseMoved` ⇒ up), because the recording only has answers for the moments live asked.
- **Divergence detection**: the replay is open-loop. `LiveWrites` matches every replay write to
  the nearest live write (±500 ms, same frame, or a no-op to where the window already was) and
  every `requested` frame report to the replay's last write; the first mismatch sets
  `report.diverged` and later violations go to `after_divergence` (printed, not failing). Every
  drag fixture now diverges within seconds of its first drop — exactly where the fix changed
  rift's writes — so **the fixtures prove the first drop of each recording and nothing after**.

A unit test covers the hold: `window_that_changes_space_with_the_button_down_is_held_until_the_drop`
(`sys::event::set_mouse_state_override` is the thread-local test hook).

**Validation that is still owed:** a live re-record on the new build. Drag a tile between displays a
few times (both directions, drop with the window's centre on either side of the seam), a float
across, and `sa+T` a couple of times; `rift execute trace start ~/Downloads/drag5.trace` …
`trace stop`; copy into `tests/traces/`; `cargo test --lib recorded_traces_replay_cleanly -- --nocapture`.
A recording made on the fixed build should not diverge at all, and its verdict then covers the
whole session.

## Round 3: `user_drag5` (recorded on the round-2 build)

The recording replays with 0 violations before divergence and confirms the flicker fix live. Three
more things came out of it, all fixed:

| Symptom | Root cause | Fix |
|---|---|---|
| Tile dragged 20 % onto the LG snapped back to the laptop | drop space came from the window's centre (`settled_space`) | drop space is the display under the pointer at `MouseUp` (`pointer_space`, before the frame-based fallbacks); a swap target still wins |
| Preview document window refused `sa+T` (admitted=false for the whole session) | the window-server snapshot cached at `WindowCreated` said `layer: 1` (the instant Preview opens a document behind its Open sheet); `refresh_heuristic` compared against the cache forever, while sticky/level were asked live | a would-be rejection re-queries `window_server::get_window` and refreshes the cache (`utils.rs::refresh_heuristic`); a cached layer 0 is not re-asked, so the normal path costs nothing extra. Not Preview-specific: any window created on a transient layer heals on its next refresh |
| Premiere squeezed to a 0-px slot (invariant 2, after divergence) | Mail reported its move halfway applied — new origin, old 2491×1399 size from the LG tile — and `HandledRefusedSize` took that as a minimum wider than the laptop display | a "refused" size larger than the window's display is not recorded as a minimum |

## Round 4: alt-drag resize left overlaps and gaps

A modifier (alt-drag) resize of a tile goes through `WindowResized` → arrange → rift writes the
window and its neighbours; the app echoes each write with the button down. The frame-report path
took "button down" as "the user is moving this window": `held_window` was set, and a held window is
skipped by every arrange — so the neighbours kept following the pointer while the resized window
stayed where the first write put it. `modifier_drag` was also never cleared (only replaced by the
next one), which kept `follow_focus_with_mouse` disabled after the first alt-drag.

Fix: reports for the window under a modifier drag are read as echoes (`effective_mouse_state = Up`
before the hold and the transaction gate), and `MouseUp` clears `modifier_drag`. Test:
`modifier_drag_echoes_do_not_hold_the_window`.

## Round 5: after an alt-drag, the next plain drag was not snapped back until a later click

`user_altdrag.trace`: after an alt-drag (even a no-op one on a lone tile) the user's next plain drag
ends with the window reported away from its slot and **no write**; the snap only comes with the
following drag's release. Cause: the event tap swallows the release that closes a modifier drag (so
the app never sees half a click) and never told the reactor, whose `modifier_drag` stayed armed —
so the next drag's reports were read as echoes of rift's own writes (round 4) and neither held nor
sessioned the window. Fix: the tap sends `Event::MouseUp` for a swallowed release too.
`MouseModifierDragBegin` is now serialisable, so alt-drags replay; this fixture predates that and
its alt-drags are events without a begin.

## Round 6: `user_altdrag2` (alt-drags now recorded), Preview after the file picker, pointer warps

| Symptom | Root cause | Fix |
|---|---|---|
| Alt-resize still left a gap until release | round 4 exempted only the *dragged* window's reports; the neighbour's echo (button down) still set `held_window` and the arrange skipped it | any frame report during a modifier drag is an echo (`modifier_drag.is_some()`) |
| Preview document opened from the file picker refuses `sa+T` | rejected at creation (`layer 1`); the round-3 re-query only runs inside `refresh_heuristic`, and nothing ever called it again for that window — `main_window()` was even `None` for it, so the command path bailed | `readmit_rejected_window`: a rejected window is judged again when it comes to the front and when a focused-window command targets it (all rejected windows of the frontmost app when rift can't name a front window); admitted, it enters the layout as a created window and the command is consumed |
| Clicking a window warped the pointer into it; clicking another display's menu bar warped to that display's app; dismissing a status-item popover warped back to Premiere | `follow_focus_with_mouse` ran on every focus change regardless of cause | `focus_change_is_pointer_driven`: no warp while the button is down or within 500 ms of `MouseUp`; and no warp when focus returns to the window it left for something rift has no window for (`focus_left_from`) |

Tests: `focus_change_right_after_a_click_does_not_warp`, `focus_returning_from_a_windowless_app_does_not_warp`.
The test helper's "unmanageable" windows are now non-standard, so the heuristic agrees with the flag
when a window is re-judged.

## Round 7: cross-display drags preview and perform the split

Dragging a tile onto another display used to tear down all drop tracking the moment the window's
space changed (`evaluate_drop_target` reset + hid on `origin_space != space`), so there was never
an overlay — and a drop always landed as a plain add at the tree's default position.

Now the display under the *pointer* supplies the drop candidates (matching round 3's rule that the
pointer decides the drop), while the dragged window's own membership stays frozen on its origin
tree (`membership_space`). Over a tiled window on the target display:

- the edge zones preview the split (`drop_action_for` allows `Insert` cross-space when the target
  layout can express it) and the drop performs it: the window is pulled out of its origin tree,
  assigned to the target space/workspace, and `insert_window_next_to` puts it beside the target —
  all synchronously in `handle_mouse_up`, before the queued removal/add events could clobber it
  (`moved_by_drop` skips the generic cross-space move block);
- the centre zone is *also* an insert cross-display (a cross-tree swap is not expressible): the
  whole target is four triangles (`DragManager::edge_direction`), so the overlay never blinks out
  over the middle and the drop lands on the side the pointer is nearest. Same-display behaviour is
  unchanged (centre still swaps there).

First live recording (`xdrag.trace`, in `tests/traces/`) replayed with 0 violations but showed the
first cut was unusable — overlay flashing, drag stutter, centre drops landing at the tree's
default slot. Three causes, fixed:

| Symptom | Cause | Fix |
|---|---|---|
| Stutter while dragging | `preview_insert_next_to` RON-round-trips the whole tree per pointer move; plus the pre-existing ~12 `window_spaces`/`space_is_user` queries per `MouseDragged` (11.5k Sys lines in a 40 s trace) now ran for the whole drag | preview cached per (dragged, target, action) on `DragManager::drop_preview_cache` — recomputed only when the triple changes, `Aim` sent only when the answer changes; `collect_drag_swap_candidates` no longer asks the window server where every window is (tree membership already says, one-tree law) — the per-move query storm from the open items is gone |
| Overlay flashing | cross-display centre zone returned `None` → hide/show cycling as the pointer crossed zones | centre maps to an edge insert (above) |
| Drop not where pointed | centre drop was a plain move to the tree's default slot | same |

Second recording (`xdrag2.trace`) still stuttered and flashed. Probing the replay showed the
sessions and drops were all correct — the report's per-drop "session None" is printed after
teardown and lies. The remaining causes were rift's own query load and preview churn, felt
directly in the drag because the event tap is a `HeadInsertEventTap`: every `MouseDragged`
passes through rift's callback before the app sees it, so WindowServer contention stutters the
user's drag itself.

| Cause | Fix |
|---|---|
| `drop_action_for` resolved the *target's* space via `best_space_for_window_id` → a live `window_spaces` query per pointer move for the whole drag | membership (`assigned_space_for_window_id`) first; the server is only asked when there is no assignment |
| `release_drop_pin_if_landed` ran after every event and live-queried `window_space(pin.window)` — a query per pointer move for 1.5 s after every drop | store agreement releases the pin with no query; live probes throttled to one per 200 ms (`DropPin::PROBE_EVERY`) |
| Pointer samples straddling a zone boundary flapped the action → preview recomputed (RON round-trip) and overlay re-aimed at report rate — the flashing and the lag correlating with the preview appearing | dwell hysteresis: a zone change is believed after the pointer stays in the new zone 150 ms (`ZoneCandidate::DWELL`, `sticky_drop_action`) |
| Crossing the gap between tiles dropped the target for a few reports → overlay blinked | 24 px grace band around the pending target's frame |
| Drop re-read the cursor at release, so it could disagree with the last shown preview | the drop performs `drop_preview_cache`'s action — the overlay's promise, verbatim |

Third recording (`xdrag3.trace`) still flashed. Probing Aim/Hide in the replay caught it exactly:
119 Aim / 119 Hide strictly alternating per `MouseDragged`, identical region every time. The root
cause of all three symptoms (flash, lag, wrong drops) was one line: `evaluate_drop_target` read
the session's origin-space hint through `get_active_drag_session`, which answers only in the
`Active` state — and a drag over a target alternates `Active ↔ PendingSwap` on every evaluation.
Every other sample lost the hint, fell back to geometry on the drag-swap manager's noted frame
(which lies on the *target* display mid-cross), concluded the dragged window was not tiled there,
found no candidates, hid the overlay, and demoted the pending swap — to rebuild it all on the next
sample (a RON preview recompute each cycle: the lag). A release landing on the wrong half of the
cycle dropped as a plain move: the wrong positions. Fix: `current_drag_session` answers in both
states; the space and origin hints read through it. The hardened cross-display test drives
consecutive evaluations through the `PendingSwap` state with the noted frame on the target display.

The discovery storm that rode on this — each mid-drag `SpaceStateChanged` answered with a full
every-app AX census, twice (~11 `WindowsDiscovered`/s with `get_window`/`live_frame` per window) —
is fixed: `request_visible_windows_for_apps` defers the sweep while a drag is in flight (same
mechanism as the refresh quarantine, `pending_visible_refresh`), and `MouseUp` flushes the one
census that is owed. Test: `discovery_sweeps_are_deferred_while_a_drag_is_in_flight`.

Tests: `cross_display_drag_previews_and_splits_the_target_under_the_pointer` (now also covers the
alternation), `edge_direction_divides_the_whole_target_into_four_triangles`,
`zone_boundary_wobble_does_not_flap_the_preview`.
Still owed: a live re-record (overlay steady over a target, no stutter, drop matches the preview).

## The trace harness (the important deliverable)

Rift can record everything the reactor sees and replay it offline, bit for bit.

Record on the running instance:

    rift execute trace start ~/Downloads/name.trace
    ... reproduce ...
    rift execute trace stop

Drop the file into `tests/traces/`; `cargo test --lib recorded_traces_replay_cleanly` replays every
`*.trace` there (`-- --nocapture` prints the report: requests, writes, state trajectory, drops,
live-vs-replay writes).

Format (`src/actor/reactor/replay.rs`, `src/sys/trace.rs`), one line each:
1. config (JSON — RON cannot round-trip flattened/untagged serde),
2. layout engine snapshot (RON),
3. `Windows {json}` — the window store,
4. `Transactions {json}` — per-window transaction ids/targets (without this, replay discards
   frame reports live accepted — the harness lied until this was added),
then, interleaved: `Ev <ms> <event json>`, `Sys {ms,kind,key,answer}` (every out-of-band system
answer the reactor got, recorded by `trace::observe` wrappers in `window_server`, `screen`,
`space_switch`, `event::get_mouse_state`, `scripting_addition`), `Out {..}` (every frame rift
wrote, hooked in `actor/app.rs`).

Recording starts mid-session, so the header is followed by synthetic `SpaceStateChanged`,
`ApplicationLaunched` (per known app, with its windows), `ApplicationGloballyActivated` and
`WindowServerFocusChanged` events, plus pre-recorded answers for the questions those launches ask.

Replay (`replay_trace`): thread-scoped replay mode; `trace::now()` returns recorded time; answers
are matched by (kind, key) as *state at the current replay time* (all lines ≤ now consumed, latest
wins, reused if asked again); misses and key drift are reported. Invariants checked after every
event / on every write:
1. no frame written to a window between its first button-down report and `MouseUp`;
2. no empty frame written;
3. a window is in at most one tree, and never floating and tiled at once;
4. no unprompted display bounce (writes to a display the window server does not have the
   window on, alternating within 1.5 s);
5. a float is written a frame only on a command's behalf.

Fixtures in `tests/traces/` (pruned after round 7 from 25 MB to ~4 MB — a recording made before a
fix diverges where the fix changed rift's writes, so its verdict covers only the prefix, and each
scenario is kept only in its latest recording): `seam_drops_tiled_premiere` (synthetic, 53
violations without the removal fix), `seam_drops_after_fix`, `user_float` (floats),
`user_altdrag2` (alt-drags, supersedes `user_altdrag`), `xdrag3` (cross-display drags, supersedes
`user_drag`/`user_drag3`/`user_drag4`/`user_drag5`/`xdrag`/`xdrag2`; the pruned recordings live in
the git history at `bc57386` and in `~/Downloads`).

Known limits: replay is open-loop — after a fix changes rift's writes, the recorded window-server
reactions no longer correspond; a clean replay of an old trace proves rift's handling of *those*
inputs, a live re-record proves the loop. Sequence-dependent gaps left: `is_point_occluded…`
(event-tap thread) and app-thread round trips (`GetVisibleWindows` gets no reply on replay; the
recorded `WindowsDiscovered` events stand in).

Latent bugs the harness surfaced in rift's own persistence (fixed): `MouseModifier` and
`SpaceStateChanged` weren't serializable; infinite `max_frame` broke JSON; a live engine snapshot
failed its own validation (workspaces on unexposed spaces — healed on snapshot load);
`ensure_space_initialized` underflowed with zero workspaces.

## Build / install (the running rift is this branch, not Homebrew's)

    cargo build --release
    codesign -f -s "Developer ID Application: Eric Wang (8UR4G77744)" -i com.performave.rift-plus --timestamp target/release/rift
    install -m 555 target/release/rift /opt/homebrew/opt/rift/bin/rift
    install -m 555 target/release/rift-cli /opt/homebrew/Cellar/rift/0.5.3/bin/rift-cli
    brew services restart acsandmann/tap/rift

Accessibility is keyed to the signature; a rebuild occasionally needs the toggle in
System Settings → Privacy & Security → Accessibility (rift polls and starts once granted).
Logs: `/tmp/rift_eric.err.log`. `launchctl setenv RUST_LOG rift_wm=debug` before a restart for
debug logs (unset afterwards).

## Open items

- Live validation of round 6 (build of 14:56+): alt-resize with two tiles, Preview opened through
  its picker then `sa+T`, clicks into Warp / another display's menu bar / a status-item popover.
  Anything off: record (`rift execute trace start … / trace stop`), drop into `tests/traces/`.
- ~~`tests/traces/` is 23 MB and in git~~ pruned to ~4 MB after round 7 (superseded recordings
  removed; see the fixture list above). Git history still carries the old ones.
- Logging defaults to `rift_wm=info` (`src/common/log.rs`); `RUST_LOG` overrides. Service logs:
  `/tmp/rift_eric.err.log`.
- `window_notify`/spaces-actor and event-tap threads make their own system queries; they reach the
  reactor as events, which is enough for replay but means those actors are not themselves replayed.
- ~~Every `MouseDragged` still triggers `window_spaces` queries for every window~~ fixed in round
  7: drop candidates come from tree membership, not per-move window-server queries.

## Round 8 (2026-08-30 evening): the "float snaps randomly" hunt, and the flight recorder

Symptom: the floating Warp window "snapped randomly" on release of a tab-bar drag; titlebar
drags were fine. Root cause (verified with rift fully stopped): **macOS relocates a
programmatically moved window that ends up straddling the display seam** ("Displays have
Separate Spaces" — a window rests on one display). Warp animates its tab drag itself, so a
seam drop is a programmatic placement and the system bounces it; server-side titlebar drags
are exempt. Fixes along the way, each real on its own:

| Fix | Where |
|---|---|
| Same-app focus resolved one click behind (sa+T toggled the sibling): resolver asked `key_focused_window(active_space())`, and the active-display designation lags; resolve across all visible spaces instead | `window_notify.rs`, `window_server::key_focused_window_across`/`visible_spaces` |
| A drop stored the float's frame **with a placement intent**, and the next drop-arrange asserted the stale intent — `follow_floating_position`, not `store_floating_position` | `events/drag.rs` |
| A float laid out at its own live frame could still become a *write*, racing mid-arrange live-frame drift — float positions are emitted only for intents (pending placement, un-parking) | `engine.rs` `calculate_layout_with_virtual_workspaces` |
| Mid-drag frame reports read the window server, not `elem.frame()` (no AX round-trips into the app during its own drag) | `app.rs` moved/resized handler |
| A dragged float's move/resize AX notifications are silenced for the drag (`SetDragNotificationSilence`) — Warp's AX bridge running per animation frame glitched its gesture | `app.rs`, reactor `sync_drag_notification_silence` |
| A seam-straddling float drop is **finished**: nudged minimally onto the display holding most of it (display-frame gaps accounted for), via a pre-layout write; replay invariant 5 exempts drop writes (`recent_drop`) | `events/drag.rs`, `replay.rs` |

Tooling (the important deliverable):

- **Flight recorder**: rift always records into a 32MB in-memory ring; `rift execute
  trace dump <path>` writes the recent history *after* a bug happened — no `trace start`
  needed. Dumps begin with a `Flight` line and are for reading, not replay.
- **`Act` lines** (`trace::act`): every rift thread records what it does — event-tap
  consumed events and slow (>1ms) callbacks, app-actor AX notifications with source and
  latency, raises, warps, EUI flips, drag silences, and all off-reactor system queries
  (focus resolver, window-notify). Replay ignores them; old traces load unchanged.
- `WindowServerFocusChanged` is serializable now (it was `serde(skip)` — recordings were
  blind to focus, and the round-1 bug class was invisible to the harness).
- `rift.rs` gained `RIFT_KEEP_WM_BRIDGE=1` to skip nulling the window-management-bridge
  delegate (A/B'd during the hunt; the null was exonerated and remains the default).

Caveat learned the hard way: synthetic CGEvent drags with modifier flags latch the synthetic
HID modifier state — post explicit `FlagsChanged` before/after, or every later synthetic drag
becomes a rift modifier-drag and poisons the experiment.

### Round 8 addendum: the final seam-drop policy

The first cut (nudge minimally onto the majority display, judged from the release-moment
frame) was wrong twice over: the release frame lags the hand (the app is mid-spring), so the
majority flips on animation phase — drops landed 842 px apart from one try to the next — and
the finish write *races* the system's own relocation, so whichever lands second won.

Final policy (`events/drag.rs` + `reactor.rs::assert_seam_finish`):

- The **pointer's display** keeps the window (the hand is the only honest witness); the frame
  is clamped fully into it.
- The finish is armed whenever a float drop is not resting on the pointer's display —
  covering both race orders (still straddling, or already relocated by the system).
- 250 ms after the drop the placement is verified against the **window server** (the
  relocation report trails rift's txid and the transaction gate discards it, so
  `frame_monotonic` lies), the window is SA-moved to the landing display's space if the
  system stranded it elsewhere (it can even land on an *inactive* space), and the frame is
  re-asserted — a frame fully on one display is never relocated, so this write is final.
  Two attempts max (`managers::SeamFinish`).
- Replay: invariant 5 exempts drop-finish writes; post-divergence unanswered questions no
  longer fail a fixture (new queries against old recordings are expected there).

Mid-drag flicker over the seam is Warp fighting macOS (programmatic straddling moves bounced
per animation frame) — reproduced with rift stopped; rift cannot fix it, only the resting
place.
