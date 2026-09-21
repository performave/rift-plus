# AGENTS.md

Guidance for AI agents and new contributors working in this repository.

## What this repo is

A fork of [acsandmann/rift](https://github.com/acsandmann/rift), a
tiling window manager for macOS. Rust; one binary (`rift`) plus a CLI
(`rift-cli`), distributed as signed and notarized releases through
[performave/homebrew-tap](https://github.com/performave/homebrew-tap) as
`rift-plus`.

### Remotes

- `origin` → `performave/rift-plus` (this fork; push here)
- `upstream` → `acsandmann/rift` (pull new releases and tags from here)

```bash
git fetch upstream --tags
```

Taking upstream's work in is a **merge**, not a rebase — this fork is a hundred
and sixty commits ahead, and rebasing would replay every one of them onto a
tree that has moved underneath. **[docs/upstream-sync.md](docs/upstream-sync.md)**
is the playbook: what it costs, what is worth taking, and what to decline.

## Conventions

- **[Conventional Commits](https://www.conventionalcommits.org/)** for every
  commit: `<type>(<scope>): <summary>`.
- **[Keep a Changelog](https://keepachangelog.com/en/1.1.0/)** in
  `CHANGELOG.md`. Add user-visible changes under `## [Unreleased]` as you make
  them. Entries describe this fork relative to *upstream*. The release notes are
  extracted from this file verbatim.
- **[Semantic Versioning](https://semver.org/spec/v2.0.0.html)**, with the fork
  suffix `<upstream>-plus.<n>` (e.g. `0.5.3-plus.1`). `Cargo.toml` is the source
  of truth; the tag is `v` + that string, and CI refuses to release when the
  tag, `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md` disagree.
- **Documentation lives in [`docs/`](docs/).** Keep `README.md` lean — what rift
  is, how to install it, and links onward. Put anything longer in `docs/` and
  link to it.
- Match the surrounding code style. Comments explain *why*, in prose, at the
  density of the code around them.
- `just fmt` formats only the files you changed. **Never `cargo fmt --all`** —
  the tree inherited from upstream does not satisfy current nightly rustfmt, and
  reformatting it wholesale destroys rebasing onto upstream. CI checks only
  changed files, for the same reason.

Full detail in [CONTRIBUTING.md](CONTRIBUTING.md).

## Local development

`just` lists every recipe; **[docs/development.md](docs/development.md)** is the
guide.

- `just dev` — the loop: fast build, swap the binaries Homebrew's service runs,
  re-sign, restart. ~20s.
- `just check` — what CI runs.

**Signing is not optional locally.** macOS records the Accessibility grant
against a binary's designated requirement, so an ad-hoc signature pins it to a
cdhash that every rebuild changes: rift then silently loses Accessibility and
respawns in a launchd loop. `just` signs with the Developer ID.

## Releases

Pushing a `v*` tag runs `.github/workflows/release.yml`, which builds universal,
Developer ID signs with the hardened runtime, notarizes, and opens a **draft**
release with notes taken from `CHANGELOG.md`. Publishing that draft fires
`tap.yml`, which bumps the formula.

- Cutting one: **[docs/releasing.md](docs/releasing.md)** (`just bump`, `just
  tag`, `just untag`)
- One-time secrets: **[docs/ci-setup.md](docs/ci-setup.md)**

## Architecture (orientation, not exhaustive)

- `src/bin/rift.rs` — entry point and actor wiring. With no subcommand it runs
  the window manager; the subcommands are one-shots that exit without starting
  it.
- `src/cli.rs` — the client command tree (`query`, `execute`, `subscribe`) and
  its dispatch. Both binaries parse and run it, so `rift query windows` and
  `rift-cli query windows` are the same code. `src/bin/rift-cli.rs` is a thin
  second entry point, deprecated and kept only so existing scripts and installs
  keep working — document `rift` everywhere.
- `src/actor/` — the actors: `reactor` is the core, with `app`, `spaces`,
  `drag_swap`, `wm_controller` and friends around it.
- `src/layout_engine/` — the tree and the layout systems (bsp, stack, scrolling,
  traditional, master-stack).
- `src/model/` — window/space state that outlives a single event.
- `src/sys/` — the macOS edge: accessibility, SkyLight, events, screens.
- `crates/rift-protocol`, `crates/rift-client` — the IPC surface, for the CLI
  and for anything else that wants to drive rift.

### The scripting addition

The one subsystem with machine-wide side effects. `src/osax/` is vendored from
yabai (MIT — see `src/osax/LICENSE-yabai`) and compiled by `build.rs` into a
payload injected into `Dock.app`; `src/sys/osax.rs` installs and injects it, and
`src/sys/scripting_addition.rs` is the client.

Read **[docs/scripting-addition.md](docs/scripting-addition.md)** before
touching any of it. Constraints that are easy to break:

- It requires the user to partially disable SIP and set the
  `-arm64e_preview_abi` boot-arg. No code or signing change removes that.
- `OSAX_VERSION` is defined in both `src/osax/common.h` and `src/sys/osax.rs`; a
  test fails if they drift.
- **The injected loader and payload must only ever be ad-hoc signed.** They run
  inside Dock. Hardened-runtime signing the main `rift` binary is fine and is
  what release builds do — never extend it to those two.
- Keep `src/osax/` close to upstream yabai so their fixes stay easy to pull in.
