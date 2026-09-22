#!/usr/bin/env python3
"""Spam a modifier-drag resize at a click rate and report whether it ratchets.

Windows are keyed by their id, not their app name: a desktop with three
TextEdits collapses to one entry otherwise, and the window being dragged
disappears into its namesakes. The target is chosen for being resizable and
having a neighbour to trade with -- System Settings, which will not resize at
all, silently turns the whole run into a no-op that looks like a pass.
"""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"

def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()

def tiled():
    try: ws = json.loads(sh(f"{B}/rift-cli query windows"))
    except Exception: return {}
    out = {}
    for w in ws:
        if w.get("is_floating"): continue
        f = w.get("frame") or {}; o, s = f.get("origin", {}), f.get("size", {})
        i = w.get("id") or {}
        out[(i.get("pid"), i.get("idx"))] = (
            w.get("app_name"), o.get("x", 0), o.get("y", 0),
            s.get("width", 0), s.get("height", 0))
    return out

start = tiled()
if len(start) < 2:
    print("need two tiled windows"); sys.exit(1)

# Group by row, then take the rightmost of the widest row: it has a neighbour.
rows = {}
for k, (app, x, y, w, h) in start.items():
    rows.setdefault(round(y / 50), []).append((k, app, x, y, w, h))
row = max(rows.values(), key=len)
if len(row) < 2:
    print("no row has two windows to trade between"); sys.exit(1)
row.sort(key=lambda r: r[2])
key, app, x, y, w, h = row[-1]
# Both directions have to stay on screen. Grabbing at 80% of a window that
# sits against the right edge puts the "grow" half of every gesture past the
# boundary, where the events are simply eaten -- which makes the shrink
# direction work and the grow direction not, and that asymmetry looks exactly
# like the ratchet being measured for. Pick an anchor with room either side.
AMP = 200.0
screen_right = max(rx + rw for _, _, rx, _, rw, _ in row)
px = min(x + w * 0.80, screen_right - AMP - 20.0)
px = max(px, x + 20.0)
py = y + h * 0.5
print(f"row: {[(r[1], round(r[4])) for r in row]}")
print(f"spamming {app} (id {key}) at x={px:.0f} (+/-{AMP:.0f} stays on screen,\n        right edge {screen_right:.0f}), 8 cycles/sec\n")

first = None
for burst in range(1, 6):
    # Re-aimed every burst. A fixed anchor falls outside the window as soon as
    # it shrinks, and the press then lands on the *neighbour*: the target stops
    # moving, which reads as a ratchet that has hit a floor. Three separate
    # readings of this run were artefacts of the harness before this.
    cur = tiled().get(key)
    if cur is None:
        print("target window vanished"); break
    _, cx, cy, cw, ch = cur
    px = min(cx + cw * 0.5, screen_right - AMP - 20.0)
    px = max(px, cx + 10.0)
    py = cy + ch * 0.5
    if not (cx <= px <= cx + cw):
        print(f"cannot aim inside a {cw:.0f}px window with +/-{AMP:.0f} of travel")
        break
    before = cw
    sh(f"{B}/mtool spam {px:.0f} {py:.0f} {AMP:.0f} 16 8 cmd+alt right")
    time.sleep(1.5)
    after = tiled().get(key, (None, 0, 0, 0, 0))[3]
    if first is None: first = before
    print(f"burst {burst}: grabbed at x={px:.0f} in [{cx:.0f}..{cx+cw:.0f}]  "
          f"{before:.0f} -> {after:.0f}   ({after-before:+.0f})")
last = tiled().get(key, (None, 0, 0, 0, 0))[3]
print(f"\nnet over 5 bursts of 16 alternating drags: {first:.0f} -> {last:.0f} "
      f"({last-first:+.0f})")
print("a gesture that alternates equally must net to zero; anything else ratchets")
