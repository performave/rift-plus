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
