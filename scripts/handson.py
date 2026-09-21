#!/usr/bin/env python3
"""Drive rift the way a person does, and look at what happens.

The scripted battery in `chaos.py` only asks the questions someone thought to
encode. This does the things a hand does -- grab the boundary between two
tiles and pull it, hold the modifier and resize, throw one window onto
another, stack, fullscreen, switch desktops, unplug the display mid-session --
and captures a screenshot at every step, because a structurally perfect tree
can still render as overlapping windows (see docs/vm-harness-handoff.md).

Run it inside the guest's Aqua session, the same way `chaos.py` is run.
"""
import json, os, subprocess, sys, time

HOME = os.path.expanduser("~")
BIN = f"{HOME}/rift-harness/bin"
CLI, MTOOL, DTOOL = f"{BIN}/rift-cli", f"{BIN}/mtool", f"{BIN}/dtool"
SHOTS = "/tmp/handson"
STEP = [0]
FAILURES = []


def sh(cmd, timeout=30):
    try:
        r = subprocess.run(["/bin/bash", "-lc", cmd], capture_output=True,
                           text=True, timeout=timeout)
        return r.stdout.strip()
    except subprocess.TimeoutExpired:
        return ""


def q(*args):
    raw = sh(f"{CLI} query " + " ".join(args))
    try:
        return json.loads(raw)
    except Exception:
        return None


def shot(label):
    STEP[0] += 1
    path = f"{SHOTS}/{STEP[0]:02d}-{label}.png"
    sh(f"/usr/sbin/screencapture -x '{path}'")
    return path


def note(msg):
    print(f"  {msg}", flush=True)


def fail(msg):
    FAILURES.append(msg)
    print(f"  FAIL {msg}", flush=True)


def shown_spaces():
    return [d.get("space") for d in (q("displays") or []) if d.get("space") is not None]


def tiled_frames(space):
    """window server id -> (x, y, w, h) for every tiled window on `space`."""
    out = {}
    for w in (q("windows", "--space-id", str(space)) or []):
        if w.get("is_floating"):
            continue
        f = w.get("frame") or {}
        o, s = f.get("origin", {}), f.get("size", {})
        out[str(w.get("window_server_id"))] = (
            round(o.get("x", 0)), round(o.get("y", 0)),
            round(s.get("width", 0)), round(s.get("height", 0)))
    return out


def settle(seconds=2.5):
    time.sleep(seconds)


def step(title):
    print(f"\n--- {title} ---", flush=True)


def main():
    os.makedirs(SHOTS, exist_ok=True)
    sh(f"rm -f {SHOTS}/*.png")

    spaces = shown_spaces()
    if not spaces:
        print("no display is showing a desktop -- is an app in native fullscreen?")
        return 2
    space = spaces[0]
    shot("start")

    step("what we are working with")
    frames = tiled_frames(space)
    for ident, f in sorted(frames.items(), key=lambda kv: kv[1][0]):
        note(f"{ident:>6}  ({f[0]:>5},{f[1]:>4})  {f[2]:>4}x{f[3]}")
    if len(frames) < 2:
        print("need at least two tiled windows to drive anything")
        return 2

    # ---------------------------------------------------------------- edges
    step("drag the boundary between two tiles (mouse.edge_resize)")
    # The boundary is the right edge of the left-most window; grab it at its
    # vertical middle, well away from the corners so only one axis moves.
    ordered = sorted(frames.items(), key=lambda kv: kv[1][0])
    left_id, left = ordered[0]
    right_id, right = ordered[1]
    edge_x = left[0] + left[2]
    edge_y = left[1] + left[3] // 2
    target_x = edge_x - 120
    note(f"boundary at x={edge_x}, pulling it to {target_x}")
    sh(f"{MTOOL} drag {edge_x} {edge_y} {target_x} {edge_y} '' 30", timeout=40)
    settle(3)
    shot("after-edge-drag")
    after = tiled_frames(space)
    if left_id in after and right_id in after:
        new_left, new_right = after[left_id], after[right_id]
        moved = left[2] - new_left[2]
        note(f"left  {left[2]} -> {new_left[2]} (narrower by {moved})")
        note(f"right {right[2]} -> {new_right[2]} at x={new_right[0]}")
        if moved < 40:
            fail(f"the boundary did not move: left width {left[2]} -> {new_left[2]}")
        elif abs((new_right[0] - (new_left[0] + new_left[2])) -
                 (right[0] - (left[0] + left[2]))) > 6:
            fail("the neighbour did not follow the boundary; a gap opened")
        else:
            note("both sides moved together -- the gutter is unchanged")
    else:
        fail("a window vanished from the tree during the edge drag")

    # ------------------------------------------------------- modifier drag
    step("modifier-drag resize (cmd+alt inside a window)")
    frames = tiled_frames(space)
    ordered = sorted(frames.items(), key=lambda kv: kv[1][0])
    wid, box = ordered[0]
    inside = (box[0] + box[2] // 2, box[1] + box[3] // 2)
    note(f"holding cmd+alt and dragging from inside {wid}")
    sh(f"{MTOOL} drag {inside[0]} {inside[1]} {inside[0] + 150} {inside[1]} cmd+alt 30",
       timeout=40)
    settle(3)
    shot("after-modifier-drag")
    after = tiled_frames(space)
    if wid not in after:
        fail("the window left the tree during a modifier drag")
    elif after[wid] == box:
        fail(f"modifier-drag changed nothing: still {box}")
    else:
        note(f"{box} -> {after[wid]}")

    # ------------------------------------------------------------ stacking
    step("stack and unstack")
    sh(f"{CLI} execute workspace set-layout traditional"); settle()
    sh(f"{CLI} execute layout ascend"); settle(1.5)
    sh(f"{CLI} execute layout toggle-stack"); settle(2.5)
    shot("stacked")
    lay = q("layout", "--space-id", str(space))
    kinds = json.dumps(lay).count("_stack") if isinstance(lay, dict) else 0
    note(f"stacked containers in the tree: {kinds}")
    if kinds == 0:
        fail("toggle-stack produced no stacked container")
    sh(f"{CLI} execute layout toggle-stack"); settle(2)
    sh(f"{CLI} execute workspace set-layout bsp"); settle(2)
    shot("unstacked")

    # --------------------------------------------------------- fullscreen
    step("native fullscreen and back")
    before = tiled_frames(space)
    sh("open -a TextEdit"); settle(3)
    sh(f"{DTOOL} fullscreen"); settle(5)
    shot("fullscreen")
    if shown_spaces():
        note("the display still shows a desktop -- the key may not have landed")
    else:
        note("the display is showing a fullscreen space, as expected")
    sh("open -a TextEdit"); settle(2)
    sh(f"{DTOOL} fullscreen"); settle(6)
    shot("after-fullscreen")
    after = tiled_frames(space)
    if set(before) != set(after):
        fail(f"windows changed desktop across fullscreen: {set(before) ^ set(after)}")
    else:
        note("every window came back to the same desktop")

    # -------------------------------------------------------------- churn
    step("plug a display, then pull it out")
    sh(f"launchctl bootstrap gui/$(id -u) {HOME}/Library/LaunchAgents/vdisp.plist")
    settle(6)
    shot("plugged")
    note(f"displays now: {[(d.get('name'), d.get('space')) for d in (q('displays') or [])]}")
    sh("launchctl bootout gui/$(id -u)/vdisp"); settle(6)
    shot("unplugged")
    back = tiled_frames(space)
    note(f"{len(back)} tiled window(s) on desktop {space} after the round trip")
    if not back:
        fail("the desktop came back empty after a plug/unplug")

    print(f"\nscreenshots in {SHOTS}")
    if FAILURES:
        print(f"\n{len(FAILURES)} thing(s) went wrong:")
        for f in FAILURES:
            print(f"  - {f}")
        return 1
    print("\nnothing went wrong that this pass can see")
    return 0


if __name__ == "__main__":
    sys.exit(main())
