# Handoff: display churn, 2026-09-17

Commits `c00680c`, `f33b6e0`, `4ab2e07`, `d0a4722`, `a1d9324` on `main`.

Started from one report — two monitors unplugged and replugged, ChatGPT and
Outlook came back swapped, and a pile of empty desktops. Six defects, all in
the path that reconciles a display change, all found from the flight recorder
and the err log rather than by reading code.

## What was wrong, in one sentence

The reconciliation reasons in `SpaceId`, which macOS destroys and reissues at
every reconfiguration, so every question it asks is really "is this the same
desktop as before?" — and it answers that in about a dozen independent places,
each with its own failure mode.

## Fixes

| Symptom | Root cause | Fix | Where |
|---|---|---|---|
| Two tiles come back swapped | A restore whose snapshot has gone stale matches nothing and re-projects every live window in `WindowId` order — pid then AX index — so the tree comes back sorted by which app launched first | Capture each target space's window order before the trees are replaced; unmatched windows go back in that order, new ones still sort by id behind them | `persistence/restore.rs::apply` |
| The stand-in desktop becomes permanent and macOS's new one sits empty beside it | A settle ran for a departure arriving in the *same report* as a return (a pseudo display across a replug does this), filing the return's own fresh desktops into `record.seen`, so the return read them as the user's and paired nothing | Skip the settle once every recorded display is back on screen | `display_record.rs::settle_after_departure` |
| Empty desktops accumulate, one per unplug | Nothing retired the desktops macOS mints across a reconfiguration | Retire an unclaimed desktop holding no window rift knows of, through the same path as rift's own stand-ins. Whose desktop it is turns on when it was *first* listed and whether the window server was still moving windows then (`met` / `minted`) | `display_record.rs`, `display_archive.rs::note_display_set` |
| Two displays' contents swap over | `note_window_placed_while_away` took the window server's own shuffling for user intent: its "is the churn over?" test measured from rift's last sighting, ten seconds, and the window server moves windows far past that | Also ask the window server how long ago *it* last moved windows (`since_windows_last_moved`), which the archive already trusts for the same question about desktops | `display_record.rs`, `PLACEMENT_AFTER_CHURN` |
| A desktop's tree lands next to the desktop holding its windows | Destroyed and fresh desktops were paired by zipping two lists in macOS's report order | Pair by windows in common, best match first; order is the tie-break and the answer for an empty desktop, so a churn that moved nothing pairs as it always did | `display_record.rs::pair_by_windows` |
| Nothing is ever put back; rift settles the survivor over and over | The return waits for *every* recorded display, and a lid opened out of clamshell joins the record then goes for good | Give up on a recorded display the window server has stopped listing at all after two minutes; forget its desktops, reconcile the rest. Never the survivor | `display_record.rs::give_up_on_displays_gone_for_good` |

Nine tests added. Four (`pair_by_windows`) are direct; the rest were each run
against the unfixed code to confirm they fail.

## What is validated, and what is not

`just churn` drives a **virtual** display that macOS treats as a genuine
hotplug, and BetterDisplay was already installed — hardware is not needed for
ordinary churn. 30 cycles clean, no window loss, no desktop accumulation, and
the return completes every cycle (`Displays back ... replaced={316: 319}`).

Not validated:

- **The clamshell case.** `docs/display-churn.md` says a virtual display will
  not reproduce the pseudo-display a lid close reports, and that is exactly the
  sequence that wedged the return. The give-up path is unit-tested and has run
  live for an ordinary unplug; it has never run for a lid. **Try this first
  when the monitors are back.**
- **Content matching deciding anything.** Every probe desktop in those 30
  cycles was empty, so `pair_by_windows` fell back to order every time. The
  unit tests cover it; hardware has not.
- Space renumbering after sleep (`wake-space-renumber-loses-layout`). Gated on
  topology change, which sleep does not trigger; the virtual display cannot
  reproduce it either.

## Not done

- **The `DesktopId` re-key.** 1,434 `SpaceId` uses across 47 files, a
  `layout.ron` v3→v4 migration, and the IPC surface. See below.
- **The startup restore flipping tile order.** Seen once at 19:59:16 on
  2026-09-17, immediately after a redeploy, with no `display_record` activity
  since 19:55:30 — so it is the startup path reading `layout.ron`, not the
  churn path. May not be a defect at all: autosave may have captured a wrong
  order and faithfully restored it. Never established which.
- **One empty desktop migrated** from the probe to the survivor over 20 cycles,
  when the probe went away for good. Probably correct — the desktop moved to
  the remaining display — but unexamined. Against roughly one leaked desktop
  per cycle before.

## Where this should go

A desktop's identity cannot come from macOS. It destroys desktops and mints new
ones with new ids, and a destroyed desktop cannot be identified because it no
longer exists — no id scheme survives that, a space UUID no better than an id.
The durable anchor is the **windows**: window server ids survived every cycle of
a whole session here, an unplug, a clamshell flap and a restart of rift.
`pair_by_windows` is that principle applied in one place; the rest of the
subsystem still guesses.

The shape that would end the class of bug:

1. **rift owns identity** — a `DesktopId` it assigns and persists, with
   `SpaceId` demoted to the current address.
2. **Re-bind by content**, since that is the only durable anchor. This is not a
   second mechanism bolted on; it is what the id has to point at.
3. **One scored matching function** on every topology change, in place of
   `subst` / `paired` / `stand_in` / `stopgaps` / `seen` / `met` / `minted` and
   six timers, all of which are partial answers to the same question.
4. **Idempotent, no transaction.** macOS dribbles a return across several
   reports; re-run the matching on each until it converges instead of trying to
   catch the right one. Most timers disappear, and a display that leaves for
   good stops being a special case.

Note that `VirtualWorkspaceId` is **not** this id — `workspaces_by_space:
HashMap<SpaceId, Vec<VirtualWorkspaceId>>`, so a desktop holds many workspaces
and a workspace is a sub-unit of one. A config with one workspace per desktop
makes the two look interchangeable; they are not.

The audit gate this needs is now in the tree: two live recordings under
`tests/traces`, replaying with no violations.

## Gotchas worth keeping

- **`record.seen` is written only inside `settle_after_departure`.** Its doc
  comment claims every desktop listed since the record was taken; an ordinary
  report does not update it, so a desktop the user makes while a display is
  away is usually absent from it. `met` / `minted` exist because widening
  `seen` would change which desktops the return pairs.
- **A single report can carry a departure and a return.** A pseudo display
  appearing and going again across a replug does exactly that.
- **`space_window_list_for_connection` is unbacked under test** — every desktop
  looks empty. Ask per-window `window_space` instead, which the fixtures do
  back. Live, the list also includes the wallpaper window (id 84 here), so it
  is never truly empty.
- **The give-up absence is only aged while reports keep arriving**, because
  that is where the check runs. A machine sitting quiet with a display
  unplugged never gives up on it however long it stays away. That fell out of
  the placement rather than being designed; know it before moving the call.
- **Every `just dev` re-breaks the sudoers pin.** `just sa` after each one.

## Reproducing

```bash
just churn --dry-run            # invariants only, changes nothing
just churn --cycles 20          # 20 hotplug cycles, invariants after each
rift execute trace dump /tmp/x.txt   # the always-on ring, retroactively
```

The log lines worth grepping in `/tmp/rift_$USER.err.log`:

| Line | Means |
|---|---|
| `Displays back` | the return ran. Its **absence** across a session is the clamshell wedge |
| `replaced={...}` | destroyed→fresh pairing. Empty when nothing paired |
| `matched=0, unmatched=N` | a restore that matched nothing — used to be the tile-swap signature |
| `Window placed by the user while a display is away` | the record was edited. Should be rare; a burst of them is the mis-attribution |
| `minted for the return holds nothing` | the reaping fired |
| `giving up on it so the rest can be put back` | a display was dropped from the record |
