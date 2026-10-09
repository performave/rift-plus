# Performance: open findings

A read-only audit (2026-09-02) listed where rift spends time it does not need
to. The findings below were re-checked against the code on 2026-09-29 and are
still present. The full audit, including the lower-impact items not re-checked
here and the list of paths already verified efficient, is in git history
(`f3222e9`).

Frequencies: *per event* is every reactor event, *per pass* every layout
arrange.

## Always-on instrumentation

**The flight-recorder ring is paid for by every thread** (per event, per system
query, per frame write). The recorder is on from startup, so `is_recording()`'s
fast path never skips. Each line is JSON-encoded (a `Sys` line encodes its
answer and then the whole line again), `elapsed_ms()` locks the global `MODE`
mutex, `write_line` locks it again and then `RING`, and copies the string.
App threads writing frames, the event tap and the focus resolver contend on
the same two locks. *Fix:* take the string by value, take the clock once and
keep its start in an atomic, and consider per-thread buffers merged on dump.
`src/sys/trace.rs`.

**INFO-level `#[instrument]` spans Debug-format their payload** (per event, per
AX request and notification). The default filter is `rift_wm=info`, so the
reactor's `handle_event` span renders every event, and the app actor's spans
render `self.app` (an `AXUIElement`, whose Debug calls `CFCopyDescription`) on
every request. The `tracing-timing` layer, read only by `show-timing`, takes a
process-wide lock per span. *Fix:* `level = "debug"` on the hot spans or record
a discriminant only; install the timing layer behind an env var.
`src/actor/reactor.rs`, `src/actor/app.rs`, `src/common/log.rs`.

## The arrange path

**Every arrange builds the menu bar's workspace list** (per pass).
`maybe_send_menu_update` returns early only when there is no menu channel, and
there always is one. `query_workspaces` then lays out every inactive workspace
(`calculate_layout_for_workspace`) — for a menu bar that is disabled by
default. *Fix:* gate on `settings.ui.menu_bar.enabled`, or let the menu actor
pull after its debounce. `src/actor/reactor/query.rs`.

**One full arrange per changed layout event** (per event batch).
`send_layout_event` calls `update_layout_or_warn` whenever a layout event
reports a change, so an app arriving with N windows runs N full passes and then
the outcome's own. *Fix:* accumulate `changed` across the batch and arrange
once. `src/actor/reactor.rs`.

**Floats are re-read from the window server once per screen per pass.**
`refresh_floating_frames_from_window_server` runs inside the per-screen loop, one
`live_window_frame` query per float, and passes fire per `WindowResized` during
a resize drag. *Fix:* hoist it out of the loop.
`src/actor/reactor/managers.rs`.

**A space switch resolves every visible window twice.**
`authoritative_active_space_windows` calls `resolve_native_space` per window,
and `refresh_active_space_window_membership` calls it again on the same list.
*Fix:* resolve once and trust the passed-in space. `src/actor/reactor.rs`.

**Broadcast snapshots are built with nobody subscribed** (per layout change).
The full layout-state payload is built on the reactor thread whenever the
outcome asks for a broadcast; the subscriber check happens later in the IPC
server. *Fix:* expose "has subscribers for this kind" and skip the build.

## Persistence

**Every save round-trips the engine through RON, then fsyncs on the reactor
thread** (per 60 s autosave, and on window removal and fullscreen entry, when
the reactor is already busy). `loadable_snapshot` serializes, parses back,
prunes, and the save serializes again, then `sync_all()`s the file and the
directory. *Fix:* prune through a borrowed view and hand the string to a
blocking thread for the write. `src/layout_engine/engine/persistence/storage.rs`.

## The macOS edge

**One AX round trip per tracked window on every main-window change.**
`remove_stale_windows` reads `role()` for every window the app has on each
`AXMainWindowChanged`, so every click between an app's twenty windows is twenty
blocking IPCs into its main thread. *Fix:* check staleness in one batched
window-server query. `src/actor/app.rs`.

**The spaces actor asks every visible window's space on every snapshot, and
uses the answer only on a conflict.** `build_forwarded_state` calls
`window_space` for each window before `record_visible_window_space`, which
reads it only in the `Occupied` arm. *Fix:* compute it lazily there.
`src/actor/spaces.rs`.
