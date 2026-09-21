# Syncing with upstream

Written 2026-09-21, against upstream `c345d6a7` (v0.5.10) and fork
`5c97bbe1`. Measurements go stale; the reasoning should not.

## Where the fork sits

```
$ git fetch upstream --tags
$ git rev-list --left-right --count upstream/main...main
61  160
```

Sixty-one upstream commits the fork does not have, a hundred and sixty of its
own. None of the sixty-one carry a `(cherry picked from commit …)` trailer in
this fork's history, so none of them have been taken in.

**Upstream at that commit compiles cleanly.** The note that it does not was
about `v0.5.8`, which is long past.

## Merge, not cherry-pick

The fork's earlier practice was to take upstream in commit by commit, because
the CI format gate would otherwise hand nightly rustfmt every file upstream
touched — the wholesale reformatting [AGENTS.md](../AGENTS.md) exists to
prevent. That reason is gone: the gate in
[`.github/workflows/test.yml`](../.github/workflows/test.yml) now skips merges
(`--no-merges`) and subtracts anything upstream already carries (`--not
--remotes=upstream`), which is exactly the sync-merge case.

And cherry-picks never move the number. A fork that picks upstream's work one
commit at a time stays sixty-one behind for ever, because the commits it has
taken are not the commits upstream has. Only a merge makes `git rev-list` say
zero.

So: merge, and decline the upstream commits this fork does not want by taking
our side inside the resolution rather than by leaving them out of the range.
Write the declines down under "Declined from upstream" in
[CHANGELOG.md](../CHANGELOG.md).

## What it costs

```
$ git merge --no-commit --no-ff upstream/main
25 conflicted files, 53 conflict hunks, 72 files auto-merged
```

Most of it is small. The hunks concentrate where you would expect:

| file | hunks |
| --- | --- |
| `src/actor/reactor.rs` | 12 |
| `src/layout_engine/engine.rs` | 9 |
| `.github/workflows/release.yml` | 4 |
| `src/bin/rift-cli.rs`, `src/actor/reactor/events/outcome.rs` | 3 each |
| eleven more files | 1–2 each |

One thing in the merge is not a conflict at all and is the single largest piece
of work. `2ec98815 perf: combine event taps` **deletes**
`src/actor/event_tap.rs` and `src/actor/gesture_tap.rs`, folding both into a
new `src/actor/input.rs`; git reports them as `UD` (modified by us, deleted by
them). The fork has 428 lines of its own in those two files — edge-drag resize,
the tile-frame hit test, the 8 ms modifier-drag interval.

The saving grace, worth checking again before starting: **upstream made no
logic change to either file before moving them.**

```
$ git diff --stat $(git merge-base main upstream/main) 2ec98815~1 \
    -- src/actor/event_tap.rs src/actor/gesture_tap.rs
(empty)
```

So `input.rs` is a pure reorganisation of the versions the fork branched from,
and the fork's 428 lines re-apply into the corresponding regions rather than
needing a rewrite.

## Reading the sixty-one

Twenty-two `fix`, seventeen `chore`, nine `perf`, nine `feat`, one `ci`, one
revert. Seven have a commit body; the rest are a subject line, and some of
those are `chore: lol`, `chore: brandng` and `fix: avoid redundant layout
[asses`. Read the diffs, not the subjects.

A useful way to order the work is by how far this fork has already moved the
files each upstream commit touches — the risk is not the size of the upstream
change, it is how much of the fork's own work sits in the same place.

**Take, cheaply.** These cherry-pick clean against `main` today, which is a
good proxy for "merges without argument":

- `be3bbeaa fix: memory leaks in ipc`
- `9b4f133f fix(traditional): avoid panic when join collapses the shared parent (#470)`
- `1714f90a fix: windowtitlechanged not firing (#471)` — two lines
- `c33cf4a7 fix: add missing outcome`
- `e9eafba7 fix: pause mouse focus while menu open`
- `dd9ad8f4 perf: menu bar`

**Take, carefully — high value, lands in the fork's most-rewritten code.**

- `05fbe10d fix: release the display-churn gate on an edge, not on every
  snapshot (#503)` is thirty-four lines with the best commit message in the
  range, and it is a bug this fork still has: `releases_display_churn_refresh_quarantine`
  is set on every coherent snapshot and read as a level, so a screenshot
  overlay or a browser dropdown triggers a full inventory refresh and a raise.
  Upstream measured 320 refreshes and 241 raises from one screenshot overlay.
  Its own churn work makes this fork *more* exposed to that, not less.
- `d3be1bc1 perf: bulk window attributes (2-4x faster)` — real, and it touches
  AX paths the fork has opinions about.
- `2ec98815 perf: combine event taps` — see above.
- `52b2846b feat: move-workspace-to-display (#314)`, `1628c1c2 feat: floating
  layout`, `44d7fa11 feat: more display/space scoping` — features, additive,
  but they land in `layout_engine` where the fork has diverged most.

**Look hard before taking.**

- `83f3b666 feat: experimentally ignore axuielementdestroyed` puts the whole
  `AXUIElementDestroyed` handler behind a feature flag that returns early. The
  fork has a more careful fix for the same class of problem in the same place —
  `remove_stale_windows` for apps that never fire window-closed, and
  `is_current_window_element` so a late destroy for a superseded element cannot
  tear down its replacement. Keep the fork's body; the flag is not worth the
  branch.
- `c8d3afa3 chore: lol` strips the comments crediting OmniWM for two pieces of
  reasoning this codebase borrowed. Attribution is cheap to keep.
- `7a59369b` reverts `6baa565a`; take both or neither.

## Order of operations

1. `git fetch upstream --tags`, then merge in a throwaway detached worktree
   (`git worktree add --detach …`) — Eric often has another session in the main
   checkout, and a half-merged `main` there is the worst outcome.
2. Resolve. Take our side wherever the fork has deliberately diverged, and
   note the declines for the changelog as you go.
3. Port the fork's `event_tap.rs`/`gesture_tap.rs` work into `input.rs`.
4. `just check`, then the VM battery in
   [vm-harness-handoff.md](vm-harness-handoff.md) — the merge touches the churn
   and layout paths that battery exists for, and `cargo test` will not see a
   regression there.
5. Version: upstream is `0.5.10`, so the fork becomes `0.5.10-plus.1`. CI
   refuses to release when the tag, `Cargo.toml`, `Cargo.lock` and
   `CHANGELOG.md` disagree.
6. Only then move `main` onto the result.

Expect imported upstream *tests* to assert upstream behaviour this fork
diverges from. The code change is usually still right: retarget the test rather
than dropping it, and confirm the retargeted test fails against a revert of the
fix.
