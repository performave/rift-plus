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
- **Deploy `rift` and `rift-cli` together, never one alone.** rift-plus answers
  to `com.performave.rift-plus` as of the identifier rename; a client from
  before it looks up `git.acsandmann.rift` and finds nothing. The fallback runs
  the other way only — a *new* client still reaches an *old* rift — so
  upgrading the daemon on its own is the combination that breaks, and it breaks
  looking exactly like the wedge below: every query empty, no error.

  What that rename does *not* cost, here: Accessibility. macOS keys the grant
  on the signing identifier, so a real install has to be re-approved, but this
  guest's rows were inserted by hand with `client_type = 1` — keyed on the
  absolute path — and a binary replaced at the same path keeps the grant
  whatever its identity. `sudo sqlite3 '/Library/Application Support/com.apple.TCC/TCC.db'
  "select service, client, client_type, auth_value from access where client like '%rift%';"`
  is how to check before assuming it is gone.

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
whether Safari happened to be running". [`scripts/vm-ab`](../scripts/vm-ab)
does exactly that and nothing else; it drives
[`scripts/vm-run`](../scripts/vm-run), which drives
[`scripts/vm-ssh`](../scripts/vm-ssh).

### What a comparable run costs

Four separate defects made two "identical" batteries disagree, none of them in
rift. Each is worth knowing about because each one produced a number I
believed:

- **The guest keeps its own copy of `chaos.py`, and nothing used to push it.**
  A run therefore measured whatever revision was last copied across by hand.
  `vm-run` now `scp`s it on every invocation; if you add a runner, do the same.
- **Every scenario was judged against one snapshot taken before the first of
  them.** The ninth scenario was charged with everything the previous eight
  left behind, which is how one slot reordering got reported three times.
  `scenario_start` now takes a fresh snapshot after the reset and hands *that*
  to the scenario; the residue the reset could not clear is printed instead of
  being folded into a verdict.
- **Native fullscreen is not detectable at all when its space is not shown,
  and often not even when it is.** Apps reopen in the state they were closed
  in, so a reboot restores a fullscreen Safari onto a desktop nothing is
  showing, where `displays_showing_fullscreen` cannot see it. The obvious
  substitute — look for a window whose frame covers a whole display — does not
  work either, because rift drops a fullscreen space's windows from `query
  windows`; measured on 2026-09-21, Safari fullscreen *and in front* listed no
  window whatsoever. Nothing distinguishes that state from "nothing is
  fullscreen" except fronting each app and looking, so
  `reset_between_scenarios` runs `clear_native_fullscreen` unconditionally.
  Guarding it on a detector is guarding it on the one question that cannot be
  answered without running it — and the run where it was guarded left Safari
  fullscreen through the whole battery, failing `check_frames_within_display`
  in scenario after unrelated scenario.
- **`vm-ab` piped the summary through `head -30`.** Fifteen scenarios' reasons
  do not fit, so the last few came back blank — which reads exactly like
  scenarios that failed without saying why.

Until a battery repeats itself twice in a row, treat its count as a symptom of
the harness, not a verdict on the build. Two runs of the same build on the same
binary disagreed on six of fifteen before these four were fixed.

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
- **A split ratio survives in `layout.ron` and poisons every later run.** After
  an afternoon of dragging boundaries about, the guest had a `ratio:0.95` saved
  — a 5% slot, about 115px — and every geometry scenario then failed on
  "tiled windows overlap", because TextEdit will not render that narrow and
  spills into its neighbour. Same shape as the app-minimum confound above, with
  a cause that outlives a reboot. `rift execute layout balance` resets every
  split in the active workspace to an even share; run it before a battery, and
  suspect it first when an overlap appears out of nowhere. The saved file says
  so plainly: `grep -oE 'ratio:[0-9.]+' ~/.rift/layout.ron`.

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

## What only a hand on the mouse found

The upstream sync (`docs/upstream-sync.md`) compiled clean, passed all 790
tests, and gave **identical** results to the pre-merge build across the whole
churn battery — and had killed every mouse gesture rift owns.

Upstream's event tap subscribes to mouse *down* and *up* and not *dragged*,
because it acquires drags through AX. This fork drives modifier drags, the
tile-edge grab and the float-strip takeover from the tap itself, so it needs
the whole sequence. Taking upstream's mask captured every press and left
nothing to continue it. Nothing failed to compile; no test noticed; the churn
scenarios do not touch the pointer, so they did not notice either.

`scripts/handson.py` with `scripts/mtool` is what noticed: it posts real
pointer input as CGEvents and then reads the frames back. Anything that changes
`src/actor/input.rs` — a merge above all — has to be run through it.

Its own failures were all aim, and each is a trap in its own right:

- **The two left-most windows are not side by side.** In a bsp spiral they are
  usually stacked one above the other, so the "boundary" between them is a
  point in empty space. Require a pair that is horizontally adjacent *and*
  overlaps vertically.
- **Drag the boundary in the direction that grows.** An app at its minimum
  width refuses to shrink and the boundary does not move — Safari's floor is
  574px, which is exactly what a balanced five-window spiral hands it on this
  display. Growing always works; shrinking is the app's decision.
- **The modifier's two buttons do different jobs.** `action1` is on the left
  and `action2` on the right, and this config maps move and resize
  respectively — so a resize test that sends the left button is testing move.
- **Count windows across every desktop, not the one you started on.** In
  `spaces` mode a departing display hands its desktops back and windows
  legitimately end up on a different one. Counting a single desktop reported
  five tiled windows becoming three after a plug/unplug and read as a loss;
  counting all of them gives five, and a separate census plus the trace
  (`reconcile [0,0,0,16]`, `pass_done ["Back",16]`) confirms nothing was
  stranded — the frames had only re-flowed for the remaining display.
- **Modifier gestures are for floating windows.** On a tiled one they correctly
  do nothing. Float the window first, and grab three-quarters across rather
  than dead centre: the resize takes an edge, and the middle has no nearer one.

With those right, the merge passes: the boundary moves by exactly what it was
dragged and both neighbours follow with the gutter unchanged, a floated window
resizes by exactly the drag, stacking round-trips, native fullscreen returns
every window to its desktop, and a plug/unplug survives.

Worth knowing: modifier-drag **resize** works on the merged build and did
nothing on the pre-merge one. Restoring the dragged-event subscription fixed
more than it put back.

## What this guest cannot test at all

Worth knowing before trusting a clean run, because these are not gaps in
coverage that more scenarios would close:

- **Gestures.** `ioreg -c AppleMultitouchDevice` finds nothing here and there
  is no trackpad, so the swipe and scroll paths — rift reads them from IOHID
  directly — never fire. A good part of what upstream changed in the v0.5.10
  sync lives there (`perf: gesture scrolling`, `fix: get rid of gesture
  cooldown`, the gesture half of the combined tap) and none of it has been
  exercised. `mtool` can post mouse events; it cannot manufacture a touch
  device.
- **Anything needing a second *physical* display.** `CGVirtualDisplay` gives a
  real hotplug, but it is always the same synthetic panel: no mixed scale
  factors, no real EDID, no display asleep while another is awake.
- **The user's own hardware quirks.** The LG replug that started this work has
  a pseudo-display phase (see `settle-and-return-in-one-report`) the virtual
  display does not reproduce.

## Where this left off (2026-09-21)

The upstream v0.5.10 sync is on `main` and the fork is level with upstream for
the first time. `just check` is green, and the hands-on pass is clean.

### The sync, measured

Fifteen scenarios, run on the merged build and on the fork's `main` at the
merge base, back to back on one guest with the same harness and the same
`vm-ab` preamble:

| | pre-merge `0.5.5-plus.5` | merged `0.5.10-plus.1` |
| --- | --- | --- |
| failed | 11 of 15 | 13 of 15 |
| identical on both | 11 fail, 2 pass | — |
| differed | `transient-glitch`, `straggler-after-return` — passed before, failed after |

**Eleven of the fifteen fail the same way on both builds**, and
`Safari at (2600,260,1324,856)` — the same stranded frame, to the pixel —
appears in both columns. That family is rift's pre-existing behaviour under
churn; the sync did not introduce it and is not what to look at for it.

The two that differ are not yet regressions. `straggler-after-return` failed on
the merged side while building its *own preconditions* — "external needs two
desktops and has 1", which is `space create` acting on whichever display owns
the menu bar, not a churn result. `transient-glitch` saw overlap in 33 mid-churn
samples on merged and none on pre-merge, from a sampler that runs *during* the
churn, where timing decides what it catches. A difference that does not repeat
is timing; re-run both on both builds before believing either.

### What a number from this battery is worth

Judge a count against the other column, never on its own. On the merged build
alone the same suite has read 5, 10 and 13 failures on the same binary within
one afternoon, each time because the harness changed underneath -- see
[the four measurement defects](#what-a-comparable-run-costs) above. The A/B is
robust to exactly that: whatever the harness gets wrong, it gets wrong twice.

Still never run: `matrix`, so `float` and `tile` restoration modes are
unexercised against the sync.

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
Two consecutive clean runs once the harness stopped racing itself (below).
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
