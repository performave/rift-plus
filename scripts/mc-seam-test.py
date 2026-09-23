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
    when `hidpi`, sized like a 14-inch MacBook's default in points."""
    unplug(quiet=True)
    args = [f"{BIN}/vdisp", str(width), str(height), "1"] + (["hidpi"] if hidpi else [])
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
    # Opening it again closes it.
    sh("open -a 'Mission Control'")
    time.sleep(1.8)


def screenshot(name):
    os.makedirs(SHOTS, exist_ok=True)
    path = f"{SHOTS}/{name}.png"
    sh(f"screencapture -x {path}")
    return path


def thumb_point(d, index, count):
    """Where desktop `index` of `count` sits in `d`'s Spaces bar, once the bar
    is expanded by hovering it. Thumbnails are centred as a row; the x pitch
    and the y were read off calibration screenshots (see `calibrate`)."""
    x, y, w, h = span(d)
    pitch = float(os.environ.get("MC_PITCH", "0")) or min(w / (count + 1), w * 0.11)
    row = count * pitch
    cx = x + (w - row) / 2 + pitch * (index + 0.5)
    cy = y + float(os.environ.get("MC_THUMB_Y", "70"))
    return cx, cy


def drag_desktop(src_display, src_index, src_count, dst_display, dst_count):
    """Hover the source bar to expand it, press on the thumbnail, carry it to
    the end of the destination bar, release."""
    sx, sy = thumb_point(src_display, src_index, src_count)
    x, y, w, h = span(dst_display)
    sh(f"{MTOOL} move {sx:.0f} {span(src_display)[1] + 8:.0f}")
    time.sleep(1.0)
    sh(f"{MTOOL} move {sx:.0f} {sy:.0f}")
    time.sleep(0.6)
    # Past the last thumbnail, where Mission Control shows its "+" -- a drop
    # there appends the desktop to that display.
    dx, dy = thumb_point(dst_display, dst_count, dst_count + 1)
    sh(f"MTOOL_HOLD_MS=400 {MTOOL} drag {sx:.0f} {sy:.0f} {dx:.0f} {dy:.0f} 0 60", timeout=30)
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
        did = sh("sed -n 's/.*display_id=\\([0-9]*\\).*/\\1/p' /tmp/vdisp.out").strip()
        main_h = max(b[3] for b in cg_display_bounds() if b[0] == 0 and b[1] == 0)
        pw, ph = 1512, 982
        print("  " + sh(f"{DTOOL} place {did} {-pw} {int(main_h - ph)}").strip())
        settle(6)
    ds = displays()
    probe = probe_display(ds, before)
    main_d = next(d for d in ds if d["uuid"] != probe["uuid"])
    print(f"  main {span(main_d)} desktops={main_d.get('space_ids')}")
    print(f"  probe {span(probe)} hidpi={hidpi} desktops={probe.get('space_ids')}")

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
        if not others:
            mx, my, mw, mh = span(main_d)
            sh(f"{MTOOL} move {mx + mw / 2:.0f} {my + mh / 2:.0f}")
            rift_exec("space", "create")
            time.sleep(1.5)
            ids = (next(d for d in displays() if d["uuid"] == main_d["uuid"]).get("space_ids") or [])
            others = [s for s in ids if s != home]
        rift_exec("space", "switch-to", str(ids.index(others[0]) + 1))
        time.sleep(1.5)

    ds = displays()
    main_d = next(d for d in ds if d["uuid"] == main_d["uuid"])
    probe = next(d for d in ds if d["uuid"] == probe["uuid"])
    ids = main_d.get("space_ids") or []
    pids = probe.get("space_ids") or []

    mission_control(True)
    screenshot("before-drag")
    drag_desktop(main_d, ids.index(home), len(ids), probe, len(pids))
    screenshot("after-drag")
    mission_control(False)
    time.sleep(3)

    ds = displays()
    probe = next(d for d in ds if d["uuid"] == probe["uuid"])
    if home not in (probe.get("space_ids") or []):
        print(f"FAIL setup: the drag did not move desktop {home} to the probe "
              f"(probe has {probe.get('space_ids')}); see {SHOTS}")
        return 2
    bad = report("dragged", home)

    if probe.get("space") != home:
        px, py, pw, ph = span(probe)
        sh(f"{MTOOL} move {px + pw / 2:.0f} {py + ph / 2:.0f}")
        pids = probe.get("space_ids") or []
        rift_exec("space", "switch-to", str(pids.index(home) + 1))
        time.sleep(3)
        bad += report("shown on the probe", home)
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
