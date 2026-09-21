# Changelog

All notable changes to this fork are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
with the fork scheme described in [AGENTS.md](AGENTS.md): `<upstream>-plus.<n>`.

Entries describe this fork's changes relative to
[upstream rift](https://github.com/acsandmann/rift), not upstream's own history.

## [Unreleased]

### Added

- **`mouse.edge_resize`.** Drag the boundary between two tiled windows to
  resize them, with no modifier held — what every other window manager does and
  what a stock rift does not, where resizing means reaching for a floating
  window's edge. The drag moves the boundary, so both neighbours resize
  together, and it changes the split ratio rather than writing a frame. Only
  interior boundaries are grabbed: the outer rim of the layout has nothing to
  trade space with, so an app's own edge-resize there is left alone.
  `mouse.edge_grab_px` sets how close the pointer has to be (default 8).

- **`ui.drop_overlay.tint`.** The wash of colour over the drop region was a
  fixed system blue at 0.28 alpha, and that alpha is most of what reads as
  frost: it is laid *over* the Liquid Glass material rather than through it, so
  no amount of `clear_style` gets past it. It is now `[r, g, b, a]` in the
  config, left out for the blue it always was. Pair a lower alpha with
  `clear_style = true` for a pane you see through rather than a tinted slab.

### Fixed

- **A press no longer panics the input thread when `mouse.edge_resize` is on.**
  The edge hit test read the mouse settings off the tap's own state cell, which
  the mouse-down handler already holds mutably, so the first click killed the
  input thread with "RefCell already mutably borrowed". The settings are passed
  in now.

- **Modifier-drag resize is smooth rather than stepped.** Updates were rate
  limited to one every 68ms — yabai's interval — which is a visible 15Hz
  staircase. The limit was not arbitrary: each update lays out the workspace,
  the reactor's channel is unbounded, and a rate it could not drain grew a
  queue until the window trailed the cursor and went on moving after the button
  came up. The reactor now collapses a run of drag updates into its last
  sample, which is lossless because each one carries the movement since the
  press rather than since the previous update. An update it cannot keep up with
  is dropped instead of queued, so the gesture clocks itself and the interval
  could come down to 8ms.

- **A modifier-drag resize no longer animates.** The arrange it requested was
  not marked as a resize, so with `layout.animate` on, every update started a
  fresh animation that the next update replaced — the window never reached the
  frame it was given and permanently lagged the cursor.

- **The space switch animation survives a Dock restart.** The animation lives
  inside Dock, because that is where the addition's payload lives, so anything
  that restarts Dock takes the setting with it — and a Dock *crash* is silent:
  Dock is back in a blink and the only sign is that the swipe has gone back to
  its own timing. The same gap swallowed the setting at every boot, where rift
  sent it immediately after kicking off `run_on_start` and so, since those
  commands run on their own threads, usually before `sudo rift sa load` had put
  a payload in Dock to receive it. rift now supervises the addition: a
  handshake every couple of seconds, and when the payload it knew is gone it
  asks for the addition back — running the user's own `run_on_start` line, with
  `sudo -n` so it can never wait at a prompt — and replays the settings the old
  payload was holding. Startup is the same path, which is why nothing has to
  assume `run_on_start` finished first. rift will not load an addition that
  `sa uninstall` removed, and with no `sa load` line to run it says so once
  rather than each time it looks.

- **Two app rules may share an `ax_role` again.** Validation treated a repeated
  `ax_role`, `ax_subrole`, `app_name`, `title_regex` or `title_substring` as a
  duplicate rule wherever it appeared, so the documented shape — an
  app-specific rule above a catch-all, both naming `AXWindow` — made the config
  invalid. Validation gates every reload, so the effect went well past a
  warning: `rift execute config set`, edits to `config.toml` and the reload
  `sudo rift sa load` asks for were all rejected, silently, leaving rift
  running on whatever it started with. A rule is now a duplicate only when its
  whole matcher repeats, which is the only case where the later rule can never
  match. A rejected config change also says so in the log, and the two lines
  that announced success before validation had run — `Updated <key> to <value>`
  and `Config reloaded successfully` — now wait until there is something to
  announce. `sudo rift sa load` likewise no longer reports that rift re-applied
  its settings when rift refused the reload.

- **The drop overlay no longer stops showing for the rest of the session.**
  The overlay's panel is built once and kept, and it was ordered on screen only
  on the edge of a `visible` flag kept beside it — correct exactly as long as
  nothing but rift's own `hide` ever takes the panel off screen. When something
  else did, the flag stayed true, the edge never came round again, and the
  overlay was gone for good while every other part of the drag — the target,
  the drop zones, the swap itself — went on working, which is what made it read
  as random. Only a config reload brought it back, because a reload is the one
  thing that throws the panel away. Both overlays now ask the panel whether it
  is on screen instead of remembering, so any frame can put it back, and the
  first one that has to says so in the log. The tile halo carried the same
  latch and is fixed with it.

- **A deactivated space no longer answers every layout command with silence.**
  `toggle_space_activated` hands a macOS space back to macOS, and until now the
  only trace of that was one `WARN` line per command in the log: the reactor
  saw no active space, returned no change, and the CLI reported "Command
  executed successfully". On a float-by-default configuration, where the tile
  key is the only thing that ever tiles a window, a space left deactivated —
  by an accidental keypress, or carried onto a fresh space id by display churn
  — presents as two or three particular apps that "refuse to be tiled", since
  the apps that happen to live on that desktop are the only ones affected.
  Layout commands on an unmanaged space now fail with a message naming the
  cause and the way back, so `rift execute ...` reports it and the log says it
  once rather than once per keypress.

- **A layout command aimed at an unmanaged window says so too.** The sibling
  of the above, and the one that fires while the space is perfectly healthy:
  a command targeting the focused window is dropped when that window is in
  neither the tiling tree nor the floating set, which is the state a window
  falls into after `WindowFocused ignored: ... not in active layout`. It was
  an `INFO` line; it now fails the command with the window in front and the
  engine's idea of focus named, which is the pair that identifies the stale
  focus behind it.

- **Space activation changes are logged.** Nothing recorded when a space was
  deactivated or why, which left no way to tell an accidental toggle from a
  disabled marker carried across a space id churn after a display came and
  went. Both paths now log at `INFO`.

## [0.5.5-plus.3] - 2026-09-18

### Added

- **`only_first_window`, for apps whose other windows are documents.** An app
  rule with `floating = false` tiles every window its app opens, and most apps
  open more than one kind. Outlook's drafts and message windows, a browser's
  Library and Page Info: each one split the desktop the moment it appeared. The
  usual answer is a `title_regex` exception, and it does not hold. Those windows
  report the same `ax_role` and `ax_subrole` as the main window, carry no size
  constraints to tell them apart, and are titled with whatever the user typed —
  an Outlook draft is `<Subject> • <account>` against a main window of
  `<Folder> • <account>`, two arbitrary words in the same position. Every
  pattern that works is one the app can invalidate by existing, so the list
  only ever grows.

  The difference that does hold is ordinality: the window worth tiling is the
  one the app opens for itself, and everything after it was spawned from that
  one. `only_first_window = true` applies a rule only while the app has no
  tiled window, leaving later windows to fall through to whatever comes next —
  a float-by-default catch-all, in the configuration this is for.

  It asks whether the app has a *tiled* window rather than whether one has been
  seen, which is what makes it survive the cases a counter would not. Close
  every window and reopen the app and it tiles again, because at that moment
  nothing of the app's is tiled. A splash screen cannot take the slot either:
  Outlook's is an `AXUnknown` window that fails admission, so it is never
  tiled, and a rule that counts appearances would have lost the slot to it on
  every launch.

  It is app-wide rather than per-desktop, because a draft opened from a desktop
  the main window is not on is still a draft; the cost is that an app cannot be
  auto-tiled on two desktops at once, which is the trade the key exists to
  make. It does not count toward rule specificity — it narrows *when* a rule
  applies, not which window it describes — and a rule setting it with no
  matcher is still ignored for having no matcher.

- **A focus ring confirms that the float toggle landed.**
  `[settings.ui.tile_halo]`, off by default. Under a float-by-default config
  the float toggle is the only thing that pulls a window into the tree, and it
  is also the one command whose effect can be entirely invisible. A window the
  user has already sized by hand frequently lands on a frame it was practically
  sitting on, so nothing moves; a window that joins a stack covers the windows
  already there exactly, so nothing moves *and* the result is actively
  misleading. Either way the key reads as broken when it worked.

  A ring in the accent colour the user chose in System Settings now springs
  onto the window's new frame, holds, and fades — inward when the window joins
  the tree, outward and in a neutral grey when it leaves, so the direction is
  legible from the motion without reading the colour. A stacked landing draws a
  fainter rim for each window sharing the stack, up to two, because the depth
  is the only thing on screen that distinguishes it from an ordinary tile.

  The ring is drawn *inside* the window's own edge rather than around it. The
  inner gap between two tiled windows is a handful of points, so a ring outside
  the frame lands on the neighbour, and two windows tiled in a row would flash
  rings into each other. The spring's overshoot is what makes the arrival read
  as a snap, and it carries the ring briefly past the frame — inward, where
  there is room. Nested rims take their corner radius from the outer one less
  their inset, so the curves stay concentric; macOS 26 ships that rule as
  `NSViewCornerRadius.containerConcentric`, and it is one subtraction here.

  Deliberately not gated on whether arrange actually wrote any frames, unlike
  the mouse warp it sits beside: a toggle that moves nothing writes nothing and
  reports no change, and that is the case the ring exists for. It respects
  Reduce Motion by dropping the travel and fading in place. Unlike the drop
  overlay it uses no `NSGlassEffectView` and so has no macOS 26 floor — glass
  is a filled shape, and an outline of it would need four bars merged by an
  `NSGlassEffectContainerView` on a rule documented only as "sufficiently
  similar". Its timer runs only while a flash is on screen.

### Changed

- **`space create` and `space destroy` go by the pointer.**
  `settings.space_target`, `"pointer"` by default; `"focus"` restores the old
  behaviour. Both commands used to act on whatever `CGSGetActiveSpace` named,
  which is the desktop of the display that owns the menu bar. Switching the
  *other* display to an empty desktop moves neither, because an empty desktop
  has no window that could take the focus — so a destroy aimed at the empty
  desktop in front of you took the desktop you were working in, windows and
  all. The workaround was to click the empty desktop first.

  The pointer is the signal that survives that, and it agrees with the intent
  in both directions: you reach a desktop on another display either by
  gesturing on that display or through a rift command that warps the pointer
  there, and when you do mean the desktop holding the focused window, the
  pointer is almost always on that display too, because that is where you were
  working. Emptiness is deliberately not the rule — the choice is between
  *this* desktop and *that* one, and keying it on whether a desktop happens to
  be empty would be wrong precisely when it mattered, and unpredictable in the
  meantime. The pointer falls back to the focused desktop when it is over no
  display rift manages, so a pointer parked off-screen changes nothing.

  Not fixed by moving focus with the switch instead: that is what
  `follow_space_switch_across_displays` does, and loosening its gate is what
  once carried the user away from an app they had just activated. The
  addition's destroy takes an arbitrary desktop id, so nothing here needed the
  target to be focused in the first place.

- **Layout state is keyed by workspace rather than by native macOS space.** The
  space in that key was redundant: workspace ids come from one slot map shared
  by every space, so they identify a workspace on their own. It was also a key
  macOS owns and re-mints — it destroys a desktop at an unplug and mints a fresh
  one at the replug — so every layout had to be carried by hand from the dead id
  onto the new one, and a carry that missed left a tree stranded under an id
  nothing pointed at any more. That is how a stacked desktop came back tiled.
  Keyed by the workspace there is nothing to carry, and one of the two
  remap paths is gone outright. Layout files written by earlier versions are
  migrated when they load.

### Fixed

- **A destroyed desktop now finds its replacement by its windows, not by
  list order.** macOS destroys a desktop on unplug and mints a fresh one on
  replug, and the two had to be paired for the tree to go back. They were
  paired by zipping two lists — the desktops that vanished against the ones
  that appeared, in the order macOS reports them — which is right only when
  macOS lists a replacement where the old one stood. When it does not, a
  desktop's tree lands on the desktop next to the one holding its windows and
  every window follows it there.

  A destroyed desktop cannot be identified, because it no longer exists, so
  the only thing that can speak for it is what outlives it: the windows that
  were on it. Window server ids survive a churn, an unplug and a restart of
  rift; desktop ids survive none of them. Each destroyed desktop now takes the
  fresh one holding most of its windows, best match first. Order remains the
  tie-break and the answer when the windows cannot speak — an empty desktop
  has nothing to match on — so a churn that moved nothing pairs as it always
  did.

- **A display that never comes back no longer holds up the whole return.**
  The return runs only once every recorded display is back, which is what
  makes it one coherent diff rather than a series of guesses; the cost was
  that a display which never returned wedged it for good. A laptop screen
  opened out of clamshell joins the record, and shutting the lid again leaves
  the record waiting on a display macOS has switched off — so nothing was ever
  put back, and rift settled the survivor over and over instead while macOS
  pulled the same windows off the desktop it kept putting them on.

  A recorded display the window server has stopped listing *at all* — not
  merely showing nothing, which is what a display mid-churn does — is given up
  on after two minutes. Its desktops are forgotten the way a desktop destroyed
  while a display is away already was, its windows are filed wherever they
  next turn up, and the displays that did come back reconcile without it. The
  survivor is never given up on.

  Two minutes because giving up on a display that does come back costs its
  trees: the return has nothing to pair it with and restores nothing, so the
  windows are not lost but nothing puts them back either. At twenty seconds
  that happened to a monitor unplugged for half a minute while the desktop was
  still in use, which is an ordinary thing to do. The absence is also only
  aged while reports keep arriving, so a machine sitting quiet with a display
  unplugged never gives up on it however long it stays away.

- **The window server's own shuffling is no longer mistaken for the user
  moving a window.** A window that turned up on another desktop while a
  display was away was taken as deliberate, and where it landed became where
  it belonged from then on. The test for "is the churn over?" measured from
  rift's last sighting of a reshuffle, ten seconds; the window server goes on
  moving windows far past that, and over a long absence — a monitor switched
  off for a minute, a lid opened and closed — it crossed the threshold easily.
  A desktop's worth of windows would then be filed where macOS had dumped
  them, and every later pass faithfully put them there: two displays' contents
  swapped over, with nothing in the layout engine wrong.

  It now also asks the window server how long ago it last moved windows for a
  reconfiguration, which the archive already treats as the only honest witness
  for the same question about desktops. Erring long costs a placement made
  just after a churn, and that window goes back where the record has it; erring
  short corrupts the record, which is not recoverable without moving everything
  back by hand.

- **A replug no longer swaps two tiles round.** Two windows sharing a desktop
  could come back from an unplug in the other order. When a display change
  hands the layout engine a snapshot whose windows have all gone, the restore
  matches nothing and every live window is re-projected instead — and they
  were walked in `WindowId` order, which is process id then AX index. That has
  nothing to do with where the user put them, so the tree came back sorted by
  which app happened to launch first. The order the windows were in before the
  trees are replaced is now the order they go back in, and a window that was
  not on the desktop still sorts by id, after the ones that were.

- **A replug no longer strands a desktop's windows on the stand-in made for
  them.** macOS destroys a desktop on unplug and mints a fresh one on replug,
  and rift pairs the two so the windows it parked on a stand-in desktop go
  home and the stand-in is destroyed. The pairing turns on which desktops the
  window server had listed before the displays came back — and a single report
  can carry a departure and a return at once, as it does when a pseudo display
  appears and goes again across a replug. Settling for that departure filed the
  return's own fresh desktops among the ones seen while a display was away, so
  the return read them as desktops the user had made: nothing was paired, the
  stand-in became permanent, and macOS's new desktop was left empty beside it.
  A settle is now skipped once every recorded display is back on screen, which
  is the return's business anyway.

- **An empty desktop macOS minted for a replug is now cleaned up.** It makes
  them freely across a reconfiguration and never takes them away again, so they
  accumulated, and a display churned often enough collected one after another.
  One that no destroyed desktop accounts for and that holds no window rift
  knows of is now retired the same way the stand-ins are — destroyed once no
  display is showing it, and left alone if a window turns up on it after all.

  Whose desktop it is turns on when it first appeared. The record notes every
  desktop the window server lists and whether it was still moving windows for
  a reconfiguration at the time, which is the one account of that worth
  trusting: macOS mints its desktops mid-reshuffle, when the reports are least
  coherent, and a desktop the user makes appears while the window server is
  quiet. Only the first sighting counts, so a desktop of the user's that is
  still listed through a later churn stays theirs, empty or not.

- **A screen snapshot with no desktop on it no longer reports no active
  desktop.** Resolving the command space and the menu-bar space both ended at
  the desktops on the incoming screens, so a snapshot that arrived mid-churn
  with none of them carrying a desktop yet answered "no active desktop" rather
  than holding the last one that had them. Both now fall back to the previous
  screens, which is what the test path had been doing all along — these two
  resolvers ran different logic under `cfg(test)` than in a release build, so
  the behaviour that shipped was the one nothing covered.

- **A modifier-drag resize now moves the boundary, not just the window.**
  Alt-dragging a tile's edge mostly did nothing, and when it did take, it left
  a gap between the two windows. Every frame rift writes during such a drag is
  followed a few milliseconds later by the app's own move/resize notification
  carrying the frame the window had *before* the write — unrequested, with the
  button still down. Rift read each of those as the user resizing the window by
  hand and rolled the split ratio back a step. The dragged window was written
  to the pointer again on the next update while its neighbour stayed behind, so
  the boundary between them opened; a drag short enough to end on a rollback,
  or whose last notification landed after the button was up, was undone
  entirely.

  Reports that reach the layout while rift is driving a resize from the
  pointer — and for a moment after the release, since the notifications trail
  the writes — are now read as what they are. Only rift moves tiles during a
  modifier drag, so there is nothing else they can be. Floats are untouched:
  their own geometry path is what records where a modifier-drag move left
  them. Which edge an update moves is also read against the frame rift last
  asked for rather than the one the app last reported, so a lagging report can
  no longer make both edges look like they moved and send the layout after the
  boundary the user was not dragging.

- **Reloading the config no longer throws away the layout mode you switched
  to.** Hot reload re-applies `virtual_workspaces.workspace_rules` to
  workspaces that already exist, so that editing a rule takes effect without a
  restart. But the mode it compared against fell through to the global
  `layout.mode` whenever no rule named the workspace, and a fallback is
  indistinguishable from an instruction once it reaches the comparison: every
  reload put every unruled workspace back to the default. `set_workspace_layout`
  and `toggle_workspace_layout` are runtime commands, so a desktop switched to
  `stack` by hand reverted to `bsp` the next time anything reloaded the config
  — including a reload prompted by an edit to an unrelated key, and including
  `Reload Config` in the menu bar.

  Only an explicit rule re-applies now. A workspace no rule names keeps the
  mode it has; `layout.mode` goes back to being what a workspace is *born*
  with, which is all it ever claimed to be. Configs that do set
  `workspace_rules` are unaffected, and the existing test for that path still
  covers it.

- **`preserve_focus_per_workspace` does something.** The key was declared,
  documented in `rift.default.toml` and defaulted to `true`, but nothing in the
  codebase ever read it: arriving on a workspace always returned focus to the
  window last used there, whatever the config said. It is a real switch now.
  Turning it off falls through to the workspace's own selection instead; the
  last-used window is still recorded either way, so turning it back on resumes
  where it left off. The key could not simply be deleted — the settings block
  is `deny_unknown_fields`, so removing it would reject every config that sets
  it, the shipped default among them. Anyone who set it to `false` expecting
  nothing will now get the behaviour they asked for.

- **Opening an app from the Dock no longer lands you on the desktop's
  previous window.** Clicking the Dock tile of an app whose window lives on
  another desktop makes macOS switch desktop by itself. rift follows a switch
  it sees on a display the pointer is not on, carrying focus to the window
  last used there — right for a switch you asked for, wrong here, because the
  activation had already decided who should be focused. It was meant to stand
  down for exactly this case: the check asks whether the key window is on
  either desktop and leaves the switch to macOS if it is. But the window
  server names the new key window the moment the app comes forward, while
  that window's own record only arrives with the next inventory, and in the
  gap rift cannot say which desktop it is on. An unplaceable key window read
  the same as no key window at all, so the guard passed and rift went. The
  pointer offers no second line of defence, because a Dock on a screen edge
  sits outside the display frame the guard tests. Recorded three times in one
  session, the margin between the desktop change and the window's record
  ranging from 32ms to 137ms, which is why it came and went. A key window
  rift cannot place now holds the switch rather than releasing it.

- **A departing display's windows get a desktop of their own, instead of
  landing on somebody else's layout.** macOS does not carry over the desktop a
  departing display was showing: it destroys that one and merges its windows
  into whatever the survivor is showing, while the display's other desktops
  migrate with their ids intact. Those windows are stranded exactly as the
  survivor's own are when the traffic goes the other way — but the settle
  looked for destroyed desktops only among the survivor's, found none, and
  left them where they landed. Close the lid with an external display
  attached and the laptop's windows piled onto whatever was on the external
  screen, on top of its tiling. It compounded from there: a stranded window
  drifts, and a drift seen more than ten seconds after the reshuffle is
  recorded as the user putting it there, so the record's memory of where the
  window belongs was overwritten with wherever it had wandered to, and the
  replug put it back in the wrong place. The settle now considers every
  recorded display's desktops, and a destroyed one with windows to rescue
  gets a desktop made for it, standing where it stood in its own display's
  order. A desktop that went empty still gets nothing — that is the first
  thing the window server reaps. Two things that hid the case are fixed with
  it: the list of what is still on screen was taken from rift's own display
  map, which is updated only *after* the settle runs and so still named the
  departing display's desktops as present.

- **A desktop macOS reaps no longer throws the record away.** macOS 27 garbage
  collects desktops of its own accord in the wake of a display change, seconds
  after the event announcing it has been handled and rift's churn flag
  cleared — including the desktop rift had just made to hold the merged
  windows, 3.6s after making it. rift read that as the user destroying a
  desktop, which means forgetting it, the desktop it stood in for, and every
  window filed on either. So the record was discarded moments after it was
  taken and there was nothing left to put back when the display returned.
  Whether a desktop went with a reshuffle or by the user's hand is now asked
  of the window server itself, which keeps the time it last moved windows for
  a display change, rather than inferred from rift's own event handling.

- **A desktop that was stacked comes back stacked after a reboot.** The saved
  layout is only restored if it is fresh, on the reasoning that after a reboot
  or an afternoon away the windows have moved on and putting them back would
  fight the user. That is true of the windows and of nothing else: which
  layout a desktop is in, and what workspaces it has, is a setting the user
  chose, as true after a reboot as before one. Restoring nothing at all when
  the snapshot aged out meant every reboot silently reset every desktop to the
  default layout — a 20-minute gap was enough, and the two-minute window makes
  one certain. The age now bounds putting the *windows* back; past it the
  desktops still come back in the layouts they were in, empty, and whatever is
  opened next tiles into them.

- **The scripting addition works on macOS 27.** The payload's version gate knew
  Tahoe and nothing after it, so on 27 it returned before a single symbol was
  looked up, and the handshake reported dock.spaces, the desktop picture
  manager and add, remove and move space all missing at once. Five at once
  reads like a Dock that changed everything; it was only the gate. Dock's
  internals are very nearly the same: every byte pattern still matches, and
  only the offsets the search starts from moved — far enough that Tahoe's
  hints now sit *past* their match for all but dock.spaces, and the search
  runs forward from its hint, never back. 27 now carries its own offsets and
  shares Tahoe's patterns, so sending a window to a desktop, creating and
  destroying desktops, and the teleporting space switch all work again.

  The two routines rift patches rather than calls needed reading again, and
  both turned out to be near-identical. The space-switch step gained a single
  instruction -- a `mov x8, #0x7fefffffffffffff` between the call and the
  `ldr d2`, for a magnitude check Tahoe did not make -- while its prologue,
  its tail and the distance between them are unchanged, so 27 reuses Tahoe's
  resume delta. `setFrontWindow` did not change at all: the pattern began at
  the `cbz w1` that returns on a zero window id, whose first byte carries the
  low bits of its own branch distance, and 27 branches further. That byte is
  now left open, and the prologue behind it still narrows to one match.

- **The layout is saved again after a desktop migration.** Layout state is now
  keyed by the workspace rather than by the native space, and the call that
  used to re-key it across a migration went with the change — but that call did
  two jobs. It carried a desktop's layout onto its new id, which is genuinely
  no longer needed, and it also dropped the layout state of the workspaces the
  migration deletes to make room, which still is. Left behind, that state sat
  under a workspace id that no longer resolved, and a save validates the whole
  file: every autosave from then on failed, once a minute, and `layout.ron`
  silently stopped being written. Since the migration runs at startup whenever
  a display comes up on a different desktop id than it was last seen on, a
  single unplug could cost an entire session's layout, with nothing to show for
  it but a warning in the log. The migration now drops that state with the
  workspaces it deletes.

- **A settling display change no longer builds a window a tile on a desktop it
  was never on.** The window server goes on moving windows between desktops for
  a second or two after a display change is done, and rift's trees follow it one
  desktop at a time, so which desktop holds a window answers differently from
  one millisecond to the next. The bookkeeping that remembers where a window
  sat before native fullscreen re-read that answer while it was still moving and
  kept the newest one, walking a window's remembered slot off its own desktop,
  onto the departed display's, and from there onto whichever surviving desktop
  sorted first by id. The restore then put a tile there for a window that was
  somewhere else entirely, and the window itself stopped taking focus on the
  desktop it was really on. The reading from before the shuffle started is now
  the one that is kept.

- **A window the window server had already moved no longer loses its desktop.**
  When macOS destroys a desktop and mints a fresh one, it puts windows on the
  new desktop before it tells rift the new desktop replaced the old. rift had
  already given that fresh desktop default workspaces and assigned those
  windows to them, so the migration deleted the workspaces — and struck out the
  assignments with them, leaving the windows on no desktop at all. The restore
  that follows a display's return then had nothing to match its saved tree
  against: the windows came back unmatched and were discarded from the layout.
  They now move to the migrated workspace holding the same position, keeping
  both the desktop and which of its workspaces they were on. The window store's
  own re-pointing had the same shape of bug and merged nothing — it overwrote
  the destination, dropping whatever the window server had put there — and now
  merges.

- **Windows no longer fall out of their layout when a display returns.** A
  display change makes every application busy at once, and a busy application
  answers rift's window enumeration with `kAXErrorCannotComplete`. That failure
  was the one case the refresh queue dropped outright: the successful-but-stale
  replies were queued again, the failed ones were forgotten. rift's idea of
  that application's windows then stayed as it was, and the restore that
  follows a display's return matched each saved tree against windows it had
  lost track of — they went unmatched, and unmatched candidates are discarded,
  so they left the layout for good. In one replug here that cost seven windows
  across two desktops, one of which matched nothing at all. A failed refresh is
  now queued again, to be asked at the sweep after the churn settles rather
  than in the same breath, since an application too busy to answer this instant
  is still too busy the next.

- **A desktop whose tree cannot be put back no longer comes back tiled.** The
  record taken at a display's departure keeps each desktop's workspaces' layout
  modes, but only ever compared them, to tell a desktop the user rearranged
  while away from one they did not. Putting a mode back was left to the tree
  restore, which carries the mode along with the tree — so whenever that
  restore failed, a stacked desktop came back on the default mode, and the
  failure was logged at `debug`, which the shipped log level drops. The modes
  the record holds are now put back on their own when the tree cannot be, and
  a desktop that needed it says so at `warn` instead of vanishing into a log
  level nobody runs.

- **Replugging a display no longer piles every desktop onto one screen.** With
  `displaced_windows = "spaces"`, rift records where each desktop lives when a
  display departs and puts them back when it returns. The two halves of that
  record come from different places — SkyLight lists each display's desktops,
  the display list says which displays are on screen — and an unplug parts
  them: SkyLight hands the departing display's desktops to the survivor a
  moment before the display list has lost the display. Caught in that moment,
  the record had the departing display owning a desktop *and* the survivor
  owning the same one, because a display SkyLight had already dropped fell
  back to being credited with whatever it was still showing. The return then
  did as it was told and dragged the departing display's own desktop to the
  survivor, taking every desktop filed behind it along — six desktops stacked
  on the laptop, and the monitor that had just come back left with a single
  empty one. A snapshot showing a display a desktop that SkyLight has already
  filed under a different display is now read as the reshuffle it is, and the
  record waits for one where the two agree; a desktop claimed by both a
  departing display and one that stays is the departing display's, since the
  survivor cannot have gained a desktop in the instant the other left.
- **A modal no longer knocks the window it covers out of the layout.** Open a
  JetBrains modal — Push Commits on Cmd+Shift+K, Confirm Exit — and the IDE
  re-reports its own document window as an `AXDialog` for as long as the modal
  is up. rift believed it: the same window, same id, still a resizable root
  `AXWindow`, was retired from its tree, everything tiled beside it reflowed to
  fill the gap, and only some later inventory — seconds away — put it back. A
  window rift has already admitted now keeps its place when a report still
  describes a root `AXWindow`; that report's identity is dropped whole rather
  than in half, since it also blanks the app id the layout is matched by. A
  promotion still counts, and a window that has genuinely changed shape is
  still retired.
- **A desktop macOS replaces during sleep keeps its layout.** With both
  displays still attached across a sleep, macOS can destroy the desktop a
  display is showing and put a new one in its place. The remap that carries
  the old desktop's layout onto its replacement only ran when the display
  topology changed, which a sleep does not do, so the new desktop arrived
  with the default layout — a stacked space came back tiled — and the layout
  it should have inherited was left orphaned in `layout.ron`. That remap now
  runs on an unchanged topology too, from any snapshot that lists the
  display's own desktops; that list is what tells a replaced desktop apart
  from one you merely switched away from, so a snapshot arriving without it
  still stands aside.
- **Destroying a desktop while a display is away no longer comes back to
  haunt the next wake.** With `displaced_windows = "spaces"`, rift makes a
  stand-in desktop for the one macOS destroys when a display goes. Destroy
  that stand-in yourself (`destroy_space`, Mission Control) and the record
  taken at departure still expected it: every later settle — the one after a
  wake included — made another, pulled the windows you had moved elsewhere
  onto it, switched to it and reordered the desktops around it. A desktop
  gone from the list with the same displays on screen and no reshuffle under
  way is now forgotten, together with the desktop it stood in for and the
  windows filed on either, which are filed again wherever they next turn up.
- **`rift status` notices when two launchd jobs are loaded for rift.** The
  agent `rift service install` writes and Homebrew's `brew services` job can
  both be loaded; one holds the process and the other starts, finds rift
  running, exits, and is respawned every ten seconds into the same log.
  `rift status` now recognises Homebrew's current `sh.brew.*` label, reports
  the extra job as degraded and names the command that removes it, and `just
  restart` stops Homebrew's job when the agent is the one it keeps.
- **Leaving native fullscreen puts the window back in its slot even when the
  window server orders it in last.** Coming out of a fullscreen video in Zen,
  the window server ordered the window out, moved it home, moved the display
  home, and only then ordered it back in. The inventory taken in between left
  the window out, the space change that would have re-tiled it had come and
  gone, and the window sat floating over the layout until the next switch to
  that space. The order-in now puts a window with a fullscreen slot waiting
  back where it was.

## [0.5.5-plus.2] - 2026-09-06

Against upstream `v0.5.5`.

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

[Unreleased]: https://github.com/performave/rift-plus/compare/v0.5.5-plus.3...HEAD
[0.5.5-plus.3]: https://github.com/performave/rift-plus/compare/v0.5.5-plus.2...v0.5.5-plus.3
[0.5.5-plus.2]: https://github.com/performave/rift-plus/compare/v0.5.5-plus.1...v0.5.5-plus.2
[0.5.5-plus.1]: https://github.com/performave/rift-plus/compare/v0.5.3-plus.1...v0.5.5-plus.1
[0.5.3-plus.1]: https://github.com/performave/rift-plus/compare/v0.5.3...v0.5.3-plus.1
