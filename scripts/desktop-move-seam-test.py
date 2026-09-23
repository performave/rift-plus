#!/usr/bin/env python3
"""Move a desktop to the other display and look for windows left over the seam.

Reported from real use: plug the LG in, move desktops onto it (Mission Control
drag), and some windows end up "way over the seam" -- only a sliver visible at
the edge of the viewport to drag them out by.

Mission Control's drag cannot be scripted, but what it asks for can: the
scripting addition's SPACE_MOVE puts a desktop behind one on another display,
which moves it there. That is the same operation rift's own return pass uses,
spoken here directly over the addition's socket.

Both kinds of window are put on the desktop, because they are handled by
different code: tiled ones are laid out by the tree for whatever screen now
shows the desktop; floating ones are not laid out at all, but rift remembers
and restores floating positions per desktop -- and a position remembered in the
coordinates of the display the desktop came from, re-applied after it moved,
would put a window exactly where this report says. The flight recorder is
dumped so rift's own writes to each window after the move can be read.
"""
import json, os, socket, struct, sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, rift_exec, plug, unplug, settle, tile_all, is_tiled
from chaos import focus as chaos_focus

SOCK = "/tmp/rift-sa_vm.socket"
SPACE_MOVE = 0x05

def sa(op, args):
    body = bytes([op]) + args
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(3)
    s.connect(SOCK)
    s.sendall(struct.pack("=h", len(body)) + body)
    try:
        while s.recv(64):
            pass
    except OSError:
        pass
    s.close()

def move_space_after(space, after, focus=True):
    sa(SPACE_MOVE, struct.pack("=QQQB", space, after, 0, 1 if focus else 0))

def displays():
    return sorted(rift("displays") or [], key=lambda d: d["frame"]["origin"]["x"])

def span(d):
    f = d["frame"]
    return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])

def visible_fraction(frame, dspan):
    x, y, w, h = frame
    dx, dy, dw, dh = dspan
    ox = max(0, min(x + w, dx + dw) - max(x, dx))
    oy = max(0, min(y + h, dy + dh) - max(y, dy))
    return (ox * oy) / (w * h) if w and h else 0

def report(tag, space):
    d = next((d for d in displays() if space in (d.get("space_ids") or [])), None)
    if d is None:
        print(f"  {tag}: desktop {space} is on no display"); return []
    shown = d.get("space") == space
    bad = []
    print(f"  {tag}: desktop {space} on display x={span(d)[0]:.0f} w={span(d)[2]:.0f}"
          f" ({'shown' if shown else 'NOT shown'})")
    for w in rift("windows", "--space-id", str(space)) or []:
        f = w["frame"]
        fr = (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])
        vis = visible_fraction(fr, span(d))
        kind = "tiled" if is_tiled(w) else ("float" if w.get("is_floating") else "limbo")
        flag = "" if vis > 0.9 else "   <== OVER THE SEAM" if vis > 0 else "   <== OFF THE DISPLAY"
        print(f"      {w['id']['idx']} {w['app_name'][:10]:<10} {kind:<5} "
              f"{fr[0]:.0f},{fr[1]:.0f} {fr[2]:.0f}x{fr[3]:.0f}  {vis*100:.0f}% visible{flag}")
        if shown and vis <= 0.9:
            bad.append((w["id"]["idx"], kind, vis))
    return bad

def main():
    tile_all(); time.sleep(1)
    a = displays()[0]
    home = a.get("space")
    ws = [w for w in rift("windows", "--space-id", str(home)) or [] if is_tiled(w)]
    if len(ws) < 3:
        print(f"FAIL setup: need 3 tiled windows on desktop {home}, have {len(ws)}"); return 2
    # One of them floating, where rift restores positions rather than tiles.
    chaos_focus(ws[-1]); time.sleep(0.5)
    rift_exec("window toggle-float"); time.sleep(1)
    # Give the float a position well inside the display it is on.
    report("before", home)

    plug(); settle(6)
    ds = displays()
    other = next((d for d in ds if home not in (d.get("space_ids") or [])), None)
    if other is None or other.get("space") is None:
        print("FAIL setup: the probe display did not attach with a desktop of its own")
        return 2
    anchor = other["space"]
    print(f"  probe display x={span(other)[0]:.0f} w={span(other)[2]:.0f}, showing {anchor}")
    report("attached, not moved yet", home)

    move_space_after(home, anchor, focus=True)
    time.sleep(6)
    bad = report("moved to the probe display", home)
    sh(f"{CLI} execute trace dump /Users/vm/rift-harness/seam.json", timeout=90)

    unplug(); settle(6)
    report("probe unplugged", home)

    if bad:
        print(f"FAIL  {len(bad)} window(s) left over the seam after the desktop moved: {bad}")
        return 1
    print("PASS  every window on the moved desktop is on the display now showing it")
    return 0

if __name__ == "__main__":
    sys.exit(main())
