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

## `space: null` is almost never the wedge — check for fullscreen first

A display that reports `space: null` with no active desktops is, nearly always,
a display **showing a native-fullscreen space**. That is rift answering
correctly: a fullscreen space is not a desktop it manages. It reads exactly
like the wedge below, and telling them apart is the single most expensive
mistake available here — an afternoon went into rebooting the guest, A/B-ing
binaries and bisecting a regression that did not exist, because Safari was
sitting in fullscreen.

It persists, which is what makes it so misleading: **an app restores its own
saved window state**, so a scenario that leaves Safari fullscreen leaves it
fullscreen across relaunches *and across guest reboots*. Every run after that
one starts on a display with no desktop, tiles nothing, and reports an empty
layout on every desktop.

What makes it hard to observe is that "is a display showing a fullscreen
space" is not the same question as "is this app fullscreen". Fronting any
other app switches the display away from the fullscreen space and makes the
state look cleared while nothing has changed. The only reliable test is to
bring each app up *and then* look — which is what `clear_native_fullscreen`
does, and why `fullscreen_key` checks after `open -a` rather than before.

`chaos.py` now clears this before every baseline and before every `setup`,
prints `space=FULLSCREEN` rather than `space=None` in `render_displays`, and
`native-fullscreen-across-churn` clears it on the way out even when it fails.

## When rift really has stopped seeing displays in the guest: reboot it

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
6. **The return pass strands windows** (`straggler-after-return`). The VM
   reproduction of the 2026-09-21 host incident, root-caused below. **Fixed on
   2026-09-21**; see "the straggler bug" below for what the fix is and how it
   was seen working.

   Read the original evidence with care. The check compared raw desktop ids
   before and after, and macOS destroys desktops across a churn while rift's
   record substitutes a replacement for each one — so every window of a desktop
   legitimately comes back on a *new id*, together, with its tree. The log says
   so plainly (`replaced={SpaceId(72): SpaceId(85)}`). A run where all four
   windows moved 65 → 71 was that, not a straggler. The check is now
   `grouping`-based: it asks only who still shares a desktop with whom, which
   is exactly what a window left behind breaks and what a substitution does
   not.
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

And for a suspected *regression*, A/B against a build of `HEAD` from a detached
worktree — not against whatever old binary is lying around in the guest, which
differs by far more than the change under test. Give both sides the same
starting conditions, explicitly: quitting the apps first is what turned "my
build wedges and HEAD does not" into "both wedge, and the difference was
whether Safari happened to be running". The `ab` helper in the scratchpad does
exactly that and nothing else.

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

Four more, found the hard way on 2026-09-21:

- **`query layout --space-id` only answers for a desktop some display is
  showing right now.** Every other desktop comes back with no `container_tree`
  at all, which `tree_shape` marks `absent` — so "4 tiled windows on a desktop
  with no layout" is the ordinary state of any desktop that is not in front,
  not a bug. A scenario that needs to read a tree has to pick its window from a
  desktop that is being shown, and one that churns first has to go and find
  where the windows ended up (`show_the_desktop_holding_the_windows`).
- **The flight recorder is a ring, so slice it by timestamp, not by index.** A
  churn writes enough to wrap it; `trace_acts(...)[mark:]` on a list that has
  lost its head silently drops exactly the entries the assertion is about. It
  reported "no slot outcomes at all" for a window whose slot was recorded and
  restored perfectly.
- **`matrix` used to leave `displaced_windows` on whichever mode it finished
  with.** Every later single-scenario run then exercised a mode nobody chose —
  and `spaces` is the *only* mode with a display record, so the record's own
  scenarios ran against machinery that was not loaded and reported PASS. It now
  puts the config back.
- **Two identifier namespaces.** `window_map` keys on the window server id; a
  tree's leaves are rift's own `pid:idx`. Comparing one against the other finds
  nothing and says so as "the window is not tiled".

And in the `run` wrapper itself: a launchd job **stays registered after its
process exits**, so `launchctl print` succeeding says nothing about whether the
harness is still going. Poll its `state` field instead — the old check never
terminated.

Also worth knowing: `role == "stack"` is master-stack's name for its secondary
column and is an ordinary split. Only `layout_kind` ending in `_stack` is a
stack.

**Do not bulk-destroy the leaked desktops.** Walking them with `space
switch-to` + `space destroy` to tidy up left the window server showing a
desktop rift could not name, with every desktop inactive and nothing
manageable. That state survived a rift restart and needed a guest reboot. The
leak is noise; this cure is worse.

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

**`native-fullscreen-across-churn` now drives it.** It fullscreens a window,
churns a display while the window is away in its own space, brings it back, and
asserts on the trace as well as the end state: none of `"landed elsewhere;
dropped"`, `"nothing matched"` or `"stale slot replaced"` for that window, at
least one of `"restored"` / `"re-anchored"` / `"ordered in; restoring"`, the
window tiled again, and the tree in the same leaf order.

Three things it had to learn, each of which had it silently testing nothing:

- **Name the window, not the app.** `dtool fullscreen` posts the key to
  whatever is frontmost, and `open -a TextEdit` with three documents open is a
  coin toss — the trace showed a different window going fullscreen than the one
  the assertions were about. It now focuses the target through rift on every
  attempt.
- **Do not check "did it leave its tree" while it is fullscreen.** Its display
  is showing the fullscreen space, so the home desktop is not shown and has no
  readable tree, which made "no longer a leaf" true of every window including
  the ones that never moved. The `"recorded"` line in the trace, keyed on the
  window's own idx, is the honest check.
- **Settle properly before taking the slot.** A slot taken while rift still
  considers the churn to be settling is *kept*, not recorded
  (`"churn settling; slot kept"`) — correct behaviour, and it means the
  scenario would be measuring the previous run's leftovers.

**The result: native fullscreen round-trips correctly across a display churn.**
The trace reads `recorded → ordered in; restoring → restored` for the window,
repeatedly, with none of the three failure outcomes, and the tree at rest is
the same leaves in the same order with the window back in its slot — checked
directly against `query layout --space-id`, not only through the harness.

Everything the scenario reported before that was the scenario measuring the
guest mid-toggle, and both wrong answers are worth knowing because they are so
convincing:

- **A window "missing from its tree" after the round trip.** The trace said
  `restored`, and the harness said the window was tiled on a desktop whose tree
  did not contain it — the window-limbo signature exactly. At rest it was in
  the tree, in its slot. A posted key is not a transaction: `open -a` can front
  the app a beat after the state was read, and the retry toggles the window
  back *into* fullscreen. The scenario now clears fullscreen unconditionally
  and settles before it measures anything.
- **A window "left carrying a fullscreen-sized frame"** — `(0,0,1443,886)`,
  exactly display 1's full bounds — on a desktop nothing was showing. Same
  cause, and not reproduced since. Do not report a geometry reading taken while
  an app might be mid-transition; a fullscreen frame on a window that is
  fullscreen is not a finding.

Two things had to be true before the assertions meant anything, on top of the
three above: `"stale slot replaced"` is only a failure *during* the churn (it
is the mechanism working when it drops a slot an earlier run left behind), and
the whole leaf list cannot be compared for equality, because a churn
legitimately moves other windows onto the desktop. The order of the windows
that were already there is the invariant; a subsequence check is what states
it.

**The straggler bug, from the 2026-09-21 host trace — fixed.** A separate root
cause from the replug remap bug, and the better-understood of the two:

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

The fix keeps the record's answer alive past the record. `finish_pass` now
leaves an `Aftercare` behind on `Stage::Back` — where each window belonged, on
the desktop ids of now — and a window that turns up somewhere the pass did not
put it is sent home, once, while the window server's own
`SLSGetDisplayReconfigureTimeWhenWindowsLastMoved` says it has been moving
windows for a display change. A window the *user* moves fails that test and
stays where they put it. Five seconds, deliberately short (`AFTERCARE`).

It was seen working in the guest, which is the only evidence that counts here:
`record_aftercare [407, 76, 4]` — window 407 landed on desktop 76 after the
pass and was sent back to 4. The pass it followed reported the bug's own
signature, `reconcile [5, 0, 0, 6]` with `windows_waited_for=0`.

Worth adding on top: assert from the trace rather than only from queries —
`windows_waited_for > 0` whenever `topology_window_delta` is non-null in the
snapshot following `pass_done ["Back", …]`. That asserts the mechanism instead
of its downstream symptom.

**`compute_space_remaps` — one defect fixed, the guards still worth auditing.**
Note from the host trace that its signature — `topology_changed: true,
allow_space_remap: true, space_remaps: []` — appeared and was *harmless*,
because `display_record` carried the identity independently. So the guards are
a real defect but not the cause of every symptom blamed on them.

What was fixed on its own evidence is narrower and provable: a snapshot too
incomplete to tell a desktop *switch* from a desktop *replacement* declined to
remap and then wrote the desktop the display was showing into
`last_user_space_by_display` on its way past — answering the question it had
just said it could not answer. The next snapshot, coherent this time, saw
nothing to remap and the replaced desktop's layout was gone for good. The
history now waits for a snapshot that can be trusted
(`an_untrusted_snapshot_does_not_cost_the_remap_that_follows_it`, which fails
without the guard). The two content guards — `source_still_exists` and
`source_is_now_owned_by_another_display` — are untouched and still want
evidence of their own.

**rift dies on an unknown config key.** `src/bin/rift.rs:190` unwraps the config
parse, and `MouseSettings` is `deny_unknown_fields`, so running an older binary
against a newer config panics at startup rather than warning and ignoring the
key. This bit the A/B test here and would bite any downgrade.
