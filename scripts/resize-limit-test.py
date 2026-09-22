#!/usr/bin/env python3
"""What a resize does once the window can go no further.

Push a tiled window's shared edge past the point where the layout stops
obeying, then come back *part* of the way inside the same gesture. Going all
the way back lands on the origin whatever the implementation does, which is why
`wiggle` could never see this; a partial return has exactly one right answer --
the width the pointer is asking for -- and any slack shows up as the difference.

The floor is measured first rather than assumed. A run that reports "three
gestures all ended at 480" has said nothing until 480 is known not to be the
app's own minimum, which is the reading that made this look worse than it is.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, tile_all, focus as chaos_focus

TOOL = CLI[:CLI.rindex("/")]
MODS = "cmd+alt"
# `action1` is the left button and `action2` the right, and the guest's config
# is yabai's: move on the left, resize on the right. A resize test that sends
# the left button drives *move*, which a tiled window ignores outright -- so
# every gesture reads as "the window did not resize" and the control fails too,
# which is the tell that the button is wrong rather than the limit.
BUTTON = "right"

def tiled():
    return [w for w in (rift("windows") or []) if not w.get("is_floating")]

def geom(idx):
    for w in tiled():
        if w["id"]["idx"] == idx:
            f = w["frame"]
            return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])
    return None

def settle(idx, seconds=2.5):
    """Wait for the width to stop changing, rather than sleeping a guess."""
    last, stable, deadline = None, 0, time.time() + seconds + 6
    while time.time() < deadline:
        g = geom(idx)
        w = g[2] if g else None
        if w is not None and w == last:
            stable += 1
            if stable >= 3:
                return w
        else:
            stable = 0
        last = w
        time.sleep(0.25)
    return last

def pick():
    """The rightmost window of the widest row: it has a neighbour to trade with."""
    ws = tiled()
    rows = {}
    for w in ws:
        f = w["frame"]
        rows.setdefault(round(f["origin"]["y"] / 50), []).append(w)
    row = max(rows.values(), key=len) if rows else []
    if len(row) < 2:
        return None
    row.sort(key=lambda w: w["frame"]["origin"]["x"])
    return row[-1]

def grab_point(g):
    """Just inside the window's left edge, vertically centred."""
    x, y, w, h = g
    return (x + 12, y + h / 2)

def main():
    tile_all(); time.sleep(1)
    target = pick()
    if target is None:
        print("FAIL  setup: no row has two tiled windows to trade between")
        return 1
    idx = target["id"]["idx"]
    chaos_focus(target); time.sleep(0.5)
    base = settle(idx)
    g = geom(idx)
    print(f"target: {target['app_name']} idx={idx} width={base:.0f} x={g[0]:.0f}")

    # 1. Where is the floor? Keep asking for less until asking stops working.
    #    A single gesture that happens to land where it aimed proves nothing
    #    about limits -- the first version of this asked for 339px, got 339px,
    #    and called it the floor.
    floor, asked = base, None
    for _ in range(6):
        g_i = geom(idx)
        px, py = grab_point(g_i)
        want_in = g_i[2] - 40          # aim 40px wide: nothing will grant it
        sh(f"{TOOL}/mtool path {px:.0f} {py:.0f} {want_in:.0f} {MODS} 30 {BUTTON}")
        got = settle(idx)
        asked = g_i[2] - want_in
        if floor is not None and abs(got - floor) < 4:
            floor = got
            break
        floor = got
    print(f"floor: {floor:.0f}px -- the last gesture asked for {asked:.0f}px and "
          f"{'was refused' if abs(floor - asked) > 4 else 'was granted'}")
    if abs(floor - base) < 4:
        print("FAIL  floor: the window never shrank at all; nothing below is meaningful")
        return 1

    # Put it back for a clean start.
    g2 = geom(idx)
    px2, py2 = grab_point(g2)
    sh(f"{TOOL}/mtool path {px2:.0f} {py2:.0f} -900 {MODS} 30 {BUTTON}")
    settle(idx)
    g3 = geom(idx)
    print(f"recovered to {g3[2]:.0f}px")

    # 2. The question. One gesture: out past the floor, back part of the way.
    #    Anchor from a width we know we can reach, so the arithmetic is honest.
    start_w = g3[2]
    px3, py3 = grab_point(g3)
    out = int(start_w - floor + 400)       # comfortably past the floor
    back = int(out * 0.4)                  # return 60% of the way
    if start_w - back <= floor:
        back = int(start_w - floor) // 2   # keep the answer above the floor
    sh(f"{TOOL}/mtool path {px3:.0f} {py3:.0f} {out},{back} {MODS} 16 {BUTTON}")
    got = settle(idx)
    want = start_w - back
    slack = want - got
    ok = abs(slack) <= 30
    print(f"{'PASS' if ok else 'FAIL'}  partial return: pressed at width {start_w:.0f}, "
          f"pushed +{out} past the floor ({floor:.0f}), came back to +{back}; "
          f"want {want:.0f}px, got {got:.0f}px, slack {slack:+.0f}px")

    # 3. The same turn-around that never reaches a limit, as the control. If
    #    this fails too, the fault is in turning around, not in the limit.
    g4 = geom(idx)
    px4, py4 = grab_point(g4)
    s4 = g4[2]
    sh(f"{TOOL}/mtool path {px4:.0f} {py4:.0f} 200,80 {MODS} 16 {BUTTON}")
    got2 = settle(idx)
    want2 = s4 - 80
    slack2 = want2 - got2
    ok2 = abs(slack2) <= 30
    print(f"{'PASS' if ok2 else 'FAIL'}  control turn-around (never hits the floor): "
          f"want {want2:.0f}px, got {got2:.0f}px, slack {slack2:+.0f}px")

    # 4. Repeat the partial return a few times. One sample on a guest that
    #    animates is a coin toss, and the fault this is looking for was
    #    intermittent enough to survive three earlier rounds of testing.
    worst = abs(slack)
    for trial in range(3):
        gi = geom(idx)
        if gi[2] - floor < 200:
            sh(f"{TOOL}/mtool path {grab_point(gi)[0]:.0f} {grab_point(gi)[1]:.0f} "
               f"-600 {MODS} 20 {BUTTON}")
            settle(idx)
            gi = geom(idx)
        sw = gi[2]
        pxi, pyi = grab_point(gi)
        o = int(sw - floor + 400)
        b = int(o * 0.35)
        if sw - b <= floor:
            b = int(sw - floor) // 2
        sh(f"{TOOL}/mtool path {pxi:.0f} {pyi:.0f} {o},{b} {MODS} 14 {BUTTON}")
        gw = settle(idx)
        sl = (sw - b) - gw
        worst = max(worst, abs(sl))
        print(f"   trial {trial+1}: pressed {sw:.0f}, out +{o}, back +{b}; "
              f"want {sw-b:.0f}, got {gw:.0f}, slack {sl:+.0f}")
    ok3 = worst <= 30
    print(f"{'PASS' if ok3 else 'FAIL'}  worst slack over four partial returns: {worst:.0f}px")

    # 5. The shape the complaint actually had: not one clean turn but a spasm.
    #    Several reversals, most of them well past the floor, ending somewhere
    #    reachable. Every leg but the last is slack the implementation has to
    #    not keep; the width at the end is decided by the last leg alone.
    gi = geom(idx)
    if gi[2] - floor < 400:
        pxi, pyi = grab_point(gi)
        sh(f"{TOOL}/mtool path {pxi:.0f} {pyi:.0f} -800 {MODS} 20 {BUTTON}")
        settle(idx)
        gi = geom(idx)
    sw = gi[2]
    pxi, pyi = grab_point(gi)
    end = int((sw - floor) * 0.4)
    far = int(sw - floor + 500)
    legs = [far, end + 120, far - 200, end + 300, far, end]
    sh(f"{TOOL}/mtool path {pxi:.0f} {pyi:.0f} {','.join(str(l) for l in legs)} "
       f"{MODS} 10 {BUTTON}")
    gw = settle(idx)
    sl = (sw - end) - gw
    ok4 = abs(sl) <= 40
    print(f"{'PASS' if ok4 else 'FAIL'}  six reversals, four of them past the floor: "
          f"pressed {sw:.0f}, ended asking +{end}; want {sw-end:.0f}, got {gw:.0f}, "
          f"slack {sl:+.0f}")

    return 0 if (ok and ok2 and ok3 and ok4) else 1

if __name__ == "__main__":
    sys.exit(main())
