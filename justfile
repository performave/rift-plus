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
dev: (_build profile) (_install profile) restart _sa-pin-check
    @echo "rift is live"

# Same, but a full optimized build — what a release ships.
install: (_build "release") (_install "release") restart _sa-pin-check

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
# Restart whichever service is actually running rift. When both are loaded —
# the agent installed and `brew services start` run too — the loser respawns
# every ten seconds into the same log; the agent is the one this file keeps.
restart:
    #!/usr/bin/env bash
    set -euo pipefail
    if launchctl print "gui/$UID/git.acsandmann.rift" >/dev/null 2>&1; then
        if launchctl print "gui/$UID/sh.brew.{{formula}}" >/dev/null 2>&1; then
            echo "just: Homebrew's {{formula}} service is loaded beside the agent; stopping it" >&2
            brew services stop {{formula}}
        fi
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

# Re-inject the scripting addition. Needed after a Dock restart or a reboot,
# and after any rebuild that ships a new payload.
#
# Three things can be wrong at once and each hides the next, so they are done
# in order rather than left to be discovered one command at a time: the
# sudoers rule is pinned to a sha256 and every rebuild invalidates it, and a
# running Dock cannot swap payloads in place, so a payload bump needs Dock to
# go first. One password prompt covers the lot.
sa:
    #!/usr/bin/env bash
    set -uo pipefail
    if ! rift sa status 2>&1 | grep -q 'pinned to this binary'; then
        echo "just: re-pinning the passwordless 'sa load' rule to this build"
        sudo rift sa install-sudoers || exit 1
    fi
    out="$(sudo rift sa load 2>&1)"
    printf '%s\n' "$out"
    if printf '%s' "$out" | grep -q 'restart Dock'; then
        echo "just: Dock is holding an older payload and cannot swap it in place; restarting it"
        killall Dock
        for _ in $(seq 1 50); do
            pgrep -x Dock >/dev/null && break
            sleep 0.1
        done
        sudo rift sa load
    fi

# The sudoers rule is pinned to a binary's hash, so every rebuild staleness it.
# Nothing breaks at the swap — the payload already inside Dock keeps working —
# so the cost lands at the next Dock restart, hours later: `sa load` asks for a
# password nobody is there to type, the addition stays out, and rift silently
# loses every desktop and window move across spaces. rift says so at startup,
# but that is a line in a log at the moment you stop looking. Say it here,
# while you are still at the keyboard.
_sa-pin-check:
    #!/usr/bin/env bash
    set -uo pipefail
    status="$(rift sa status 2>&1 || true)"
    printf '%s\n' "$status"
    if printf '%s' "$status" | grep -q 'pinned to this binary'; then
        exit 0
    fi
    cat >&2 <<'    EOF'

    just: the passwordless 'sudo rift sa load' rule is pinned to the previous
          build. Until it is re-pinned, the next Dock restart leaves the
          scripting addition unloaded, and rift cannot move desktops or move
          windows between spaces — an unplug then merges everything and cannot
          put it back.

          Fix now:  just sa
    EOF

logs:
    tail -f "/tmp/rift_${USER}.out.log" "/tmp/rift_${USER}.err.log"

# What CI runs.
check: fmt-check
    cargo check --locked
    cargo test

test:
    cargo test

# Connect and disconnect a virtual display at the running rift, checking its
# invariants each cycle. See docs/display-churn.md.
churn *ARGS:
    ./scripts/display-churn.py {{ARGS}}

# The files the format gates judge: everything in commits you have not pushed
# yet, plus whatever is still in the working tree.
#
# The commit half is the part that matters. CI checks the files a *push*
# touched, so a file stopped being examined here the moment it was committed:
# the working-tree question `git diff HEAD` asks goes empty, and a gate with
# nothing to check exited 0 and read as a pass. `just check` on a clean tree
# was therefore green without having run rustfmt over anything -- which is the
# state every release is cut in, since `just tag` refuses a dirty tree.
# Asking origin/main..HEAD instead puts exactly what the next push will be
# judged on back in view.
#
# Deliberately not the fork's whole diff from upstream. That is 118 files, and
# the unformatted lines in most of them are upstream's own; rewriting those is
# the wholesale reformat AGENTS.md forbids, and it would wreck rebasing.
_fmt-files:
    #!/usr/bin/env bash
    set -euo pipefail
    {
        # `--not --remotes=upstream` matches nothing when upstream has never
        # been fetched, and an empty --not silently widens the range to the
        # whole history -- every file in the repo, the one outcome this must
        # never produce. So both refs have to exist before the range is asked.
        if git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null &&
           [ -n "$(git for-each-ref --count=1 refs/remotes/upstream/)" ]; then
            git log origin/main..HEAD --no-merges --not --remotes=upstream \
                --name-only --pretty=format: -- '*.rs' | sed '/^$/d'
        else
            echo "just: no origin/main or upstream/* locally -- checking the working tree only" >&2
        fi
        git diff --name-only --diff-filter=d HEAD -- '*.rs'
        git ls-files -o --exclude-standard -- '*.rs'
    } | sort -u | while IFS= read -r f; do
        # `git log` still names a file a later commit deleted or renamed away.
        [ -f "$f" ] && printf '%s\n' "$f"
    done

# Format the files you changed and have not pushed (never --all; see AGENTS.md).
#
# --skip-children is what keeps that promise. rustfmt follows `mod`
# declarations, so formatting a module root reformats everything below it:
# touching src/lib.rs reformats the entire crate, which is the wholesale
# reformat AGENTS.md forbids.
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    files="$(just _fmt-files)"
    [ -z "$files" ] && { echo "nothing to format"; exit 0; }
    echo "$files" | xargs rustfmt +nightly --edition 2024 --unstable-features --skip-children
    echo "rustfmt: formatted $(printf '%s\n' "$files" | wc -l | tr -d ' ') file(s)"

fmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    files="$(just _fmt-files)"
    # Said out loud on purpose. An empty set exiting 0 in silence is what let a
    # vacuous run pass for a real one.
    [ -z "$files" ] && { echo "rustfmt: nothing to check"; exit 0; }
    echo "rustfmt: checking $(printf '%s\n' "$files" | wc -l | tr -d ' ') file(s)"
    echo "$files" | xargs rustfmt +nightly --edition 2024 --unstable-features --skip-children --check

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
