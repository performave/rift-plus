#!/usr/bin/env python3
"""Check that a modifier-drag resize moves an edge the way the pointer went.

Eric's recorder showed it: a press in the half of the rightmost tiled window
next to the screen picked the right edge, which has nothing to trade width
with, and the layout took the drag out of the left edge instead -- in the
other direction. Dragging left moved the left edge right. `resize-spam.py`
could not see it: it measures width drift, and it pressed at the window's
exact middle, where the choice of edge is a coin toss.

For each end of the widest row, press in the half next to the screen, drag
inwards, then back out, and require the edge that moved to have moved with
the pointer. Then spam the same press point at a click rate, cycles
alternating direction and each returning to its anchor, and require the
window to end where it began.

    resize-direction-test.py

Guest only; needs at least two tiled windows side by side.
"""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"
MODS = "cmd+alt"
BUTTON = "right"
TRAVEL = 150.0
TOL = 30.0


def sh(c):
    return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()


SPACE = None


def pick_space():
    """The shown desktop with the most tiled windows. Unfiltered, `query
    windows` answers for whichever display is active, and a press on one
    display makes it the active one -- the first version of this lost its
    window after the first drag that way."""
    global SPACE
    best = (0, None)
    for d in json.loads(sh(f"{B}/rift-cli query displays") or "[]"):
        sid = d.get("space")
        if sid is None:
            continue
        SPACE = sid
        n = len(tiled())
        if n > best[0]:
            best = (n, sid)
    SPACE = best[1]


def tiled():
    arg = f" --space-id {SPACE}" if SPACE is not None else ""
    try:
        ws = json.loads(sh(f"{B}/rift-cli query windows{arg}"))
    except Exception:
        return {}
    out = {}
    for w in ws:
        if w.get("is_floating"):
            continue
        f = w.get("frame") or {}
        o, s = f.get("origin", {}), f.get("size", {})
        i = w.get("id") or {}
        out[(i.get("pid"), i.get("idx"))] = (
            w.get("app_name"), o.get("x", 0), o.get("y", 0), s.get("width", 0), s.get("height", 0))
    return out


def settle_frame(key, wait=2.5):
    time.sleep(wait)
    return tiled().get(key)


def drag(x0, y, x1):
    sh(f"{B}/mtool drag {x0:.0f} {y:.0f} {x1:.0f} {y:.0f} {MODS} 12 {BUTTON}")


def edges(frame):
    _, x, _, w, _ = frame
    return x, x + w


def check_leg(key, at_x, y, dx, which, failures):
    before = tiled()[key]
    drag(at_x, y, at_x + dx)
    after = settle_frame(key)
    (l0, r0), (l1, r1) = edges(before), edges(after)
    moved_left, moved_right = l1 - l0, r1 - r0
    moving = moved_left if which == "left" else moved_right
    fixed = moved_right if which == "left" else moved_left
    ok = abs(moving - dx) <= TOL and abs(fixed) <= TOL
    print(f"  pointer {dx:+.0f}: left edge {moved_left:+.0f}, right edge {moved_right:+.0f}"
          f"  -> {'ok' if ok else 'WRONG'} (expected the {which} edge to follow)")
    if not ok:
        failures.append((key, which, dx, moved_left, moved_right))


def main():
    pick_space()
    print(f"desktop {SPACE}")
    start = tiled()
    rows = {}
    for k, (app, x, y, w, h) in start.items():
        rows.setdefault(round(y / 50), []).append((k, app, x, y, w, h))
    row = max(rows.values(), key=len)
    if len(row) < 2:
        print("no row has two windows side by side")
        return 2
    row.sort(key=lambda r: r[2])
    failures = []
    for end, which, inward in ((row[-1], "left", -1.0), (row[0], "right", +1.0)):
        key, app, x, y, w, h = end
        # The half next to the screen: the press that used to pick the edge
        # that cannot move.
        at_x = x + w * (0.75 if which == "left" else 0.25)
        py = y + h * 0.5
        print(f"{app} (id {key}) at [{x:.0f}..{x + w:.0f}], pressing at x={at_x:.0f}; "
              f"the {which} edge should move")
        check_leg(key, at_x, py, inward * TRAVEL, which, failures)
        cur = tiled()[key]
        at_x = cur[1] + cur[3] * (0.75 if which == "left" else 0.25)
        check_leg(key, at_x, py, -inward * TRAVEL, which, failures)

        cur = tiled()[key]
        at_x = cur[1] + cur[3] * (0.75 if which == "left" else 0.25)
        before = cur[3]
        sh(f"{B}/mtool spam {at_x:.0f} {py:.0f} 120 16 8 {MODS} {BUTTON}")
        after = settle_frame(key)[3]
        ok = abs(after - before) <= TOL
        print(f"  spam 16 cycles at 8/s from x={at_x:.0f}: width {before:.0f} -> {after:.0f} "
              f"({after - before:+.0f}) -> {'ok' if ok else 'DRIFTED'}")
        if not ok:
            failures.append((key, "spam", before, after))
    if failures:
        print(f"FAIL  {failures}")
        return 1
    print("PASS  every edge followed the pointer; spam ended where it began")
    return 0


if __name__ == "__main__":
    sys.exit(main())
