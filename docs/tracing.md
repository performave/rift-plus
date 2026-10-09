# Traces and the flight recorder

Most of rift's hard bugs are two of its records disagreeing — which tree a
window is in, whether it floats, which display it is on, where it was last seen
— and rift acting on the stale one, in a window of a few hundred milliseconds
that no log line at `info` captures. Reading code has found few of them. Traces
found most.

## The flight recorder: reproduce first, dump after

rift always records into a 32 MB in-memory ring, from startup. After something
goes wrong:

```bash
rift execute trace dump /tmp/what-happened.txt
```

The dump starts with a `Flight {"dumped_at_ms":…}` line (tie the recorder's
clock to the wall clock with it; the ring's `ms` is since process start and
pauses while the machine sleeps). It carries:

- `Ev` — every event the reactor handled, with full payloads. `WindowInfo`
  includes `ax_role`/`ax_subrole`, so a window's AX identity can be read
  retroactively.
- `Sys` — every out-of-band answer the reactor got from the system (window
  server, screens, space switching, mouse state, the scripting addition).
- `Out` — every frame rift wrote.
- `Act` — what every *other* thread did: event-tap events it consumed and slow
  (>1 ms) callbacks, app-actor AX notifications with their latency, raises,
  warps, and the named decisions of the display record (`record_members`,
  `pass_done`, `display_churn_end`, …).

A dump is for reading, not replay: it has no state header. It is a ring, so a
long churn can wrap it; slice it by timestamp, not by line index.

## Recording a replayable trace

```bash
rift execute trace start ~/Downloads/name.trace
# reproduce
rift execute trace stop
```

A trace starts with the config (JSON — RON cannot round-trip the flattened and
untagged serde types the events use), the layout engine snapshot, the window
store and the per-window transaction state, then synthetic events that bring a
fresh reactor to the moment recording began. After that, `Ev`, `Sys` and `Out`
lines as above.

Drop the file into `tests/traces/` and every `cargo test` replays it
(`recorded_traces_replay_cleanly`; `-- --nocapture` prints the report: the
state trajectory, every write, and live-versus-replay writes).

## What replay checks

Replay runs a fresh reactor over the events, answering its system questions
from the recording by (kind, key) as the state at the current replay time, and
reads the clock from the recording too, so holds and grace periods elapse as
they did. Frames are captured, not sent, and checked against invariants no
window manager should break whatever the apps and the window server do (see
`src/actor/reactor/replay.rs`):

1. no frame written to a window while the user is dragging it;
2. no empty frame written;
3. a window is in at most one tree, and never floating and tiled at once;
4. no window bounced between displays by rift on its own;
5. a float is written a frame only on a command's behalf (or to finish a drop).

## Replay is open-loop

The recorded window server reacted to what *live* rift wrote. Once a fix
changes rift's writes, the recorded reactions no longer correspond, and nothing
after that point means anything. Replay detects this: it matches each replay
write to the nearest live one, and the first mismatch marks the trace
**diverged**; later violations are printed as `after_divergence` and do not
fail. So a trace recorded before a fix proves the fix up to its first changed
write and nothing after. A fix is validated by a fresh live recording on the
fixed build, which should not diverge at all.

Only the reactor thread is replayed. The app actors, the event tap and the
window-notify thread reach it through events, which is enough for replay but
means those threads' own behaviour is not re-run.

Keep `tests/traces/` small: a scenario needs only its latest recording, since
an older one's verdict covers less of it.

## Useful switches

- `RUST_LOG=rift_wm=debug`, set with `launchctl setenv` before restarting the
  service (and unset after), for the per-event debug stream. The default is
  `rift_wm=info`.
- `RIFT_KEEP_WM_BRIDGE=1` leaves the window-management bridge delegate alone
  instead of nulling it, for A/B-ing that.
- Synthetic CGEvent drags posted with modifier flags latch the synthetic
  modifier state. Post explicit `FlagsChanged` events before and after, or every
  later synthetic drag becomes a rift modifier-drag.
