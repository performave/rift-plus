# Local development for the rift fork. `just` lists these.
#
# The loop that matters is `just dev`: build fast, swap the binaries Homebrew's
# service actually runs, re-sign with the Developer ID, restart. One command,
# because doing it by hand is four and forgetting the signature costs you the
# Accessibility grant (see AGENTS.md).

# The Homebrew formula whose service runs rift. Override for a different tap:
#   just formula=rift dev
formula := "rift-plus"

# Signing identity. Ad-hoc (`-`) will run, but macOS re-prompts for
# Accessibility on every rebuild and rift exits 1 until you do.
identity := env("RIFT_CODESIGN_IDENTITY", "Developer ID Application: Eric Wang (8UR4G77744)")

# The profile `dev` uses: opt-level 2, no LTO, incremental. ~20s vs ~2m.
profile := "release-fast"

_default:
    @just --list --unsorted

# Build, install over the running service's binaries, restart. The iteration loop.
#
# These chain through dependencies rather than nested `just` calls: a nested
# call is a fresh invocation and would not carry a `formula=` override, so
# `just formula=rift dev` would build one thing and install into another.
dev: (_build profile) (_install profile) restart
    @echo "rift is live"

# Same, but a full optimized build — what a release ships.
install: (_build "release") (_install "release") restart

_build profile:
    cargo build --profile {{profile}} --bins

# Swap binaries into the Cellar the launchd service resolves through opt/.
_install profile:
    #!/usr/bin/env bash
    set -euo pipefail
    prefix="$(brew --prefix {{formula}} 2>/dev/null)/bin"
    if [ ! -d "$prefix" ]; then
        echo "just: {{formula}} is not installed via brew; nothing to swap into" >&2
        echo "      install it first, or run with formula=<name>" >&2
        exit 1
    fi
    for bin in rift rift-cli; do
        chmod u+w "$prefix/$bin"
        cp "target/{{profile}}/$bin" "$prefix/$bin"
        codesign --force --sign "{{identity}}" "$prefix/$bin"
        chmod 555 "$prefix/$bin"
    done

# Two services can run rift: the per-user launchd agent that
# `rift service install` writes (`git.acsandmann.rift`), and Homebrew's own.
# Only one of them holds the process, so when the agent is loaded it is the
# rift serving your code — and `brew services restart` then restarts the
# *other* one, which starts, finds rift already up, exits 1, and says nothing.
# The binaries are swapped, the running rift is untouched, and the build just
# made never runs: the silent way `just dev` appears to do nothing at all.
#
# Restart whichever service is actually running rift.
restart:
    #!/usr/bin/env bash
    set -euo pipefail
    if launchctl print "gui/$UID/git.acsandmann.rift" >/dev/null 2>&1; then
        "$(brew --prefix {{formula}})/bin/rift" service restart
    else
        brew services restart {{formula}}
    fi

stop:
    #!/usr/bin/env bash
    set -euo pipefail
    if launchctl print "gui/$UID/git.acsandmann.rift" >/dev/null 2>&1; then
        "$(brew --prefix {{formula}})/bin/rift" service stop
    else
        brew services stop {{formula}}
    fi

# Is everything actually up? Service, payload, and the last errors.
status:
    #!/usr/bin/env bash
    rift status || true
    tail -n 5 "/tmp/rift_${USER}.err.log" 2>/dev/null || true

# Re-inject the scripting addition. Needed after a Dock restart or a reboot.
sa:
    sudo rift sa load

logs:
    tail -f "/tmp/rift_${USER}.out.log" "/tmp/rift_${USER}.err.log"

# What CI runs.
check: fmt-check
    cargo check --locked
    cargo test

test:
    cargo test

# Format only the files you changed (never --all; see AGENTS.md).
#
# --skip-children is what keeps that promise. rustfmt follows `mod`
# declarations, so formatting a module root reformats everything below it:
# touching src/lib.rs reformats the entire crate, which is the wholesale
# reformat AGENTS.md forbids.
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    files=$(git diff --name-only --diff-filter=d HEAD -- '*.rs'; git ls-files -o --exclude-standard -- '*.rs')
    [ -z "$files" ] && { echo "nothing to format"; exit 0; }
    echo "$files" | sort -u | xargs rustfmt +nightly --edition 2024 --unstable-features --skip-children

fmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    files=$(git diff --name-only --diff-filter=d HEAD -- '*.rs'; git ls-files -o --exclude-standard -- '*.rs')
    [ -z "$files" ] && exit 0
    echo "$files" | sort -u | xargs rustfmt +nightly --edition 2024 --unstable-features --skip-children --check

# --------------------------------------------------------------------------
# Releasing. See docs/releasing.md; the workflow does the building.
# --------------------------------------------------------------------------

# Every version this repo accepts, in one place: semver, with the -plus.N
# suffix this fork uses. Rejects a leading `v` so `just bump v1.2.3` fails
# loudly rather than writing "vv1.2.3" into a tag later.
_semver := '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'

_check-semver version:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{version}}" in
        v*) echo "just: pass the bare version, without the leading 'v' (got '{{version}}')" >&2; exit 1 ;;
    esac
    if ! printf '%s' "{{version}}" | grep -Eq '{{_semver}}'; then
        echo "just: '{{version}}' is not valid semver (expected e.g. 0.5.3-plus.2)" >&2
        exit 1
    fi

# The version in Cargo.toml, and the one Cargo.lock records for this package.
_cargo-version:
    @sed -n '/^\[package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1

_lock-version:
    @awk '/^name = "rift-wm"$/ { f = 1; next } f && /^version = / { gsub(/[",]/, "", $3); print $3; exit }' Cargo.lock

# Bump the version in Cargo.toml and Cargo.lock, after confirming.
bump version: (_check-semver version)
    #!/usr/bin/env bash
    set -euo pipefail
    current="$(just _cargo-version)"
    locked="$(just _lock-version)"

    # Retrying a half-finished bump is the common case: Cargo.toml was already
    # written but `cargo` never ran, so the lockfile lagged. Detecting that
    # drift lets the retry fix the lockfile instead of refusing, or
    # double-bumping something already at the target.
    if [ "$current" = "{{version}}" ] && [ "$locked" = "{{version}}" ]; then
        echo "just: already at {{version}}; nothing to do"
        exit 0
    fi
    if [ "$current" != "$locked" ]; then
        echo "just: Cargo.toml ($current) and Cargo.lock ($locked) disagree -- finishing the bump"
    fi

    echo "  Cargo.toml:  $current -> {{version}}"
    echo "  Cargo.lock:  $locked -> {{version}}"
    read -r -p "Bump to {{version}}? [y/N] " reply
    case "$reply" in [yY]*) ;; *) echo "aborted"; exit 1 ;; esac

    /usr/bin/sed -i '' -E "1,/^version = /s|^version = \".*\"|version = \"{{version}}\"|" Cargo.toml
    # cargo rewrites Cargo.lock as a side effect; --offline keeps it from
    # touching anything else.
    cargo check --offline --quiet 2>/dev/null || cargo check --quiet
    echo
    echo "Bumped. Next: write the CHANGELOG.md section, commit, then 'just tag'."

# Tag the current commit from the version in Cargo.toml. Annotated, so
# `git push --follow-tags` carries it.
tag:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(just _cargo-version)"
    just _check-semver "$version"

    locked="$(just _lock-version)"
    if [ "$version" != "$locked" ]; then
        echo "just: Cargo.lock says $locked but Cargo.toml says $version; run 'just bump $version'" >&2
        exit 1
    fi
    if ! grep -q "^## \[$version\]" CHANGELOG.md; then
        echo "just: CHANGELOG.md has no '## [$version]' section -- the release notes come from it" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "just: working tree is dirty; commit before tagging" >&2
        exit 1
    fi
    if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
        echo "just: tag v$version already exists (use 'just untag $version' first)" >&2
        exit 1
    fi

    git tag -a "v$version" -m "v$version"
    echo "Tagged v$version. Push it to start the release:"
    echo "  git push origin main --follow-tags"

# Delete a tag locally and on the remote, and offer to clear its release.
untag version: (_check-semver version)
    #!/usr/bin/env bash
    set -euo pipefail
    echo "This deletes tag v{{version}} locally and on origin."
    read -r -p "Delete v{{version}}? [y/N] " reply
    case "$reply" in [yY]*) ;; *) echo "aborted"; exit 1 ;; esac

    git tag -d "v{{version}}" 2>/dev/null || echo "  (no local tag)"
    git push origin ":refs/tags/v{{version}}" 2>/dev/null || echo "  (no remote tag)"

    # A release left behind still points at the deleted tag. The release action
    # upserts, so leaving it is safe if you are about to re-tag the same
    # version -- which is why this asks instead of assuming.
    if command -v gh >/dev/null && gh release view "v{{version}}" >/dev/null 2>&1; then
        echo
        gh release view "v{{version}}" --json isDraft,publishedAt,url \
            --template 'release: {{{{.url}}}} (draft: {{{{.isDraft}}}})'
        echo
        echo "A release exists for v{{version}}. Deleting it is optional:"
        echo "the release workflow upserts, so re-tagging will reuse it."
        read -r -p "Delete the release too? [y/N] " reply
        case "$reply" in
            [yY]*) gh release delete "v{{version}}" --yes && echo "deleted" ;;
            *) echo "left in place" ;;
        esac
    fi
