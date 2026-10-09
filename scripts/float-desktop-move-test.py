#!/usr/bin/env python3
"""Move a desktop of floating windows between displays; none may be left off screen.

Reported from real use (2026-09-29): after replugging the LG, a desktop dragged
from the laptop onto the LG in Mission Control came up with its floating windows
hanging off the LG's far edge -- Obsidian at x=2520 of 2560, Preview a third
off. macOS places a moved desktop's windows itself; rift lays out tiled windows
but leaves floating ones where they are, so nothing put these right.

Mission Control's drag cannot be scripted, but what it asks for can: the
scripting addition's SPACE_MOVE (see desktop-move-seam-test.py). The desktop is
moved onto the probe display and back again -- small to large is the direction
of the report -- and every floating window's frame is sampled through each move,
not compared once before and once after: a window that sits off the edge for a
second is still one the user watched.

Passes when, a few seconds after each move, every floating window on the moved
desktop lies wholly on the display showing it.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import BIN, CLI, sh, rift, plug, unplug, settle
from importlib import import_module

MTOOL = f"{BIN}/mtool"
move_space_after = import_module("desktop-move-seam-test").move_space_after

SLACK = 2.0


def displays():
    return sorted(rift("displays") or [], key=lambda d: d["frame"]["origin"]["x"])


def rect(f):
    return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])


def overhang(frame, screen):
    x, y, w, h = frame
    sx, sy, sw, sh_ = screen
    return max(0, sx - x, sy - y, (x + w) - (sx + sw), (y + h) - (sy + sh_))


def floats(space, only=None):
    return [w for w in rift("windows", "--space-id", str(space)) or []
            if w.get("is_floating") and (only is None or w["id"]["idx"] in only)]


# The windows the scenario opened; any other float on the desktop is left out
# of the verdict.
TRACKED = None


def display_of(space):
    return next((d for d in displays() if space in (d.get("space_ids") or [])), None)


def sample(space, seconds, tag):
    """Frames of the desktop's floats, every quarter second. Returns the
    overhang of each window at the last sample and the longest any window
    spent off screen."""
    worst, since, last = {}, {}, {}
    end = time.time() + seconds
    while time.time() < end:
        d = display_of(space)
        if d is not None and d.get("space") == space:
            screen = rect(d["frame"])
            for w in floats(space, TRACKED):
                idx = w["id"]["idx"]
                off = overhang(rect(w["frame"]), screen)
                last[idx] = (off, rect(w["frame"]))
                if off > SLACK:
                    since.setdefault(idx, time.time())
                    worst[idx] = max(worst.get(idx, 0), time.time() - since[idx])
                else:
                    since.pop(idx, None)
        time.sleep(0.25)
    d = display_of(space)
    print(f"  {tag}: desktop {space} on display x={d['frame']['origin']['x']:.0f} "
          f"w={d['frame']['size']['width']:.0f}")
    for idx, (off, fr) in sorted(last.items()):
        print(f"      {idx} at {fr[0]:.0f},{fr[1]:.0f} {fr[2]:.0f}x{fr[3]:.0f}  "
              f"overhang {off:.0f}  off screen for {worst.get(idx, 0):.1f}s")
    return {idx: off for idx, (off, _) in last.items()}, worst


def open_floats(space):
    """Three fresh TextEdit documents. The guest's config floats every window
    by default, as the reporter's does, so they open floating at TextEdit's
    own size on the shown desktop."""
    # A previous run's documents carry the same titles.
    sh("killall TextEdit 2>/dev/null; sleep 2")
    for i in (1, 2, 3):
        with open(f"/tmp/fdoc{i}.txt", "w") as fh:
            fh.write(f"float doc {i}\n")
    sh("open -F -a TextEdit /tmp/fdoc1.txt /tmp/fdoc2.txt /tmp/fdoc3.txt")
    time.sleep(5)
    sh(f"pkill -x hopapp; nohup {HOPAPP} >/dev/null 2>&1 &")
    time.sleep(4)
    return [w for w in floats(space)
            if "fdoc" in (w.get("title") or "") or w.get("app_name") == "hopapp"]


HOPAPP = f"{BIN}/hopapp"


def hop(x, y, w=640, h=420):
    """Have hopapp move its own window: a frame change with no pointer, the
    way macOS relocates a moved desktop's windows (see hopapp.m)."""
    with open("/tmp/hopapp.frame", "w") as fh:
        fh.write(f"{x} {y} {w} {h}\n")
    sh("pkill -USR1 -x hopapp")


def drag_to(w, x, y):
    """Drag a window by its title bar so its top-left lands at (x, y)."""
    fx, fy, fw, _ = rect(w["frame"])
    gx, gy = fx + min(fw / 2, 200), fy + 12
    sh(f"{MTOOL} drag {gx:.0f} {gy:.0f} {x + (gx - fx):.0f} {y + 12:.0f}")
    time.sleep(0.8)


def main():
    unplug(quiet=True); settle(4)
    main_display = displays()[0]
    home = main_display["space"]
    screen = rect(main_display["frame"])
    global TRACKED
    ws = open_floats(home)
    TRACKED = {w["id"]["idx"] for w in ws}
    if not any(w.get("app_name") == "hopapp" for w in ws):
        print("FAIL setup: hopapp's window is not a float on the home desktop")
        return 2
    if len(ws) < 4:
        print(f"FAIL setup: need 3 floating windows on desktop {home}, have {len(ws)}")
        return 2
    # Spread out: one flush with the right edge, one with the bottom, one in
    # the middle -- where a move onto another display is likeliest to overhang.
    sx, sy, sw, sh_ = screen
    a, b, c = [w for w in ws if w.get("app_name") != "hopapp"][:3]
    drag_to(a, sx + sw - rect(a["frame"])[2] - 2, sy + 40)
    drag_to(b, sx + 60, sy + sh_ - rect(b["frame"])[3] - 2)
    drag_to(c, sx + sw / 2 - rect(c["frame"])[2] / 2, sy + sh_ / 3)
    time.sleep(1)
    before, _ = sample(home, 1.0, "before")
    if any(off > SLACK for off in before.values()):
        print("FAIL setup: a float was already off screen before any move")
        return 2

    plug(); settle(6)
    probe = next((d for d in displays() if home not in (d.get("space_ids") or [])), None)
    if probe is None or probe.get("space") is None:
        print("FAIL setup: the probe display did not attach with a desktop of its own")
        return 2
    main_uuid = display_of(home)["uuid"]

    failures = []
    move_space_after(home, probe["space"], focus=True)
    # A beat after the move, the way macOS's own placement lands: 300 px past
    # the probe's left edge, into no display at all.
    time.sleep(0.8)
    px, py, _, _ = rect(probe["frame"])
    hop(px - 300, py + 100)
    to_small, stuck_small = sample(home, 7.0, "moved onto the probe display")
    failures += [("onto probe", i, off) for i, off in to_small.items() if off > SLACK]

    # Whatever the main display shows now that its desktop has gone.
    back_anchor = next(d["space"] for d in displays() if d["uuid"] == main_uuid)
    move_space_after(home, back_anchor, focus=True)
    time.sleep(0.8)
    mx, my, mw, _ = rect(next(d for d in displays() if d["uuid"] == main_uuid)["frame"])
    hop(mx + mw - 340, my + 100)
    to_large, stuck_large = sample(home, 7.0, "moved back onto the main display")

    # Settled now: a window put off the edge from here on was put there on
    # purpose, and stays.
    hop(mx + mw - 340, my + 100)
    parked, _ = sample(home, 3.0, "parked off the edge after the move settled")
    hop_id = next(w["id"]["idx"] for w in ws if w.get("app_name") == "hopapp")
    if parked.get(hop_id, 0) <= SLACK:
        failures.append(("settled move rescued a parked window", hop_id, parked.get(hop_id)))
    failures += [("back to main", i, off) for i, off in to_large.items() if off > SLACK]

    sh(f"{CLI} execute trace dump /Users/vm/rift-harness/traces/float-desktop-move.trace",
       timeout=90)
    unplug(); settle(6)
    longest = max(list(stuck_small.values()) + list(stuck_large.values()) + [0])
    print(f"  longest any float stayed off screen: {longest:.1f}s")
    if failures:
        print(f"FAIL  {len(failures)} float(s) left off screen after a desktop move: {failures}")
        return 1
    print("PASS  every float on the moved desktop ends wholly on the display showing it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
