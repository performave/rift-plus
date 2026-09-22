#!/usr/bin/env python3
"""Moving a window between displays and between desktops, by hand.

Both of these have been reported broken and both resisted ad-hoc testing,
because the interesting configurations are fiddly to construct: a cross-display
drop needs a tiled window on *each* display, and a desktop move needs the
1-based index macOS uses, which is not the desktop id. `chaos.py` already knows
how to map one to the other (`all_space_ids`), so this builds on that rather
than guessing.

What this does not cover: Mission Control's own drag. It is a system UI with no
scripting surface and no stable hit targets, so what is driven here is the
thing that drag ultimately asks for -- the window server putting a window on
another space -- and rift's reaction to it, which is the half that has misbehaved.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import (CLI, sh, rift, plug, unplug, settle, all_space_ids, tile_all)

def tiled(space=None):
    ws = rift("windows", "--space-id", str(space)) if space else rift("windows")
    return [w for w in (ws or []) if not w.get("is_floating")]

def displays():
    return sorted(rift("displays") or [], key=lambda d: d["frame"]["origin"]["x"])

def index_of(space):
    order = all_space_ids(rift("displays") or [])
    return order.index(space) + 1 if space in order else None

def frame(w):
    f = w["frame"]
    return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])

def on(d, w):
    x, _, wd, _ = frame(w)
    dx, dw = d["frame"]["origin"]["x"], d["frame"]["size"]["width"]
    return dx <= x + wd / 2 <= dx + dw

def report(name, ok, detail):
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")
    return ok

def test_space_move():
    """A window sent to another desktop must still be rift's, and be there."""
    ds = displays()
    d = ds[0]
    here = d.get("space")
    others = [s for s in (d.get("active_space_ids") or []) + (d.get("inactive_space_ids") or [])
              if s != here]
    if not others:
        return report("space move", True, "only one desktop on this display; nothing to test")
    dest = others[0]
    di = index_of(dest)
    if di is None:
        return report("space move", False, f"desktop {dest} has no index in the order")
    ws = tiled(here)
    if not ws:
        return report("space move", False, "no tiled window on the shown desktop")
    w = ws[0]
    idx = w["id"]["idx"]
    sh(f"{CLI} execute window focus --window-id {idx}")
    time.sleep(0.6)
    sh(f"{CLI} execute space move-window {di}")
    time.sleep(3.0)
    there = [q for q in tiled(dest) if q["id"]["idx"] == idx]
    gone_from_here = not any(q["id"]["idx"] == idx for q in tiled(here))
    ok = bool(there) and gone_from_here
    return report("space move", ok,
                  f"window {idx} -> desktop {dest} (index {di}): "
                  f"{'arrived' if there else 'NOT on the destination'}, "
                  f"{'left the old desktop' if gone_from_here else 'STILL on the old desktop'}")

def test_cross_display_drag():
    """A tiled window carried to the other display must land there."""
    plug(); settle(4)
    ds = displays()
    if len(ds) < 2:
        return report("cross-display drag", False, "probe display did not attach")
    A, Bd = ds[0], ds[1]
    # Seed the empty display by sending a window to the desktop it is showing.
    for _ in range(2):
        a = [w for w in tiled() if on(A, w)]
        b = [w for w in tiled() if on(Bd, w)]
        if a and b:
            break
        have, want = (a, Bd) if not b else (b, A)
        if len(have) < 2:
            break
        di = index_of(want.get("space"))
        if di is None:
            break
        mv = have[-1]
        sh(f"{CLI} execute window focus --window-id {mv['id']['idx']}")
        time.sleep(0.5)
        sh(f"{CLI} execute space move-window {di}")
        time.sleep(3.0)
    a = [w for w in tiled() if on(A, w)]
    b = [w for w in tiled() if on(Bd, w)]
    if not a or not b:
        return report("cross-display drag", False,
                      f"could not seed both displays (A={len(a)}, B={len(b)}); untested")
    src, dst_d, tgt = (a[0], Bd, b[0]) if len(a) >= len(b) else (b[0], A, a[0])
    x, y, wd, _ = frame(src)
    tx, ty, tw, th = frame(tgt)
    sh(f"{CLI[:CLI.rindex('/')]}/mtool drag {x+wd/2:.0f} {y+12:.0f} "
       f"{tx+tw*0.25:.0f} {ty+th*0.5:.0f} none 45")
    time.sleep(3.5)
    now = [q for q in tiled() if q["id"]["idx"] == src["id"]["idx"]]
    if not now:
        return report("cross-display drag", False, "the window is no longer listed")
    landed = on(dst_d, now[0])
    nx, _, nw, _ = frame(now[0])
    inside = nx >= dst_d["frame"]["origin"]["x"] - 40 and \
             nx + nw <= dst_d["frame"]["origin"]["x"] + dst_d["frame"]["size"]["width"] + 40
    return report("cross-display drag", landed and inside,
                  f"{src['app_name']} -> {dst_d['name']}: "
                  f"{'landed' if landed else 'did not cross'}, frame x={nx:.0f} w={nw:.0f} "
                  f"{'inside the display' if inside else 'OUTSIDE the display'}")

if __name__ == "__main__":
    tile_all(); time.sleep(1)
    results = [test_space_move(), test_cross_display_drag()]
    unplug(quiet=True); settle(2)
    print(f"\n{sum(results)} of {len(results)} passed")
    sys.exit(0 if all(results) else 1)
