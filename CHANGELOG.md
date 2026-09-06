# Changelog

All notable changes to this fork are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
with the fork scheme described in [AGENTS.md](AGENTS.md): `<upstream>-plus.<n>`.

Entries describe this fork's changes relative to
[upstream rift](https://github.com/acsandmann/rift), not upstream's own history.

## [Unreleased]

### Fixed

- **Switching to a space the display holding it already shows now takes you
  to that display.** With every desktop but one on an external monitor, the
  space number that names the laptop's only desktop had nothing to switch:
  the laptop was already showing it, so rift returned without a switch, macOS
  activated nothing, and neither focus nor the pointer left the external
  display — the key did nothing at all. Rift now finishes such a command the
  way a switch that did change a display ends, by focusing the window last
  used on that space, or its desktop and the middle of the display when the
  space is empty. `move-window-to-space --follow` follows the window across
  the same way.

## [0.5.5-plus.1] - 2026-09-06

Against upstream `v0.5.5`. The previous release was already built on it and
named against `v0.5.3` by mistake; the base number is corrected here.

### Added

- **rift can say which version it is.** `rift --version` prints the build,
  `rift query metrics` carries the version of the rift that is running, and
  `rift status` names it in the window manager row — adding a note to restart
  when it is not the version of the binary asking, which is the state every
  upgrade leaves behind until the service restarts.
- **rift notices when the passwordless `sa load` rule no longer matches it.**
  The sudoers rule is pinned to the digest of the binary that installed it, so
  every upgrade, rebuild or move silently turns `sudo rift sa load` into a
  password prompt that launchd cannot answer, and the scripting addition is
  simply gone after the next Dock restart. `rift sa status` now reports the
  rule on a second line, and rift warns in its log at startup when
  `run_on_start` loads the addition through sudo and the rule is missing or
  pinned to another build. Both name the fix: `sudo rift sa install-sudoers`.
- **`rift sa uninstall` removes the sudoers rule along with the bundle**:
  everything `rift sa` leaves outside Homebrew's prefix, which
  `brew uninstall` cannot reach. Documented under "Uninstalling" in
  `docs/scripting-addition.md`.

### Fixed

- **An unplug no longer stops being put right after a display that is not
  coming back.** In `displaced_windows = "spaces"` mode the record taken when
  a display departs is reconciled once every display it waits for is back.
  Every later departure used to add its display to that list — a monitor at
  another desk that came and went, or the stand-in display macOS reports
  while a lid is closing, which is never a screen — and since the return
  needed all of them back at once, one such display held the record for
  good: from then on every unplug left the survivor's windows merged in
  among the visitors' with no desktop of their own, and every replug
  restored nothing. The record now waits only for the displays that were
  there when it was taken; a display that arrives and leaves meanwhile is
  neither recorded nor waited for.
- **The survivor gets a desktop for its merged windows at every departure,
  not only the first.** Each unplug costs the display that stays its first
  desktop — the one rift made at the previous unplug included — and its
  windows are merged into the leaving display's desktop. Only the first
  unplug used to be settled; the survivor is now settled again whenever a
  departure destroys one of its desktops.
- **A record taken at wake is taken from before the lid closed.** Closing the
  lid on an external display reshuffles the desktops before the Mac sleeps,
  and the display's departure is only seen on waking, minutes later. The
  layout snapshot from before the reshuffle used to expire after ten seconds,
  and the display list used to be read from the half-reported state, so the
  record had the already-merged trees and no display it could settle. The
  snapshot and the last whole display set are now held across the
  reshuffle until the displays are reported whole again.
- **A layout mode switched while a display was away survives the return.**
  The return puts each desktop's tree back the way it was at departure unless
  the user rearranged it meanwhile, and only a change of window order counted
  as rearranging: stacking or tiling a desktop keeps the order, so the return
  quietly put the old mode back. A changed mode now counts too.
- **An unplug the Mac sleeps through right after is finished at wake.** Closing
  the lid straight after unplugging leaves Dock asleep before it has carried
  out what rift asked of it — the desktop for the merged windows, the moves
  onto it. The first whole display report after a wake now checks the
  outcome and does again whatever was not done: a window that never reached
  its made desktop is sent again, a desktop never made is made.
- **A desktop rift made at an unplug is only destroyed once its windows have
  somewhere to go.** When the returning display brings no fresh desktop for
  the survivor, the made one is its desktop now and stays, windows and all.
- **`mouse_follows_focus` follows a cmd-tab that comes right after a click.**
  A focus change within half a second of a mouse release is taken to be the
  click's own doing — into a window, on another display's menu bar, to dismiss
  a popover — and the pointer is left where the user put it. That grace also
  swallowed a cmd-tab (or cmd-`, or a hotkey) pressed straight after a click,
  such as selecting text with a triple-click and switching apps to paste it:
  focus moved and the pointer stayed behind. A key pressed after the release
  now marks the change as the keyboard's, and the pointer follows. Clicks
  alone behave as before.

## [0.5.3-plus.1] - 2026-09-03

First tagged release of the fork, against upstream `v0.5.3`.

### Added

- **The trackpad space switch can run on your own timing.** After the fingers
  lift from a swipe between macOS spaces, Dock finishes the slide with a
  velocity spring of its own, and nothing in it is a duration to change. With
  the scripting addition loaded, a new setting replaces that spring with a
  fixed duration and a curve, while Dock keeps tracking the fingers, rendering,
  and committing the switch itself:

  ```toml
  [settings.space_switch_animation]
  enabled = true
  duration_ms = 200
  easing = "ease-out"   # linear | ease | ease-in | ease-out | ease-in-out |
                        # apple-default, or a cubic bezier: [0.4, 0, 0.2, 1]
  ```

  The payload hooks Dock's step routine for the animation (found by pattern;
  macOS 26 on Apple silicon for now) and reports it as the `space switch step`
  attribute. Nothing is patched until the setting is on, and turning it off
  puts Dock's original instructions back. rift sends the setting at startup
  and on every config reload, so after a `sudo rift sa load` while rift is
  running, reload the config once. `OSAX_VERSION` is `1.3.0`.
- **`rift` now accepts every `rift-cli` subcommand**: `rift query windows`,
  `rift execute …` and `rift subscribe …` work exactly as their `rift-cli`
  spellings do, so the one binary covers running the window manager, managing
  the service and the scripting addition, and driving a running instance.
  `rift-cli` keeps working unchanged — it is a second entry point into the same
  code, not a second implementation.
- **`rift status`** reports whether the window manager is running, whether
  launchd is keeping it alive, and whether the scripting addition inside Dock is
  loaded and healthy — each probed separately so the output says which one to
  fix. It round-trips a real query rather than only looking the Mach service up,
  so "running but not answering" is distinguishable from "not running". `--json`
  for scripts; the exit status follows the window manager alone, since rift runs
  without the scripting addition.

- **The layout survives a restart of rift.** rift could always write its layout
  and start from a written one, but only when told to by hand: nothing saved on
  the way out and nothing read on the way in, so every crash, `brew services
  restart` or rebuild dropped back to the app rules — under a float-by-default
  config, everything to be re-tiled. It now saves on SIGTERM and on a heartbeat
  (a crash and a `kill -9` reach neither the handler nor a manual save), and
  reads the file back at startup.

  The restore is deliberately conditional. A snapshot is only worth putting back
  if rift is coming straight back up; after a reboot or an afternoon away the
  windows have moved on without it, and reasserting a stale arrangement would
  fight the user rather than help. `max_age_secs` bounds how old a snapshot may
  be, measured from when it was written — which with the heartbeat running is
  the last moment rift was known to be alive, so the age is the length of the
  gap. `rift --restore` still restores by hand, ignoring the age.

  Restoring also records the snapshot's tiled/floating verdict as the user's own
  choice, the same standing a manual toggle has. Without that the restore held
  only until the next space activation re-ran the app rules and a catch-all
  `floating` rule floated everything back. Off by default; see
  `[settings.layout_restore]` in `rift.default.toml`.

- **A scripting addition of rift's own.** rift builds, installs and injects its
  own payload into Dock (`/Library/ScriptingAdditions/rift.osax`, serving
  `/tmp/rift-sa_$USER.socket`), so moving a window to a space, creating a space
  and destroying one no longer depend on yabai being installed. New commands:
  `rift sa status | load | install | uninstall | install-sudoers |
  uninstall-sudoers`. See [docs/scripting-addition.md](docs/scripting-addition.md).
- **Display layout restore.** A display's layout is remembered when it
  disconnects — unplug, sleep, lid close — and restored when the same display
  returns, including fullscreen windows. `displaced_windows` chooses whether a
  departed display's windows float over the survivor or join its tree.
- **Drag improvements.** Dropping on a window's edge splits it rather than only
  swapping; cross-display drops preview and perform the split; a drop overlay
  drawn in Liquid Glass shows where a dragged window will land.
- **Space commands.** Switch to a space by number instantly, move windows
  between spaces, create and destroy spaces, and toggle layout modes.
- **Layout commands.** Cycle through a stack and balance the tree; `rotate` and
  `mirror`; column/row ordering in the query API.
- **Modifier-drag.** Hold a modifier and drag anywhere in a window to move or
  resize it; resizing a tiled window adjusts its split ratios.
- **`manage = true`** lets nominally unmanageable windows into the layout, and
  a catch-all rule can make floating the default.
- **An always-on flight recorder** (`sys::trace`) capturing activity from every
  thread, with a replay harness for reproducing reported sequences.

### Deprecated

- **`rift-cli` is deprecated** in favour of `rift`, which now takes the same
  subcommands. It still works and still ships; the documentation and the
  `run_on_start` examples in `rift.default.toml` now say `rift`. Run by hand it
  prints a deprecation notice, and only then — the notice is suppressed unless
  stderr is a terminal, so `run_on_start`, `subscribe cli` and hotkeys stay
  silent.

### Fixed

- **`rift sa load` re-applies rift's settings to the fresh payload.** rift
  applies the addition-backed settings, the trackpad space switch animation
  above all, when it starts; a payload loaded afterwards, after a Dock restart
  or crash, had none of them until rift was restarted too. Loading the addition
  now asks the running rift to reload its config, which re-sends them.
- **The scripting addition no longer takes Dock down over a destroyed
  desktop.** Asked to move or focus a space the window server had already
  forgotten — one destroyed in a display reshuffle but still listed by the
  snapshot in hand — the payload compared a NULL display id and crashed Dock,
  and the addition was gone until reloaded. The payload now treats a space
  without a display as nothing to do, refuses to destroy the desktop a display
  is showing, and rift asks the window server for a display's current desktops
  before moving any.
- **No ghost tiles on the laptop after a replug.** A window on a desktop that
  is not being shown can have its frame reported on another display once the
  displays are back, and that report was read as a cross-space move: the
  window was pulled into the visible desktop's tree beside the windows really
  there, and the tiles split to make room, until discovery put it back a
  moment later. A frame report for a window the window server has on a
  hidden desktop no longer changes its desktop.
- **A display that comes back showing a different desktop keeps its layout.**
  When a returning display showed a desktop other than the one it left on, the
  space it left on was treated as replaced and its whole layout remapped onto
  the shown desktop, leaving the original with no tree at all; the desktop had
  only been switched, and every restore of that display's layout then failed
  with a workspace-count mismatch. A remap now happens only when the old
  desktop is actually gone.
- **No more flicker after a Mission Control drag between displays.** Frame
  writes queue on the app's thread, behind animations and slow accessibility
  calls, and a stale one aimed at the display a window had just left made macOS
  hand the window back, whose tree wrote it again — the two displays traded the
  window dozens of times a second. A frame write that a newer one for the same
  window has superseded is now dropped unapplied.
- **A dragged tile snaps back on the drop.** The drop's arrange skipped a
  target that matched a write still pending for the window, and the write that
  had put it in that very slot usually was, so a dragged tile stayed where it
  was dropped until a later discovery sweep happened to move it. Taking hold
  of a window now clears what was pending for it.
- **No more ghost tiles.** A BSP leaf the tree's window index had lost — after
  a window identity was replaced onto a window that already had a leaf, or an
  insert overwrote the entry — stayed in the tree, rendered and given its
  share of the screen, with nothing able to remove it. Worse, a ghost left in
  another display's tree had its frame written on that display, which made
  macOS hand the window over, whose own tree wrote it back — a flicker loop of
  dozens of moves a second after a Mission Control drag between displays.
  Inserting, replacing or removing a window now retires every leaf it has,
  indexed or not.

- **`mouse_follows_focus` follows a space switch onto the other display.**
  `switch-to-space` aimed at a space of the other display, or
  `move-window-to-space --follow`, switched that display and left the pointer
  and the key window behind on this one: macOS activates a window on the new
  space only when the switch is on the active display, and that activation is
  the focus change the pointer follows. rift now finishes such a switch
  itself — the window last used on the new space is focused, and the pointer
  goes to it as with any focus; an empty space gets its desktop focused and
  the pointer in the middle of the display. A switch on the display the
  pointer or the key window is already on is left to macOS as before.
- **The layout is actually saved, so it is actually restored.** The restart
  restore never fired: on any machine with a desktop rift had listed but never
  shown since starting — a spare desktop on the other display, say — every
  heartbeat save, the save on SIGTERM and `save layout` all failed with
  "workspace … has no layout state", the file on disk only aged, and the
  next start declined it as older than `max_age_secs`. The save checked the
  live engine against the loader's rules, which reject a desktop without
  layout state; the in-memory snapshot the display archive takes already
  prunes such desktops from a copy, and the file now gets the same treatment.
  A pruned desktop is laid out afresh on exposure, as it would have been
  anyway.
- **A window that goes native fullscreen and comes back lands where it was**,
  not beside whatever is selected, and without passing through the wrong place
  on the way. A browser tiled left of an editor came back on the right after a
  video was fullscreened and closed — or, in a layout of stacked columns, on top
  of the editor. rift already remembered the window's slot on the way out, but
  the transition itself defeated it at every step: the slot was read from a
  workspace assignment macOS clears first, so usually nothing was recorded;
  when something was, it was recorded on a later removal, after the transition
  had already shoved the window across; the exit handler and a transient
  "not admitted" removal each threw the slot away; and the window was put back
  into the tree by whichever of several paths noticed first, none of which
  consulted it. Now the slot is captured at the first removal with the space
  read off the tree, survives until the window is really gone, and is
  reinstated by whatever event actually re-inserts the window — before that
  event writes a frame. The whole layout is restored as it was; the "beside
  its old neighbour" fallback is used only when the snapshot matches nothing,
  since splitting a stacked neighbour puts the window into the stack.
- **Switching spaces away from a fullscreen game no longer flies to the leftmost
  desktop.** With `space_switch_method = "auto"`, rift asked the scripting
  addition to switch and then polled the window server for up to 40ms to
  confirm it had. With Roblox fullscreen on either display that first read
  could stall past the deadline while the switch landed anyway, so rift judged
  it a miss and posted the gesture fallback on top: one synthetic swipe per
  step from the *old* space, which from a fullscreen space at the end of the
  list meant several swipes left, ending on the first desktop and
  rubber-banding at the edge. From a normal desktop the same stall overshot by
  one space. The payload now answers a space-focus command with whether it
  issued the switch, and the gesture runs only on a refusal — the readback,
  its deadline, and the miss counter and 30s cooldown that came with them are
  gone. `OSAX_VERSION` is `1.1.0`; run `rift sa load` after updating.
- **The Dock comes back when a scripting-addition switch leaves a fullscreen
  space, and hides when one enters it.** Dock decides its own visibility from a
  controller that tracks which space the bar is on, and only Dock's own switch
  transition told that controller about a new space. The addition switches
  through the window server directly, so the controller kept the old answer:
  from a fullscreen space to a desktop the Dock stayed hidden, and the other
  way it stayed up over the fullscreen app. The payload now hands the new
  space to that controller after every switch, which is the same call Dock's
  own space-change listener makes. Switches still teleport; nothing falls
  back to the swipe. `OSAX_VERSION` is `1.2.0`; run `sudo rift sa load` after
  updating.

- **The drop overlay no longer promises a move a stack cannot make.** Dragging
  a window on a space in stack mode drew a screen-sized drop region for the
  length of the drag, and releasing it swapped the dragged window with an
  arbitrary member of the stack — a change nothing on screen reflects, since a
  stack hands every window the same rect and shows one at a time. Windows in
  one stack are no longer offered to each other as drop targets, so the drag
  shows nothing and does nothing. The same holds for a stacked container inside
  the traditional layout; drops between windows that really do occupy different
  places are untouched.
- `rift-cli service …` printed "service commands have been moved to the `rift`
  binary" and exited 0 without doing anything, so a script could not tell the
  difference between starting the service and not starting it. The subcommands
  work again.
- `rift service install` wrote a plist pointing at whichever `rift` came first
  on `$PATH` rather than the one being run, so `rift service start` from a dev
  build would install and restart Homebrew's rift instead.
- `rift status`'s launchd check looks for the Homebrew labels as well as rift's
  own, because `brew services` starts rift under `homebrew.mxcl.rift`, not the
  `git.acsandmann.rift` that `rift service` manages. Checking only the latter
  reports a perfectly healthy Homebrew install as "not installed".
- `just fmt` reformatted the whole crate whenever a change touched a module
  root such as `src/lib.rs`: rustfmt follows `mod` declarations, so naming one
  file pulled in every file below it — the wholesale reformat the recipe exists
  to prevent. It and the CI check now pass `--skip-children`.
- `rift-cli execute` reported `Command executed successfully` for every
  command, including the three that need the scripting addition and do nothing
  without it. They now print why they could not run and exit non-zero.
- `just dev` / `just install` ignored a `formula=` override, because they
  chained through nested `just` calls rather than dependencies — building one
  thing and installing into another.
- **A window taken into native fullscreen came back floating** — Discord
  fullscreening a video, or anything else that moves a window to a space of its
  own. From the space it left, such a window looks exactly like one that closed:
  it is no longer ordered in there. Rift retired it on that evidence alone,
  which destroyed its record and with it the manual float/tile choice that
  outranks a matching app rule, so the window returned as a stranger for the
  rules to place again. Departure now has to be corroborated by the window
  server having forgotten the id, which a window sitting in a fullscreen space
  has not.
- **A window came back from native fullscreen in the wrong slot** — tiled on
  the left, back on the right. The window server announces the transition
  twice, as a departure from the window's own space and an arrival on the
  fullscreen one, in either order. Rift only recorded the window's slot once it
  had seen the arrival, so whenever the departure landed first the slot was read
  after the window had already left its tree: no anchor and a snapshot that no
  longer held it, leaving it to come back beside whatever was selected. The slot
  is now taken from whichever removal still has one to take. A single tiled
  window on its space could not show this; two could.

- Focus follows the window a Dock click summons, rather than leaving the pointer
  behind.
- Focus resolves across all visible spaces for same-app windows, and is never
  remapped onto an untracked or unadmitted sibling.
- Floating windows stay where the user puts them across drags and seam drops.
- `mouse_follows_focus` no longer fights the pointer during a drag.
- The drop overlay checks for `NSGlassEffectView` before using it, so a
  system older than macOS 26 loses the overlay rather than the process.

### Changed

- **A departed display's windows now stay on their own desktops, and
  everything is put back in one pass when it returns.** The new default for
  `displaced_windows`, `"spaces"`, records where every window and every
  desktop was the moment a display leaves. The desktops macOS carries over
  from an unplugged main display are shown on the survivor as they are; the
  survivor's own windows, which macOS merges into one of them, get a desktop
  made for them with their layout, the survivor's own desktops are put first,
  and the survivor is switched back to the desktop it was showing. Beyond
  that nothing is done while the display is away, deliberately. The window
  server reshuffles
  desktops and windows on its own at both ends of an unplug and not in one
  consistent way — it destroys the survivor's first desktop and merges its
  windows into a visitor's, mints a fresh desktop on replug and sometimes puts
  those windows back on it itself, sends every desktop it filed behind the
  visitors along with them, and can dump a kept desktop's windows elsewhere
  while handing that desktop back — so reacting to each of those as it is
  noticed was a losing game, and every desktop operation asked of Dock during
  the reshuffle was a chance to take Dock down. On replug, once the topology
  is quiet, the record is diffed against what the window server reports and
  the differences are put right: destroyed desktops are paired with the fresh
  ones that replaced them, strayed desktops go back to their display in order,
  strayed windows go back to their desktops, each desktop's tree is restored
  by name, and the made desktop is destroyed once nothing shows it. Windows
  and desktops the user moved or made in the
  meantime, by any means including Mission Control, stay as they were left,
  and so does a desktop whose windows the user rearranged: that desktop keeps
  the arrangement it was given, scaled to its own screen, instead of the tree
  from before the unplug. For the times that is not wanted — a desktop
  shuffled to show something during a short unplug — the new
  `restore_departure_layout` command (`rift execute space restore-departure`,
  bindable like any other) puts the active desktop's layout back the way it
  was at the last departure. The old default, `"float"`, which moved the departed display's windows onto
  the surviving display's own desktop as floats, is still available, as is
  `"tile"`. Set `displaced_windows = "float"` to keep the previous behaviour.
- **`WindowServerAppeared` and `WindowServerDestroyed` are recorded in traces.**
  They were `#[serde(skip)]`, so `rift execute trace dump` silently omitted
  them — and since a window's arrival on and departure from a space is where
  native fullscreen is decided, every bug in that area was invisible to the one
  tool meant to explain it.

- Parity with yabai for directional focus, cross-display moves and space
  creation.
- The release profile ships unstripped, so crash reports symbolicate.

[Unreleased]: https://github.com/performave/rift-plus/compare/v0.5.5-plus.1...HEAD
[0.5.5-plus.1]: https://github.com/performave/rift-plus/compare/v0.5.3-plus.1...v0.5.5-plus.1
[0.5.3-plus.1]: https://github.com/performave/rift-plus/compare/v0.5.3...v0.5.3-plus.1
