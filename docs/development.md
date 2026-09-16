# Local development

## The loop

```sh
just dev
```

Builds with the `release-fast` profile (~20s, against ~2m for `release`), swaps
the two binaries into the Cellar that Homebrew's launchd service resolves
through `opt/`, re-signs them, and restarts the service.

That is the whole answer to "how do I test a change." Everything else here
exists because one of its steps is easy to get wrong by hand.

### Why swap into the Cellar rather than run from `target/`

rift needs Accessibility, and macOS grants that to a *binary*, not to a project
directory. Running `./target/release/rift` directly means granting Accessibility
to that path too, and then two rift builds are both permitted and it stops being
obvious which one launchd started. Swapping into the path the service already
uses keeps one binary, one grant, one running copy.

### Why signing is not optional

TCC records the Accessibility grant against the binary's *designated
requirement*. An ad-hoc signature (`codesign -s -`) leaves that requirement
pinned to the cdhash, which changes on every single rebuild — so every rebuild
silently loses Accessibility, and rift exits 1 in a launchd respawn loop with
nothing but a line in `/tmp/rift_$USER.err.log` to explain itself.

Signing with a Developer ID gives a stable requirement, and the grant survives
rebuilds. `just` uses `RIFT_CODESIGN_IDENTITY` if set, and otherwise the
identity in the justfile.

Release builds get this for free — CI signs with the hardened runtime and
notarizes (see [releasing.md](releasing.md)) — so this only matters for binaries
you swap in locally.

## Other recipes

| recipe | what it does |
|---|---|
| `just` | list everything |
| `just dev` | the loop above |
| `just install` | same, full `release` profile |
| `just status` | service state, payload health, last errors |
| `just logs` | tail both service logs |
| `just sa` | re-inject the scripting addition (after a Dock restart or reboot) |
| `just check` | what CI runs: fmt, `cargo check --locked`, `cargo test` |
| `just fmt` | format **only** changed files |
| `just release <version>` | see [releasing.md](releasing.md) |

## Display churn

`just churn` connects and disconnects a virtual display at the running rift and
checks its invariants each cycle, so an unplug no longer means reaching behind
the machine. See [display-churn.md](display-churn.md).

## Formatting

The committed tree predates the current nightly rustfmt: `cargo +nightly fmt
--all` rewrites roughly a hundred files nobody touched. `just fmt` formats only
files you have actually changed, which is what you want in every case.

## Gotchas

- **After a reboot or a Dock restart**, the scripting addition is gone from
  Dock's memory even though it is still installed. `just sa`, or let
  `run_on_start` do it. `just status` says which.
- **`brew upgrade`** replaces the swapped binaries with the tap's build. That
  build is signed with the same Developer ID, so Accessibility survives — but it
  is not your working tree. Re-run `just install` if you were testing something.
- **Two window managers at once** do not fail loudly, they fight — windows
  oscillate as each reacts to the other's frame changes. Stop yabai and skhd
  before starting rift, and verify with `launchctl list | grep -iE
  'yabai|skhd|rift'` rather than assuming.
