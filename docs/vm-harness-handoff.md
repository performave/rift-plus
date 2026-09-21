# VM chaos harness — state, findings, and what to do next

Written 2026-09-21. Companion to [vm-harness.md](vm-harness.md), which is the
provisioning spec; this is the running log of what the harness has found and
what is still open.

## Where things stand

The guest (`vm@192.168.64.6`, macOS 27.0, SIP off, `-arm64e_preview_abi` set) has
rift deployed at `~/rift-harness/bin/rift`, Accessibility granted by a direct
TCC.db row, the user's own `config.toml`, and the scripting addition loaded.
`scripts/chaos.py` runs inside its Aqua login session and drives real display
hotplug through `scripts/vdisp.m`.

**Known-good procedure** is in `vm-harness.md`. The three mechanics that cost the
most time are worth repeating:

- rift-cli finds rift by **mach bootstrap lookup**, and an SSH session is in a
  different namespace. `launchctl asuser` is the documented bridge but needs
  root; bootstrapping a LaunchAgent into `gui/<uid>` reaches the same namespace
  with no privilege at all. Run the *whole* harness as one such job — a
  bootstrap costs 1–2s, so per-command dispatch is far too slow for loops.
- `rift-cli execute window focus` requires **`--window-id '{"pid":P,"idx":I}'`**.
  Passing only `--window-server-id` exits rc=2, which in a loop fails silently
  and leaves every `toggle-float` landing on whatever was already focused.
- `rift-cli query` needs the literal `query` subcommand; `rift-cli displays`
  returns empty rather than erroring visibly.

## When rift stops seeing displays in the guest: reboot it

After a long churn session rift can end up reporting `query displays` → `[]`
and emitting no `SpaceStateUpdated` at all, while `dtool count` in the same
guest still reports a display. CoreGraphics still lists it; rift's
SkyLight-based enumeration returns nothing.

**Rebooting the guest clears it.** That was checked: an initial guess that the
host display being asleep was the cause turned out to be wrong — the guest
reported `UserIsActive 1` throughout, and a reboot fixed it with the host in the
same state. The likeliest cause is the wedge the audit predicts: after enough
cycles `screen_snapshot_is_ready_for_authoritative_commit`
(`src/actor/spaces.rs:1078`) has no committable snapshot, both retry budgets
(`spaces.rs:1151`, `spaces.rs:1387`) are exhausted, and nothing reschedules.
There is no watchdog reconciling `query displays` against
`CGGetActiveDisplayList`, which is itself worth fixing.

So: if the guest goes quiet, reboot it before debugging anything else, and do
not read a `[]` as a finding until you have.

Two things a reboot does **not** clear:

- **The churn-created desktops.** They are macOS state, not rift's. After a
  reboot they come back renumbered (`[1,5,6,7,8,9,10,11]` where they had been
  `[1,129,139,174,179,488,493,513]`), so the leak survives restarts and only
  the ids change.
- **The display arrangement.** `dtool setmain` configures
  `kCGConfigurePermanently`, so a display moved to a negative origin stays
  there across reboots. Reset it with `dtool setmain <the display you want
  main>` before taking a baseline, or every geometry result is measured against
  a skewed arrangement.

## What the harness found

Five findings survived verification. Each was re-run after the false positives
below were removed.

1. **Slot reordering on churn, deterministic.** With four windows the same two
   swap the same way in `plain-replug`, `different-monitor` and
   `churn-during-space-switch`. A no-churn control (`chaos.py control`) gives
   byte-identical leaf order across four snapshots, so the tree is stable at
   rest and the reordering is churn-induced.
2. **One empty desktop leaked per churn cycle.** Observed growing monotonically
   from five to eight within a session. Same shape as the 2026-09-20 LG replug,
   which left desktops 162 and 664 as empty shells. `check_no_desktop_leak`
   asserts the set does not grow across a round trip.
3. **Windows stranded off-screen after a main-display change.** Attach a
   display, make it main (which moves the other display's origin negative),
   detach — and windows keep frames from where the old display used to be.
   Four windows, at rest: rift counted four tiled, one was visible, two sat at
   negative x and one was buried. This is the "things are all over the place"
   state.
4. **A stack does not survive a display attach.** A three-member
   `vertical_stack` was gone the moment a second display was plugged in —
   `[(('396:118','396:117','396:115'),'vertical_stack')]` before, `[]` after.
   No window was lost, so only a check that compares stacked containers keyed
   on their ordered members sees it.
5. **Windows overlap almost completely for a moment mid-churn.** Two windows
   overlapped by 1757x1054 — one effectively on top of the other — in one of 29
   samples taken through a plug/unplug, while the settled states either side
   were clean. Only the continuous sampler sees this; a before/after pair
   cannot.
6. **The return pass strands windows** (`straggler-after-return`). Two windows
   went from desktop 61 to 65 and were never brought home — the VM
   reproduction of the 2026-09-21 host incident, root-caused below.
7. **A window comes back taller than any display after a long absence**
   (`long-absence`). Safari returned at `779x1879` where the displays are
   1049 and 1064 tall. Reproduced twice, once on a deliberately clean display
   arrangement to rule out the skew a failing `setmain` had left behind.
   Whether Safari or rift chose that height is not yet established — checking
   the trace's `arrange_calc` for the frame rift actually asked for would
   settle it.

## Findings that were retracted — read this before adding checks

Five results looked like rift bugs and were not. Each is a trap the next
person will hit:

- **Frame overlaps.** A slot narrower than an app's minimum size makes the app
  render at its minimum and overflow into its neighbour. Eight windows in a bsp
  spiral drives the deep slots to 46px, which is under TextEdit's floor, so
  nearly every scenario "failed". At four windows: clean. Keep the window count
  low and prefer apps that resize freely — Calculator and Chess are effectively
  fixed-size and will also refuse to *grow*, leaving holes.
- **`mode changed bsp -> None`.** A desktop whose display has left is still
  listed but has no layout to report. `tree_shape` marks that `absent` and the
  structural checks skip it.
- **"Tiled window came back floating" under `displaced_windows = "float"`.**
  That is the mode working as designed. `check_tiled_stayed_tiled` is now
  mode-aware.
- **"Stale display frame."** rift reported 1767x1064 where the window server
  said 1823x1095; the deltas are exactly the menu bar and the Dock. It is the
  visible frame, computed correctly.
- **A 40-minute "hang" in `fullscreen-roundtrip`.** That was `GX_TIMEOUT` in the
  harness, not rift.

The discipline that caught all five: before reporting a geometry finding,
re-run it with fewer windows; before reporting a structural finding, run
`chaos.py control` and confirm the thing is stable when nothing happens.

## Traps inside the harness itself

Three scenarios silently tested nothing until these were found. Any new
scenario should be checked the same way — make it fail first, on purpose.

- **`toggle-stack` is a hard no-op in bsp** (`apply_stacking_to_parent_of_selection`
  returns immediately) and moves a window to the next column in scrolling. Only
  `traditional` and the `stack` mode itself can hold a stacked container, so a
  stack scenario has to `workspace set-layout traditional` first.
- **`toggle-stack` acts on the selected *container*, not a window.** Without a
  `layout ascend` to walk the selection up to the parent split, it does
  nothing. The scenario "passed" for a while on a stack that never existed.
- **`space create` acts on whichever display owns the menu bar**, so making a
  second desktop *on the external* means making the external main first. It
  also needs the scripting addition.
- **The transient sampler must be cheap.** A full `snapshot()` is a rift-cli
  call per desktop for windows and another for the layout — twenty-odd round
  trips once churn has left a pile of desktops behind, which cannot finish
  inside the sampling interval. The unfiltered window list is one call and
  carries the frames, which is all an overlap check needs.

Also worth knowing: `role == "stack"` is master-stack's name for its secondary
column and is an ordinary split. Only `layout_kind` ending in `_stack` is a
stack.

## Open work

**Native fullscreen is drivable now, and round-trips correctly in the simple
case.** With the desktop pinned by `--space-id`, a three-leaf traditional tree
came back as the same three leaves in the same order. During the fullscreen the
desktop is gone and `query layout` answers
`{"message":"Space or workspace not found"}` — correct, not a failure, and a
scenario has to expect it.

Two things made that test lie before it told the truth:

- **`osascript` hangs indefinitely** inside a LaunchAgent in the guest, for both
  `activate` and `System Events` keystrokes, and still hangs after granting
  `kTCCServiceAccessibility` *and* `kTCCServiceAppleEvents` to `/bin/bash` and
  `/usr/bin/osascript` against `com.apple.systemevents` and
  `com.apple.TextEdit`. The TCC grant is not the missing piece.
  `dtool fullscreen` posts Ctrl-Cmd-F with `CGEventCreateKeyboardEvent` +
  `CGEventPost` instead, underneath Apple Events, and works. It needs
  `open -a <App>` first, because a posted key goes to whatever is frontmost and
  a LaunchAgent is not an app.
- **Unfiltered `query windows` reports the active desktop only**, and native
  fullscreen changes which desktop that is. Read across a round trip it showed
  a window "returning" at 656x422 instead of 582x1054 — three different
  desktops being compared, not a layout failure. Always pass `--space-id`.

What is still untested is native fullscreen *interleaved with a display churn*:
the three failure modes in `src/actor/reactor/fullscreen_slots.rs`, whose nine
`fullscreen_slot` trace strings (`"recorded"`, `"restored"`, `"re-anchored"`,
`"landed elsewhere; dropped"`, `"nothing matched"`, …) are the assertions worth
making. The tooling to drive it now exists.

**The straggler bug, from the 2026-09-21 host trace.** A separate root cause
from the replug remap bug, and the better-understood of the two:

> `finish_pass` (`src/actor/reactor/display_record.rs:1651`) does
> `record.take()` on `Stage::Back`, and every straggler guard — `CHURN_SETTLE`,
> `PLACEMENT_AFTER_CHURN` (`display_record.rs:960-968`) — reads through that
> record, so all of them die with it. The pass declares itself finished
> (`windows_waited_for=0`) before the window server has finished reassigning
> window→space membership. In the captured incident a window arrived on the
> wrong desktop 1.24s after `pass_done` and stayed wrong for 51s, until the user
> moved it by hand. rift had retained the correct home the whole time —
> `fullscreen_slot [3135,"recorded",5,1991]` — and simply had no trigger to act
> on it.

`straggler-after-return` in `chaos.py` encodes the reproduction. Its three
preconditions are all load-bearing: the survivor must own exactly one desktop
with a tiled window (so macOS destroys it and rift mints a stand-in), the
external must own two desktops while *showing* the empty one, and the unplug
must last about 2.8s — long enough for a full Away pass, short enough that
macOS is still migrating `display_space_ids` when Back completes.

Worth adding on top: assert from the trace rather than only from queries —
`windows_waited_for > 0` whenever `topology_window_delta` is non-null in the
snapshot following `pass_done ["Back", …]`. That asserts the mechanism instead
of its downstream symptom.

**`compute_space_remaps` is still untouched.** It is the original bug and the
one fix that has not been attempted. Note from the host trace that its
signature — `topology_changed: true, allow_space_remap: true, space_remaps: []`
— appeared and was *harmless*, because `display_record` carried the identity
independently. So the guards are a real defect but not the cause of every
symptom blamed on them; fix them on their own evidence.

**rift dies on an unknown config key.** `src/bin/rift.rs:190` unwraps the config
parse, and `MouseSettings` is `deny_unknown_fields`, so running an older binary
against a newer config panics at startup rather than warning and ignoring the
key. This bit the A/B test here and would bite any downgrade.
