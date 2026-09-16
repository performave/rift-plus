#!/usr/bin/env python3
"""Drive real display churn at a running rift and check its invariants.

A display appearing and disappearing is the event rift gets wrong most often,
and reproducing it has meant reaching behind the machine. This connects and
disconnects a *virtual* display instead, which macOS treats as a genuine
hotplug: it allocates a desktop for the new display, renumbers, moves windows,
and fires the same reconfiguration callbacks a cable does. The real displays
are never touched, so the machine cannot be left without a screen.

    scripts/display-churn.py --dry-run     # check invariants, change nothing
    scripts/display-churn.py --cycles 20   # 20 connect/disconnect cycles
    scripts/display-churn.py --teardown    # also remove the probe device

Needs BetterDisplay running and its CLI on PATH:

    brew install --cask betterdisplay
    brew install waydabber/betterdisplay/betterdisplaycli

The probe device is created once with a fixed identity and reused, because
macOS writes a permanent root-owned ICC profile per display identity and
nothing ever removes them. On a violation the offending snapshot is printed
and the flight recorder is dumped.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
import time

PROBE = "rift-churn-probe"
# Fixed, so every run reuses one display identity instead of littering
# /Library/ColorSync/Profiles/Displays with a new root-owned profile each time.
PROBE_VENDOR, PROBE_MODEL, PROBE_SERIAL = 8888, 1, 1


class Violation(Exception):
    pass


def say(message: str) -> None:
    """Everything goes to stdout, flushed. Splitting progress across stdout
    and stderr lets a pipe reorder them, which loses failures above the
    window of whatever is reading — exactly when an unattended run needs
    them most."""
    print(message, flush=True)


def run(cmd: list[str], check: bool = True) -> str:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if check and proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed: {proc.stderr.strip() or proc.stdout.strip()}")
    return proc.stdout


def displays() -> list[dict]:
    return json.loads(run(["rift", "query", "displays"]))


def window_count() -> int:
    return len(json.loads(run(["rift", "query", "windows"])))


def probe_exists() -> bool:
    return PROBE in run(["betterdisplaycli", "get", "--identifiers"], check=False)


def ensure_probe() -> None:
    if probe_exists():
        return
    run([
        "betterdisplaycli", "create",
        "-type=VirtualScreen",
        f"-virtualScreenName={PROBE}",
        f"-virtualScreenVendorNumber={PROBE_VENDOR}",
        f"-virtualScreenModelNumber={PROBE_MODEL}",
        f"-virtualScreenSerial={PROBE_SERIAL}",
        "-aspectWidth=16",
        "-aspectHeight=9",
    ])


def set_connected(on: bool) -> None:
    run(["betterdisplaycli", "set", f"-name={PROBE}", f"-connected={'on' if on else 'off'}"])


def remove_probe() -> None:
    # Always identify the device. A bare `discard` drops every discardable
    # device BetterDisplay knows about, with no undo.
    run(["betterdisplaycli", "discard", f"-name={PROBE}"], check=False)


def check(snapshot: list[dict], baseline: list[dict] | None, phase: str) -> None:
    """The invariants a window manager must keep no matter what the displays do."""
    if not snapshot:
        raise Violation(f"{phase}: rift reports no displays at all")

    owner: dict[int, str] = {}
    for display in snapshot:
        uuid = display.get("uuid") or ""
        if not uuid:
            raise Violation(f"{phase}: display {display.get('screen_id')} has no uuid")

        active = display.get("active_space_ids") or []
        space = display.get("space")
        if space is not None and space not in active:
            raise Violation(
                f"{phase}: {display['name']} shows space {space}, "
                f"not among its active spaces {active}"
            )
        for space_id in active:
            if owner.setdefault(space_id, uuid) != uuid:
                raise Violation(
                    f"{phase}: space {space_id} is claimed by two displays "
                    f"({owner[space_id]} and {uuid})"
                )

    contexts = [d for d in snapshot if d.get("is_active_context")]
    if len(contexts) != 1:
        raise Violation(f"{phase}: {len(contexts)} displays claim the active context, expected 1")

    if baseline is None:
        return

    before = {d["uuid"]: d for d in baseline}
    after = {d["uuid"]: d for d in snapshot}
    if set(before) != set(after):
        raise Violation(
            f"{phase}: display set did not return to baseline; "
            f"lost {sorted(set(before) - set(after))}, gained {sorted(set(after) - set(before))}"
        )
    # The regression that keeps coming back: a display that never went away
    # comes back showing a different desktop than it was showing before.
    for uuid, was in before.items():
        now = after[uuid]
        if was.get("space") != now.get("space"):
            raise Violation(
                f"{phase}: {now['name']} was showing space {was.get('space')} "
                f"and is now showing {now.get('space')}"
            )


def settle(want_probe: bool, timeout: float) -> tuple[list[dict], float]:
    """Wait for rift's display set to match what we just asked for.

    A display is added and removed asynchronously — macOS keeps listing a
    removed display for the better part of a second. A fixed sleep either
    wastes time or, worse, reads the set mid-transition and blames the window
    manager for the window server still catching up. So poll for the state we
    asked for and fail only when it never arrives, which is a real defect
    rather than an impatient harness.
    """
    deadline = time.monotonic() + timeout
    snapshot: list[dict] = []
    while time.monotonic() < deadline:
        snapshot = displays()
        if any(d["name"] == PROBE for d in snapshot) == want_probe:
            # Seen once is not settled; require it to hold briefly.
            time.sleep(0.25)
            snapshot = displays()
            if any(d["name"] == PROBE for d in snapshot) == want_probe:
                return snapshot, timeout - (deadline - time.monotonic())
        time.sleep(0.2)
    return snapshot, timeout


def dump_recorder(tag: str) -> pathlib.Path | None:
    path = pathlib.Path(f"/tmp/rift-churn-{tag}-{int(time.time())}.trace")
    try:
        run(["rift", "execute", "trace", "dump", str(path)])
        return path
    except Exception:
        return None


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--cycles", type=int, default=1, help="connect/disconnect cycles (default 1)")
    ap.add_argument(
        "--timeout", type=float, default=10.0,
        help="seconds to wait for the display set to settle after each change (default 10)",
    )
    ap.add_argument("--dry-run", action="store_true", help="check invariants once, change nothing")
    ap.add_argument("--keep-going", action="store_true", help="report violations but keep cycling")
    ap.add_argument("--teardown", action="store_true", help="remove the probe device at the end")
    args = ap.parse_args()

    try:
        baseline = displays()
    except Exception as exc:
        say(f"cannot reach rift: {exc}")
        return 2

    say(f"baseline: {len(baseline)} display(s) — {', '.join(d['name'] for d in baseline)}")
    baseline_windows = window_count()

    try:
        check(baseline, None, "baseline")
    except Violation as exc:
        say(f"FAIL {exc}")
        return 1
    say(f"baseline invariants hold — {baseline_windows} window(s) tracked")

    if args.dry_run:
        say("dry run — nothing changed")
        if args.teardown:
            remove_probe()
            say("probe display removed")
        return 0

    try:
        ensure_probe()
    except Exception as exc:
        say(f"cannot create the probe display: {exc}")
        return 2

    failures = 0
    completed = 0
    for cycle in range(1, args.cycles + 1):
        completed = cycle
        say(f"\ncycle {cycle}/{args.cycles}")
        try:
            set_connected(True)
            attached, took = settle(True, args.timeout)
            say(f"  attached in {took:4.1f}s -> {[d['name'] for d in attached]}")
            if not any(d["name"] == PROBE for d in attached):
                raise Violation(
                    f"cycle {cycle}: rift never saw the probe display attach "
                    f"within {args.timeout}s"
                )
            check(attached, None, f"cycle {cycle} attached")

            set_connected(False)
            detached, took = settle(False, args.timeout)
            say(f"  detached in {took:4.1f}s -> {[d['name'] for d in detached]}")
            if any(d["name"] == PROBE for d in detached):
                raise Violation(
                    f"cycle {cycle}: the probe display was still listed "
                    f"{args.timeout}s after it was disconnected"
                )
            check(detached, baseline, f"cycle {cycle} detached")

            now = window_count()
            if now < baseline_windows:
                raise Violation(
                    f"cycle {cycle}: {baseline_windows - now} window(s) lost "
                    f"({baseline_windows} -> {now})"
                )
        except Violation as exc:
            failures += 1
            say(f"  FAIL {exc}")
            dumped = dump_recorder(f"cycle{cycle}")
            if dumped:
                say(f"  flight recorder dumped to {dumped}")
            set_connected(False)
            if not args.keep_going:
                break
        except Exception as exc:
            say(f"  ERROR {exc}")
            set_connected(False)
            return 2

    if args.teardown:
        remove_probe()
        say("probe display removed")

    say(f"\n{completed - failures}/{completed} cycles clean")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
