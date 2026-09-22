#!/usr/bin/env python3
"""Moving a window between displays and between desktops, by hand.

Both of these have been reported broken and both resisted ad-hoc testing,
because the interesting configurations are fiddly to construct: a cross-display
drop needs a tiled window on *each* display, and a desktop move needs the index
macOS numbers desktops by, which is not the desktop id.

Two things this learned the hard way.

The index comes from a display's `space_ids`. `chaos.all_space_ids` lists each
display's active desktops before its inactive ones, and rift indexes by Mission
Control order, where the shown desktop sits wherever the user left it -- often
in the middle. The two agree only when the shown desktop happens to be first,
so an index computed the other way moves windows to the wrong desktop and the
test blames rift for obeying it.

And nothing here waits a fixed number of seconds. A move crosses the scripting
addition, the window server and rift's own event loop; on a guest still opening
apps, three seconds is sometimes enough and sometimes not, which produced two
runs of contradictory failures before it was the sleep that was suspected.
Every assertion below polls until it holds or the deadline passes.

What this does not cover: Mission Control's own drag. It is a system UI with no
scripting surface and no stable hit targets, so what is driven here is the
thing that drag ultimately asks for -- the window server putting a window on
another desktop -- and rift's reaction to it, which is the half that misbehaves.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, plug, unplug, settle, tile_all
from chaos import focus as chaos_focus

TOOL = CLI[:CLI.rindex("/")]

def displays():
    return sorted(rift("displays") or [], key=lambda d: d["frame"]["origin"]["x"])

def order():
    """Desktops in the order the space commands index them."""
    out = []
    for d in displays():
        out += [s for s in (d.get("space_ids") or []) if s not in out]
    return out

def index_of(space):
    o = order()
    return o.index(space) + 1 if space in o else None

def on_space(space):
    return rift("windows", "--space-id", str(space)) or []

def tiled(space=None):
    """Tiled windows on `space`, or on every display's shown desktop.

    Not `rift query windows`. With no `--space-id` that answers for the active
    *context* display only, so on a two-display guest it returns one display's
    windows and reports the other as empty -- which read as "the probe display
    took everything" until the second display was asked for by name."""
    if space is not None:
        return [w for w in on_space(space) if not w.get("is_floating")]
    out, seen = [], set()
    for d in displays():
        for w in on_space(d.get("space")):
            key = w["id"]["idx"]
            if key not in seen and not w.get("is_floating"):
                seen.add(key)
                out.append(w)
    return out

def frame(w):
    f = w["frame"]
    return (f["origin"]["x"], f["origin"]["y"], f["size"]["width"], f["size"]["height"])

def on(d, w):
    x, _, wd, _ = frame(w)
    dx, dw = d["frame"]["origin"]["x"], d["frame"]["size"]["width"]
    return dx <= x + wd / 2 <= dx + dw

def until(pred, seconds=12.0, step=0.5):
    """Poll `pred` until it is truthy. Returns what it last returned."""
    deadline = time.time() + seconds
    got = pred()
    while not got and time.time() < deadline:
        time.sleep(step)
        got = pred()
    return got

def report(name, ok, detail):
    print(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")
    return ok

def find(idx):
    for d in displays():
        for w in on_space(d.get("space")):
            if w["id"]["idx"] == idx:
                return w
    for s in order():
        for w in on_space(s):
            if w["id"]["idx"] == idx:
                return w
    return None

def focus(idx):
    """Focus by window record, not by bare index.

    `--window-id` takes `{"pid":..,"idx":..}` and rejects anything else, and
    the CLI's complaint goes to stdout where a fixture that only checks the
    next assertion never sees it. Passing a bare index made every move here
    fail with "there is no focused window to move" -- rift reporting, quite
    correctly, that nothing had been focused."""
    w = find(idx)
    if w is None:
        return False
    ok = chaos_focus(w)
    time.sleep(0.4)
    return ok

def where_is(idx, spaces):
    for s in spaces:
        if any(w["id"]["idx"] == idx for w in on_space(s)):
            return s
    return None

def move_to(idx, dest):
    """Send window `idx` to desktop `dest`, and wait for it to arrive."""
    di = index_of(dest)
    if di is None:
        return None, f"desktop {dest} is not in the indexed order"
    if not focus(idx):
        return None, f"could not focus window {idx}"
    out = sh(f"{CLI} execute space move-window {di}")
    if "success" not in (out or "").lower():
        return None, f"the command was refused: {out.strip()}"
    landed = until(lambda: any(w["id"]["idx"] == idx for w in on_space(dest)))
    return (di, None) if landed else (di, "never arrived")

# --------------------------------------------------------------- the tests

def test_space_move():
    """A window sent to another desktop must be there, and only there."""
    ds = displays()
    if not ds:
        return report("space move", False, "no displays")
    d = ds[0]
    here = d.get("space")
    others = [s for s in (d.get("space_ids") or []) if s != here]
    if not others:
        return report("space move", True, "only one desktop on this display; nothing to test")
    ws = tiled(here)
    if not ws:
        return report("space move", False,
                      f"no tiled window on the shown desktop {here}; nothing to move")
    w = ws[0]
    idx = w["id"]["idx"]
    dest = others[0]
    di, why = move_to(idx, dest)
    if why:
        return report("space move", False, f"window {idx} -> desktop {dest}: {why}")
    # Arriving is half of it. A window listed on both desktops is the failure
    # that looks like a success until you switch back and find a ghost.
    gone = until(lambda: not any(q["id"]["idx"] == idx for q in on_space(here)))
    if not gone:
        also = where_is(idx, [s for s in order() if s not in (here, dest)])
        return report("space move", False,
                      f"window {idx} reached desktop {dest} (index {di}) but is STILL "
                      f"listed on {here}" + (f" and on {also}" if also else ""))
    still_tiled = any(not q.get("is_floating") for q in on_space(dest)
                      if q["id"]["idx"] == idx)
    return report("space move", still_tiled,
                  f"window {idx} -> desktop {dest} (index {di}): arrived, left {here}, "
                  + ("still tiled" if still_tiled else "arrived FLOATING"))

def seed_both(A, Bd):
    """Get at least one tiled window onto each display's shown desktop."""
    for _ in range(3):
        a = [w for w in tiled() if on(A, w)]
        b = [w for w in tiled() if on(Bd, w)]
        if a and b:
            return a, b
        have, want = (a, Bd) if not b else (b, A)
        if len(have) < 2:
            # Nothing to spare. Pull one back from another desktop of the
            # display that has none -- a guest part-way through `setup` often
            # has its windows scattered, and that is not this test's subject.
            src_d = Bd if not b else A
            pool = [s for s in (src_d.get("space_ids") or []) if s != src_d.get("space")]
            got = None
            for s in pool[:6]:
                cand = [w for w in tiled(s) if not w.get("is_floating")]
                if cand:
                    got = cand[0]
                    break
            if not got:
                return a, b
            move_to(got["id"]["idx"], src_d.get("space"))
            continue
        move_to(have[-1]["id"]["idx"], want.get("space"))
    return [w for w in tiled() if on(A, w)], [w for w in tiled() if on(Bd, w)]

def test_cross_display_drag():
    """A tiled window carried to the other display must land there."""
    plug(); settle(4)
    ds = displays()
    if len(ds) < 2:
        return report("cross-display drag", False, "probe display did not attach")
    A, Bd = ds[0], ds[1]
    a, b = seed_both(A, Bd)
    if not a or not b:
        return report("cross-display drag", False,
                      f"could not seed both displays (A={len(a)}, B={len(b)}); untested")
    src, dst_d, tgt = (a[0], Bd, b[0]) if len(a) >= len(b) else (b[0], A, a[0])
    idx = src["id"]["idx"]
    x, y, wd, _ = frame(src)
    tx, ty, tw, th = frame(tgt)
    sh(f"{TOOL}/mtool drag {x+wd/2:.0f} {y+12:.0f} "
       f"{tx+tw*0.25:.0f} {ty+th*0.5:.0f} none 45")
    def crossed():
        now = [q for q in tiled() if q["id"]["idx"] == idx]
        return now[0] if now and on(dst_d, now[0]) else None
    landed = until(crossed, 10.0)
    now = [q for q in tiled() if q["id"]["idx"] == idx]
    if not now:
        return report("cross-display drag", False, f"window {idx} is no longer listed as tiled")
    nx, _, nw, _ = frame(now[0])
    dx, dw = dst_d["frame"]["origin"]["x"], dst_d["frame"]["size"]["width"]
    inside = nx >= dx - 40 and nx + nw <= dx + dw + 40
    # Both displays are named "Apple Virtual", so the name alone says nothing
    # about which one it landed on. Print the spans and let the numbers show it.
    return report("cross-display drag", bool(landed) and inside,
                  f"{src['app_name']} from [{A['frame']['origin']['x']:.0f}"
                  f"..{A['frame']['origin']['x']+A['frame']['size']['width']:.0f}] "
                  f"to [{dx:.0f}..{dx+dw:.0f}]: "
                  f"{'landed' if landed else 'did not cross'}, now x={nx:.0f}..{nx+nw:.0f} "
                  f"{'inside it' if inside else 'OUTSIDE it'}")

def ensure_windows(want=3):
    """Open enough windows on the shown desktop to have something to move.

    Depending on `chaos.setup` having left windows here does not survive
    repetition: this file moves windows to other desktops by design, so the
    second run starts poorer than the first and the third reported "nothing to
    move" -- which is the fixture running out of subjects, not rift failing.
    Opening its own is cheap and makes a run mean the same thing every time."""
    have = len(tiled(displays()[0].get("space")))
    if have >= want:
        return 0
    for i in range(1, want + 1):
        with open(f"/tmp/mv{i}.txt", "w") as fh:
            fh.write(f"move test {i}\n")
    sh("open -a TextEdit /tmp/mv1.txt /tmp/mv2.txt /tmp/mv3.txt")
    time.sleep(8)
    tile_all()
    time.sleep(1.5)
    # How many there are now, not how many more than before: tiling can also
    # gather one in, and a delta then prints "opened -1 windows".
    return len(tiled(displays()[0].get("space")))

def gather(limit=4):
    """Bring stray windows back onto the shown desktop before testing.

    Each run of this file sends a window to another desktop and the desktop
    move is the thing under test, so it cannot also be undone as cleanup
    without the cleanup deciding the verdict. Three runs in, the shown desktop
    was empty and both tests reported "nothing to move" -- a fixture failure
    wearing a product failure's clothes. Gathering at the start is honest about
    where the state came from and costs one move per stray window."""
    d = displays()[0]
    here = d.get("space")
    moved = 0
    for s in [s for s in (d.get("space_ids") or []) if s != here]:
        if len(tiled(here)) + moved >= limit:
            break
        for w in tiled(s):
            move_to(w["id"]["idx"], here)
            moved += 1
            break
    return moved

if __name__ == "__main__":
    tile_all(); time.sleep(1)
    back = gather()
    if back:
        print(f"gathered {back} stray window(s) back onto the shown desktop")
        tile_all(); time.sleep(1)
    opened = ensure_windows()
    if opened:
        print(f"{opened} window(s) on the shown desktop to test with")

    home = displays()[0].get("space")
    results = [test_space_move(), test_cross_display_drag()]
    unplug(quiet=True); settle(2)

    # Put back what the tests moved -- after both verdicts, never before one.
    # Restoring between the tests would let the cleanup decide the second
    # test's starting state, and leaving it undone makes every rerun start
    # from a worse guest than the last.
    strays = 0
    d = displays()[0]
    for sp in [x for x in (d.get("space_ids") or []) if x != d.get("space")][:8]:
        for w in tiled(sp):
            if move_to(w["id"]["idx"], d.get("space"))[1] is None:
                strays += 1
    if strays:
        print(f"put {strays} window(s) back on desktop {d.get('space')}")

    print(f"\n{sum(results)} of {len(results)} passed")
    sys.exit(0 if all(results) else 1)
