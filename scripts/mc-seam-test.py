#!/usr/bin/env python3
"""Drag a desktop to the other display in Mission Control, then look for
windows left over the seam.

Reported from real use: plug the laptop into the LG, move desktops onto the LG
in Mission Control, and some windows end up "way over the seam" -- a sliver
at the edge of the viewport to drag them out by.

`desktop-move-seam-test.py` asked the scripting addition to move the desktop
and found nothing. That is not what the user did, and it tested one pair of
1x displays; the report came from a Retina laptop beside a 1x monitor. This
one drags the desktop's thumbnail in Mission Control with posted mouse events,
as a user would, onto a probe display that can be 2x, and checks the desktop
both as dragged and after it is later shown there -- a desktop moved while
hidden is laid out only when it is next shown.

    mc-seam-test.py calibrate [hidpi] [left]   screenshot Mission Control and exit
    mc-seam-test.py plug [hidpi] [left]        plug only, sampling for windows over the seam
    mc-seam-test.py run [hidpi] [left] [shown] drag and check

`shown` drags the desktop the main display is showing; the default drags a
hidden one, which is the usual case when moving several. `left` arranges the
probe as the reporter's laptop is: to the left, bottom edges aligned.

Guest only.
"""
import os, subprocess, sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import (BIN, CLI, DTOOL, UID, VDISP_PLIST, cg_display_bounds, is_tiled,
                   probe_display, rift, rift_exec, settle, sh, tile_all, unplug,
                   wait_for_displays, Violation)
from chaos import focus as chaos_focus

MTOOL = f"{CLI[:CLI.rindex('/')]}/mtool"
SHOTS = "/Users/vm/rift-harness/mc"


def plug(hidpi: bool, width=1512, height=982):
    """chaos.plug with a scale. The probe stands in for the laptop's panel
    when `hidpi`, sized like a 14-inch MacBook's default in points.

    A virtual display offered a HiDPI mode still comes up in its largest 1x
    one, so the 2x mode is chosen after it attaches. That choice is stored
    against the display's identity, which is why the probe here has serials
    of its own: the 1x probe every other scenario plugs must not inherit it."""
    unplug(quiet=True)
    # A fresh identity per run as well: macOS and rift's display record both
    # remember a display that has been here before, and plugging the last
    # run's probe back in rightly sends its windows back to it -- which then
    # leaves this run nothing to drag.
    serial = hex(0x5100 + int(time.time()) % 0xff) if hidpi else hex(0x5200 + int(time.time()) % 0xff)
    args = [f"{BIN}/vdisp", str(width), str(height), serial] + (["hidpi"] if hidpi else [])
    with open(VDISP_PLIST, "w") as fh:
        fh.write('<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>\n'
                 '<key>Label</key><string>vdisp</string>\n<key>ProgramArguments</key><array>'
                 + "".join(f"<string>{a}</string>" for a in args)
                 + '</array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><false/>\n'
                 '<key>StandardOutPath</key><string>/tmp/vdisp.out</string>\n'
                 '<key>StandardErrorPath</key><string>/tmp/vdisp.err</string>\n</dict></plist>')
    sh(f"launchctl bootstrap gui/{UID} {VDISP_PLIST} 2>/dev/null; true")
    if not wait_for_displays(2):
        raise Violation(f"virtual display never attached ({sh('cat /tmp/vdisp.err')})")
    if hidpi:
        # A display that has just attached has no modes to offer for a moment.
        for _ in range(20):
            out = sh(f"{DTOOL} setmode {width} {height} {probe_id()} 2>&1").strip()
            if out.endswith("-> ok"):
                break
            time.sleep(0.5)
        print(f"  {out}")
        time.sleep(3)


def probe_id():
    """The probe's CoreGraphics id: the last attach vdisp reported. The file
    is appended to, never truncated, so the first line is some long-gone
    probe's."""
    return sh("sed -n 's/.*display_id=\\([0-9]*\\).*/\\1/p' /tmp/vdisp.out | tail -1").strip()


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
        print(f"  {tag}: desktop {space} is on no display")
        return [("desktop", "gone", 0)]
    shown = d.get("space") == space
    bad = []
    print(f"  {tag}: desktop {space} on {d.get('name')} x={span(d)[0]:.0f} w={span(d)[2]:.0f}"
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
            bad.append((w["id"]["idx"], kind, round(vis, 2)))
    return bad


def mission_control(on: bool):
    """Open or close Mission Control. `open -a` toggles it and nothing says
    whether it is open, so Escape comes first -- it closes Mission Control if
    open and does nothing otherwise -- which makes the state known."""
    sh(f"{DTOOL} key 53")
    time.sleep(1.2)
    if on:
        sh("open -a 'Mission Control'")
        time.sleep(1.8)


def screenshot(name):
    os.makedirs(SHOTS, exist_ok=True)
    # One file per display, main first.
    path = f"{SHOTS}/{name}.png"
    sh(f"screencapture -x {path} {SHOTS}/{name}-2.png")
    return path


def order(d):
    """`d`'s desktops as Mission Control shows them, left to right, from the
    window server. rift's `space_ids` has been seen a move behind, and a
    thumbnail picked by a stale index is some other desktop."""
    def read():
        for line in sh(f"{DTOOL} spaces").splitlines():
            parts = line.split()
            if parts and parts[0] == d["uuid"]:
                return [int(x) for x in parts[1:]]
        return []
    # Right after a drop the window server reorders once more; wait for two
    # readings a second apart to agree.
    last = read()
    for _ in range(8):
        time.sleep(1)
        now = read()
        if now == last:
            break
        last = now
    print(f"  order on {d['uuid'][:8]}: {last}")
    return last


def whiteness(x, y, size=24):
    """The share of near-white pixels in a small square around a global
    point. A desktop holding the test's TextEdit windows has a mostly white
    thumbnail; an empty one shows the wallpaper."""
    path = "/tmp/mc-px.bmp"
    sh(f"screencapture -x -t bmp -R{x - size / 2:.0f},{y - size / 2:.0f},{size},{size} {path}")
    try:
        data = open(path, "rb").read()
    except OSError:
        return 0.0
    import struct
    off = struct.unpack_from("<I", data, 10)[0]
    w, h = struct.unpack_from("<ii", data, 18)
    bpp = struct.unpack_from("<H", data, 28)[0] // 8
    row = (w * bpp + 3) & ~3
    white = total = 0
    for j in range(abs(h)):
        for i in range(w):
            b, g, r = data[off + j * row + i * bpp: off + j * row + i * bpp + 3]
            total += 1
            white += (r > 235 and g > 235 and b > 235)
    return white / total if total else 0.0


def thumb_with_windows(rect, count):
    """Which of `count` thumbnails in the bar at `rect` shows the windows.
    The desktop order both rift and the window server report does match the
    bar -- once it settles; right after a drop both lag it, and a thumbnail
    picked by index then was some other desktop. Looking cannot lag. Hovers
    the bar first so it is expanded."""
    x, y, w, h = rect
    sh(f"{MTOOL} move {x + w / 2:.0f} {y + 8:.0f}")
    time.sleep(1.2)
    scores = [whiteness(*thumb_point(rect, i, count)) for i in range(count)]
    print(f"  thumbnails' whiteness: {[round(v, 2) for v in scores]}")
    best = max(range(count), key=lambda i: scores[i])
    return best if scores[best] > 0.3 else None


def cg_rect(d):
    """`d`'s full CoreGraphics bounds -- Mission Control lays its bar out on
    the whole display, not rift's visible frame -- found by overlap."""
    x, y, w, h = span(d)
    return max(cg_display_bounds(),
               key=lambda b: max(0, min(x + w, b[0] + b[2]) - max(x, b[0]))
               * max(0, min(y + h, b[1] + b[3]) - max(y, b[1])))


def thumb_point(rect, index, count):
    """The centre of desktop `index` of `count` in the Spaces bar of the
    display at `rect`, once the bar is expanded. Read off screenshots of this
    guest's Mission Control on a 2550x1347 1x display and a 1512x982 2x one:
    thumbnails are 90pt tall, as wide as the display's aspect makes them, 30pt
    apart, centred as a row, with their centres 96pt below the display's top."""
    x, y, w, h = rect
    tw = 90 * w / h
    pitch = tw + 30
    row = count * pitch - 30
    return x + (w - row) / 2 + index * pitch + tw / 2, y + 96


def drag_desktop(src, src_index, src_count, dst, dst_count):
    """Hover the source bar to expand it, press on the thumbnail, carry it to
    the end of the destination bar, release."""
    sx, sy = thumb_point(src, src_index, src_count)
    sh(f"{MTOOL} move {sx:.0f} {src[1] + 8:.0f}")
    time.sleep(1.0)
    sh(f"{MTOOL} move {sx:.0f} {sy:.0f}")
    time.sleep(0.6)
    # One slot past the last thumbnail: a drop there appends the desktop to
    # that display.
    dx, dy = thumb_point(dst, dst_count, dst_count + 1)
    out = sh(f"MTOOL_HOLD_MS=400 {MTOOL} drag {sx:.0f} {sy:.0f} {dx:.0f} {dy:.0f} 0 60", timeout=30)
    print(f"  {out.strip()}")
    time.sleep(1.5)


def main():
    args = sys.argv[1:]
    mode = args[0] if args else "run"
    hidpi = "hidpi" in args
    drag_shown = "shown" in args

    before = displays()
    plug(hidpi)
    settle(6)
    if "left" in args:
        # The reporter's arrangement: the laptop to the left of the monitor,
        # bottom edges aligned. The probe's CoreGraphics id is what vdisp
        # printed when it attached.
        did = probe_id()
        main_h = max(b[3] for b in cg_display_bounds() if b[0] == 0 and b[1] == 0)
        pw, ph = 1512, 982
        print("  " + sh(f"{DTOOL} place {did} {-pw} {int(main_h - ph)}").strip())
        settle(6)
    ds = displays()
    probe = probe_display(ds, before)
    main_d = next(d for d in ds if d["uuid"] != probe["uuid"])
    print(f"  main {span(main_d)} desktops={main_d.get('space_ids')}")
    print(f"  probe {span(probe)} hidpi={hidpi} desktops={probe.get('space_ids')}")

    if mode == "plug":
        # Just the plug, watched. As a display arrives macOS moves windows to
        # where they last were on it -- partly across the seam -- and rift
        # used to read those frames as tile edges being dragged, moving
        # splits past the edge of the screen. Sample every shown desktop for
        # a while and report any tiled window mostly off its display.
        bad = []
        for i in range(10):
            for d in displays():
                if d.get("space") is not None:
                    bad += [(i,) + b for b in report(f"sample {i}", d["space"])
                            if b[1] == "tiled"]
            time.sleep(1)
        unplug()
        settle(6)
        if bad:
            print(f"FAIL  tiled window(s) left over the seam after the plug: {bad[:6]}")
            return 1
        print("PASS  every tiled window stayed on its display through the plug")
        return 0

    if mode == "calibrate":
        mission_control(True)
        screenshot("collapsed")
        mx, my, mw, mh = span(main_d)
        sh(f"{MTOOL} move {mx + mw / 2:.0f} {my + 8:.0f}")
        time.sleep(1.2)
        screenshot("expanded-main")
        px, py, pw, ph = span(probe)
        sh(f"{MTOOL} move {px + pw / 2:.0f} {py + 8:.0f}")
        time.sleep(1.2)
        screenshot("expanded-probe")
        mission_control(False)
        print(f"  screenshots in {SHOTS}")
        return 0

    # The windows: all of them tiled on the main display's shown desktop,
    # one floated, which rift restores by position rather than lays out.
    # Earlier runs leave desktops behind, and the windows are not always on
    # the one shown; go to whichever holds most of them.
    mids = main_d.get("space_ids") or []
    most = max(mids, key=lambda sp: len(rift("windows", "--space-id", str(sp)) or []))
    if most != main_d.get("space"):
        mx, my, mw, mh = span(main_d)
        sh(f"{MTOOL} move {mx + mw / 2:.0f} {my + mh / 2:.0f}")
        rift_exec("space", "switch-to", str(mids.index(most) + 1))
        time.sleep(2)
        main_d = next(d for d in displays() if d["uuid"] == main_d["uuid"])
    tile_all()
    time.sleep(1)
    home = main_d["space"]
    ws = [w for w in rift("windows", "--space-id", str(home)) or [] if is_tiled(w)]
    if len(ws) < 3:
        print(f"FAIL setup: need 3 tiled windows on desktop {home}, have {len(ws)}")
        return 2
    chaos_focus(ws[-1])
    time.sleep(0.5)
    rift_exec("window toggle-float")
    time.sleep(1)
    report("before", home)

    ids = main_d.get("space_ids") or []
    if not drag_shown:
        # Show some other desktop on the main display, so the one with the
        # windows is dragged while hidden.
        others = [s for s in ids if s != home]
        # Desktop commands act on the display under the pointer.
        mx, my, mw, mh = span(main_d)
        sh(f"{MTOOL} move {mx + mw / 2:.0f} {my + mh / 2:.0f}")
        if not others:
            rift_exec("space", "create")
            time.sleep(1.5)
            ids = (next(d for d in displays() if d["uuid"] == main_d["uuid"]).get("space_ids") or [])
            others = [s for s in ids if s != home]
        rift_exec("space", "switch-to", str(ids.index(others[0]) + 1))
        time.sleep(1.5)

    def move_and_show(src, dst, tag):
        """In Mission Control: drag the desktop holding the windows from
        `src`'s bar to the end of `dst`'s, then click it there to show it --
        which closes Mission Control, as it does for a user. None on success,
        else why the setup failed."""
        mission_control(True)
        sr = cg_rect(src)
        n = len(order(src))
        i = thumb_with_windows(sr, n)
        screenshot(f"before-{tag}")
        if i is None:
            mission_control(False)
            return f"no thumbnail on {src['uuid'][:8]} shows the windows"
        drag_desktop(sr, i, n, cg_rect(dst), len(order(dst)))
        screenshot(f"after-{tag}")
        ids = order(dst)
        if home not in ids:
            mission_control(False)
            return f"the drag did not move desktop {home} (the display has {ids})"
        dr = cg_rect(dst)
        j = thumb_with_windows(dr, len(ids))
        if j is None:
            mission_control(False)
            return f"desktop {home} moved, but no thumbnail on {dst['uuid'][:8]} shows the windows"
        tx, ty = thumb_point(dr, j, len(ids))
        print("  " + sh(f"{MTOOL} click {tx:.0f} {ty:.0f}").strip())
        time.sleep(3)
        now = next(d for d in displays() if d["uuid"] == dst["uuid"])
        if now.get("space") != home:
            return f"clicking its thumbnail did not show desktop {home} (shows {now.get('space')})"
        return None

    # LG to laptop first: the windows' desktop dragged (hidden) onto the 2x
    # panel and shown there.
    why = move_and_show(main_d, probe, "drag")
    if why:
        print(f"FAIL setup: {why}; see {SHOTS}")
        return 2
    bad = report("shown on the probe", home)
    # And again a few seconds on, for anything that writes a frame late.
    time.sleep(4)
    bad += report("a few seconds later", home)

    # Then the direction the report was about: from the laptop's panel to
    # the monitor, with the windows laid out for the panel, dragged while
    # shown.
    why = move_and_show(probe, main_d, "drag-back")
    if why:
        print(f"FAIL setup: {why}; see {SHOTS}")
        return 2
    bad += report("dragged back and shown on the main display", home)
    time.sleep(4)
    bad += report("a few seconds later", home)
    screenshot("end-back")
    screenshot("end")
    sh(f"{CLI} execute trace dump /Users/vm/rift-harness/mc-seam.json", timeout=90)

    unplug()
    settle(6)
    report("probe unplugged", home)

    if bad:
        print(f"FAIL  {len(bad)} window(s) left over the seam: {bad}")
        return 1
    print("PASS  every window on the dragged desktop is on the display showing it")
    return 0


if __name__ == "__main__":
    sys.exit(main())
