#!/usr/bin/env python3
"""Reproduce the drag bugs against an app that is slow to apply frames.

The point of `laggyapp` is that it blocks its main thread in bursts, so
accessibility writes queue behind it exactly as they do behind a heavy
renderer. TextEdit and Safari apply within a millisecond and reproduce none of
this, which is why every earlier reading from this harness was about the
harness.

Two things are measured, both objective, neither depending on eyeballing a
window:

  superseded   frames rift wrote that the app never applied before the next
               one replaced them. This is the backlog; it is what makes a
               window go on moving after the pointer stops.
  drift        where a gesture that alternates equally leaves the window.
               It must end where it began.
"""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"
def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
def rift(*a):
    try: return json.loads(sh(f"{B}/rift-cli query " + " ".join(a)))
    except Exception: return None

def windows():
    out = {}
    for w in (rift("windows") or []):
        if w.get("is_floating"): continue
        f = w.get("frame") or {}; o, s = f.get("origin", {}), f.get("size", {})
        i = w.get("id") or {}
        out[(i.get("pid"), i.get("idx"))] = (w.get("app_name"), o.get("x",0), o.get("y",0),
                                             s.get("width",0), s.get("height",0))
    return out

def superseded_count():
    sh(f"{B}/rift-cli execute trace dump /tmp/lag.trace")
    n = 0
    try:
        for line in open("/tmp/lag.trace", errors="replace"):
            if '"kind":"write_superseded"' in line: n += 1
    except Exception: pass
    return n

ws = windows()
lag = [(k, v) for k, v in ws.items() if v[0] and "laggy" in v[0].lower()]
if not lag:
    print("laggyapp is not tiled; start it and re-run"); sys.exit(1)
key, (app, x, y, w, h) = lag[0]
row = sorted([v for v in ws.values() if abs(v[2] - y) < 50], key=lambda r: r[1])
print(f"row: {[(r[0], round(r[3])) for r in row]}")
if len(row) < 2:
    print("laggyapp needs a neighbour to trade with"); sys.exit(1)

AMP = 200.0
right_edge = max(r[1] + r[3] for r in row)
base = superseded_count()
start = None
for burst in range(1, 5):
    cur = windows().get(key)
    if cur is None: print("window vanished"); break
    _, cx, cy, cw, ch = cur
    px = max(min(cx + cw * 0.5, right_edge - AMP - 20.0), cx + 10.0)
    if not (cx <= px <= cx + cw):
        print(f"cannot aim inside a {cw:.0f}px window"); break
    if start is None: start = cw
    sh(f"{B}/mtool spam {px:.0f} {cy + ch*0.5:.0f} {AMP:.0f} 16 8 cmd+alt right")
    time.sleep(2.0)
    after = windows().get(key, (None,0,0,0,0))[3]
    print(f"burst {burst}: grabbed x={px:.0f} in [{cx:.0f}..{cx+cw:.0f}]  "
          f"{cw:.0f} -> {after:.0f}  ({after-cw:+.0f})")
end = windows().get(key, (None,0,0,0,0))[3]
sup = superseded_count() - base
print(f"\ndrift over 4 bursts of 16 alternating drags: {start:.0f} -> {end:.0f} ({end-start:+.0f})")
print(f"frames written and never applied: {sup}")
