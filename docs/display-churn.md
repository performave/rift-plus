# Display churn testing

A display appearing and disappearing is the event rift gets wrong most often.
The bugs live in the seconds between the reconfiguration callbacks — space ids
renumber, desktops are created and reaped, windows are moved by macOS itself —
and reproducing any of it has meant reaching behind the machine and pulling a
cable.

`just churn` does it in software instead.

```bash
just churn --dry-run          # check invariants against the current state
just churn --cycles 20        # 20 connect/disconnect cycles
just churn --cycles 200 --keep-going   # leave it running; collect every failure
```

## What it actually does

It connects and disconnects a *virtual* display. macOS treats one as a genuine
hotplug: it allocates a real desktop for the new display, renumbers, moves
windows onto and off it, and fires the same `CGDisplayReconfigurationCallback`
sequence a cable does. This was measured, not assumed — attaching the probe
gives it a real space id of its own, which is the whole reason the technique is
worth anything.

The real displays are never touched. The machine cannot end up with no screen,
which is the failure mode that makes the obvious alternative — disabling a real
display with `SLSConfigureDisplayEnabled` — a bad default.

## Requirements

```bash
brew install --cask betterdisplay
brew install waydabber/betterdisplay/betterdisplaycli
```

BetterDisplay must be running, with CLI integration enabled (the default).

## Invariants

Checked after every attach and every detach:

- every display has a UUID;
- the desktop a display is showing is one of its own active desktops;
- no desktop is claimed by two displays;
- exactly one display holds the active context;
- after detaching, the display set is the baseline set again;
- after detaching, every display that never went away is showing the same
  desktop it was showing before — the regression that keeps coming back;
- no window has been lost — checked per desktop, not just the active one;
- **windows that shared a desktop before the cycle still share one after it.**

That last one is the only invariant that catches a whole class of failure the
others are blind to. On 2026-09-20 an external display came back on a freshly
minted desktop, its workspace was never carried across, and windows that had
shared a desktop for hours were re-adopted one at a time onto whichever desktop
macOS dropped them on. Every display-level check above passed — the displays
really were all correct. What was wrong was where the windows ended up.

It compares groupings rather than desktop ids, because ids renumber across a
hotplug as a matter of course; that renumbering is the thing under test, not a
failure. And it compares only the windows present both before and after, so an
app opened or closed mid-run is not mistaken for the manager scattering things.

On a violation the offending snapshot is printed and the flight recorder is
dumped to `/tmp/rift-churn-<cycle>-<timestamp>.trace`, which replays offline.

## Why it polls instead of sleeping

Display changes are asynchronous, and not briefly: macOS goes on listing a
disconnected display for around a second after it is gone. A fixed sleep long
enough to be safe wastes most of the run, and one slightly too short reads the
display set mid-transition and blames rift for the window server still catching
up. An early version of this script did exactly that and reported twelve
failures that were all its own impatience.

So each step polls until the display set is what was asked for, requires it to
hold, and fails only on a timeout — which is a real defect. The convergence
time is printed per cycle, and is itself the interesting number: attach settles
in roughly 0.3–0.7s and detach in 0.7–1.2s on an M-series machine. A cycle that
takes much longer is worth looking at even when it passes.

## The probe display

Created on demand with a fixed identity (`rift-churn-probe`, vendor 8888) and
reused. That matters: macOS writes a permanent root-owned ICC profile into
`/Library/ColorSync/Profiles/Displays` for every display identity it has ever
seen, and nothing removes them — a script that minted a fresh identity per run
would litter indefinitely. `--teardown` removes the device at the end; the next
run recreates it under the same identity.

## What this does not cover

A virtual display is a well-behaved one. It will not reproduce anything
specific to a particular monitor's EDID, the pseudo-display a lid close
reports, or the space renumbering that follows sleep — that one is triggered by
wake rather than by topology change. Those still need real hardware.

For running churn continuously without it moving your own windows, see
[vm-harness.md](vm-harness.md).

## How rift reconciles a display change

Almost every churn bug has had one root: rift reasons in `SpaceId`, and macOS
destroys desktops and mints new ids at every reconfiguration, moves a desktop
between displays as a matter of course, and on macOS 27 reaps empty ones
seconds afterwards. No id scheme survives that — a destroyed desktop cannot be
identified because it no longer exists. What does survive is the **windows**:
window-server ids outlast unplugs, lid flaps and restarts of rift. So the
durable way to ask "is this the same desktop as before?" is by content, which
is what `desktop_match::match_desktops` does — scoring every pairing of
recorded and current desktops by windows in common, with the display and
position only as tie-breaks.

The machinery lives in `src/actor/reactor/display_record.rs`:

- **The departure record.** When a display leaves, rift records every display's
  desktops, their trees and which windows were where, from a *pre-churn
  snapshot* taken at the first sign of the change (windows start leaving a
  desktop a few hundred milliseconds before the reconfiguration arrives). It
  records every tracked window's home from that snapshot, tree or no tree: a
  window on a desktop that has never been shown is in no tree, and a record
  built from trees alone cannot move it.
- **The settle.** The survivor gets stand-in desktops for its own that macOS
  destroyed, so its windows have somewhere to go.
- **The return.** Once every recorded display is back, the pass pairs destroyed
  desktops with fresh ones, sends windows home, restores trees, and retires the
  empty desktops macOS minted for the return.
- **Aftercare** (`AFTERCARE`, five seconds). The window server goes on
  reassigning windows after the pass has declared itself done, so the pass
  leaves where each window belongs behind it, and a window that turns up
  elsewhere while the window server is still moving windows for the change is
  sent back once. A window the user moves fails that test and stays.
- **The arrival record.** A display arriving with no record open (after a
  restart while undocked, say) still gets the display that was already here
  recorded, and its desktops, windows and trees put back once macOS has
  finished handing things to the newcomer.
- **Giving up** (`GIVE_UP_ON_DISPLAY`, two minutes). A recorded display the
  window server has stopped listing is dropped so the rest can be put back, and
  its later return is still treated as one. The survivor is never given up on.

Things worth knowing before changing any of it:

- **A single report can carry a departure and a return.** A pseudo display
  appearing and going again across a replug does exactly that.
- **The give-up absence is aged only while display reports keep arriving**,
  because that is where the check runs. A quiet machine with a display unplugged
  never gives up on it. That fell out of the placement rather than being
  designed.
- **Never write a window back mid-change.** macOS moves remembered windows onto
  an arriving display before rift hears of it, and a write made then uses
  geometry that is about to be wrong. rift lays windows out once the change has
  settled, and for ten seconds afterwards checks "already there" against the
  window's live frame rather than its cache.
- **Floating windows are not laid out, so they are rescued instead**
  (`rescue_floats_left_off_screen`). After a display change, a float almost
  entirely off screen is brought back. On a desktop that has changed display
  -- a plug, an unplug, or a Mission Control drag -- any float not wholly on
  screen is, since macOS placed all of them, until the desktop has been shown
  for a few seconds. Hidden desktops are never written to; theirs wait until
  shown.
- **`space_window_list_for_connection` is unbacked under test** — every desktop
  looks empty. Ask per-window `window_space` instead. Live, the list also
  includes the wallpaper window.

## Reading a churn after the fact

`rift execute trace dump <path>` writes the always-on flight recorder
retroactively (see [tracing.md](tracing.md)). The lines worth grepping, in the
err log or the dump:

| Line | Means |
|---|---|
| `Displays back; put everything where the record has it` | the return ran; its `replaced={…}` is the destroyed→fresh pairing. Its absence across a session means a return never completed |
| `A desktop macOS minted for the return holds nothing` | an empty minted desktop is being retired |
| `Window placed by the user while a display is away` | the record was edited. Should be rare; a burst is mis-attribution |
| `giving up on it so the rest can be put back` | a display was dropped from the record |
| `record_members` (Act) | where the record's windows came from, and which tracked windows it does **not** account for |
| `reconcile`, `pass_done`, `record_aftercare`, `record_give_up` (Act) | the return's counts, a pass ending, a window sent home after it, a give-up |
| `display_churn_end` (Act) | rift considers the change over |
