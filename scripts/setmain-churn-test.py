#!/usr/bin/env python3
"""The stranding report that the shown-desktop filter does not explain.

The retraction in the handoff covers plain plug/unplug: frames on a desktop
nobody shows are stale, not wrong. The report it came from was a different
sequence -- attach a display, make it *main*, which moves the other display's
origin negative, then detach -- and that step is the one the retraction does
not touch, because it moves an existing display rather than adding one.

So run exactly that, and judge only the desktops on a screen. Two of the four
frames in the original report overlapped on what looked like the shown desktop,
which this would still catch.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, plug, unplug, settle, tile_all

TOOL = CLI[:CLI.rindex("/")]

def displays():
    return sorted(rift("displays") or [], key=lambda d: d["frame"]["origin"]["x"])

def spans():
    return [(d["frame"]["origin"]["x"], d["frame"]["origin"]["x"] + d["frame"]["size"]["width"])
            for d in displays()]

def rect(w):
    f = w["frame"]
    return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])

def offences(tag):
    """Off-display and overlapping tiled windows, on shown desktops only."""
    sp = spans()
    out = []
    for d in displays():
        s = d.get("space")
        if s is None:
            continue
        ws = [w for w in (rift("windows", "--space-id", str(s)) or [])
              if not w.get("is_floating")]
        for w in ws:
            x, y, wd, h = rect(w)
            mid = x + wd / 2
            if not any(a - 20 <= mid <= b + 20 for a, b in sp):
                out.append(f"desktop {s}: {w['app_name'][:10]}#{w['id']['idx']} "
                           f"off-display at x={x:.0f}..{x+wd:.0f}")
        for i in range(len(ws)):
            for j in range(i + 1, len(ws)):
                ax, ay, aw, ah = rect(ws[i]); bx, by, bw, bh = rect(ws[j])
                ox = min(ax+aw, bx+bw) - max(ax, bx)
                oy = min(ay+ah, by+bh) - max(ay, by)
                if ox > 20 and oy > 20:
                    out.append(f"desktop {s}: {ws[i]['app_name'][:8]}#{ws[i]['id']['idx']} and "
                               f"{ws[j]['app_name'][:8]}#{ws[j]['id']['idx']} overlap {ox:.0f}x{oy:.0f}")
    print(f"  {tag}: displays {[f'{a:.0f}..{b:.0f}' for a, b in spans()]}, "
          f"{len(out)} offence(s)")
    for o in out:
        print(f"      {o}")
    return out

def screen_ids():
    return [d["screen_id"] for d in displays()]

def main():
    tile_all(); time.sleep(1)
    total = 0
    for cycle in (1, 2, 3):
        print(f"--- cycle {cycle} ---")
        offences("start")
        plug(); settle(6)
        ids = screen_ids()
        if len(ids) < 2:
            print("  probe display did not attach; untested")
            return 1
        # The step the retraction does not cover: the *new* display becomes
        # main, which moves the original's origin negative.
        probe = ids[-1]
        print("  setmain:", sh(f"{TOOL}/dtool setmain {probe}").strip())
        settle(7)
        offences("probe is main")
        unplug(); settle(8)
        bad = offences("detached")
        total += len(bad)
        # Put the arrangement back before the next cycle, or cycle 2 starts
        # from an origin cycle 1 moved.
        ids = screen_ids()
        if ids:
            sh(f"{TOOL}/dtool setmain {ids[0]}")
            settle(5)
    print(f"\n{total} offence(s) on shown desktops across three setmain cycles")
    return 0 if total == 0 else 1

if __name__ == "__main__":
    sys.exit(main())
