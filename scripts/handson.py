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


def window_frames(space):
    """Every window on `space`, tiled or floating."""
    out = {}
    for w in (q("windows", "--space-id", str(space)) or []):
        f = w.get("frame") or {}
        o, s = f.get("origin", {}), f.get("size", {})
        out[str(w.get("window_server_id"))] = (
            round(o.get("x", 0)), round(o.get("y", 0)),
            round(s.get("width", 0)), round(s.get("height", 0)))
    return out


def focus(w):
    wid = w.get("id") or {}
    if wid.get("pid") is None:
        return False
    arg = json.dumps({"pid": wid.get("pid"), "idx": wid.get("idx")})
    return "success" in sh(f"{CLI} execute window focus --window-id '{arg}'").lower()


def rift(*args):
    return q(*args)


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
    # A boundary only exists between two windows that are side by side *and*
    # overlap vertically. Sorting by x and taking the first two finds neither:
    # in a bsp spiral the two left-most windows are usually stacked one above
    # the other, and the "boundary" between them is a point in empty space.
    pair = None
    for a_id, a in frames.items():
        for b_id, b in frames.items():
            if a_id == b_id:
                continue
            side_by_side = abs(b[0] - (a[0] + a[2])) < 40 and b[0] > a[0]
            overlap = min(a[1] + a[3], b[1] + b[3]) - max(a[1], b[1])
            # The window that has to give up width must have width to give.
            # One already at its app minimum refuses, the boundary does not
            # move, and the result says nothing about what rift asked for.
            if side_by_side and overlap > 120 and a[2] > 420:
                pair = (a_id, a, b_id, b)
                break
        if pair:
            break
    if pair is None:
        fail("no two tiles share a vertical boundary; nothing to grab")
        return 1
    left_id, left, right_id, right = pair
    edge_x = left[0] + left[2]
    edge_y = max(left[1], right[1]) + min(left[1] + left[3], right[1] + right[3]) \
        - max(left[1], right[1])
    edge_y = max(left[1], right[1]) + (min(left[1] + left[3], right[1] + right[3])
                                       - max(left[1], right[1])) // 2
    target_x = edge_x + 120
    note(f"boundary at x={edge_x}, pushing it to {target_x} (growing the left tile)")
    sh(f"{MTOOL} drag {edge_x} {edge_y} {target_x} {edge_y} '' 30", timeout=40)
    settle(3)
    shot("after-edge-drag")
    after = tiled_frames(space)
    if left_id in after and right_id in after:
        new_left, new_right = after[left_id], after[right_id]
        moved = new_left[2] - left[2]
        note(f"left  {left[2]} -> {new_left[2]} (wider by {moved})")
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
    step("modifier-drag resize (the modifier's action2 button)")
    # `mouse.modifier` + action1 on the left button and action2 on the right,
    # and the config here maps action1=move, action2=resize -- so a resize test
    # that sends the left button is testing move and will report nothing.
    # Float it first: the modifier gestures are for floating windows, and on a
    # tiled one they correctly do nothing -- which reads as a failure if the
    # test never says which kind of window it is holding.
    frames = tiled_frames(space)
    wid, box = max(frames.items(), key=lambda kv: kv[1][2] * kv[1][3])
    target = next((w for w in (rift("windows", "--space-id", str(space)) or [])
                   if str(w.get("window_server_id")) == wid), None)
    if target is None:
        fail("could not find the window to float")
    else:
        if focus(target):
            sh(f"{CLI} execute window toggle-float")
            settle(3)
        floated = window_frames(space).get(wid)
        note(f"floated {wid}: {floated}")
        if floated is None:
            fail("the window vanished when floated")
        else:
            x, y, w, h = floated
            inside = (x + w * 3 // 4, y + h // 2)
            note(f"holding cmd+alt and right-dragging from inside {wid}")
            sh(f"{MTOOL} drag {inside[0]} {inside[1]} {inside[0] + 140} {inside[1]} "
               f"cmd+alt 30 right", timeout=40)
            settle(3)
            shot("after-modifier-drag")
            now = window_frames(space).get(wid)
            if now == floated:
                fail(f"modifier-drag changed nothing: still {floated}")
            else:
                note(f"{floated} -> {now}")
            if focus(target):
                sh(f"{CLI} execute window toggle-float")
                settle(3)

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
