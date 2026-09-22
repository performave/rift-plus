#!/usr/bin/env python3
"""8-9 CPS of modifier-drag resize on a window's right half, as reported."""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"
def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
def rift(*a):
    try: return json.loads(sh(f"{B}/rift-cli query " + " ".join(a)))
    except Exception: return None
def tiled():
    out=[]
    for w in (rift("windows") or []):
        if w.get("is_floating"): continue
        f=w.get("frame") or {}; o,s=f.get("origin",{}),f.get("size",{})
        out.append((w.get("app_name"), o.get("x",0), o.get("y",0), s.get("width",0), s.get("height",0)))
    return sorted(out, key=lambda r: r[1])

start = tiled()
if len(start) < 2:
    print("need two tiled windows"); sys.exit(1)
app, x, y, w, h = start[-1]
px, py = x + w*0.80, y + h*0.5
print(f"start: {[(a,round(ww)) for a,_,_,ww,_ in start]}")
print(f"spamming {app}'s right half at x={px:.0f}, 8 cycles/sec, +/-200px\n")

for burst in range(1, 6):
    before = {a: ww for a,_,_,ww,_ in tiled()}
    sh(f"{B}/mtool spam {px:.0f} {py:.0f} 200 16 8 cmd+alt right")
    time.sleep(1.5)
    after = {a: ww for a,_,_,ww,_ in tiled()}
    cur = tiled()
    tot = sum(ww for _,_,_,ww,_ in cur)
    overlap = ""
    if len(cur) >= 2:
        # do any two tiled windows on the same row overlap?
        for i in range(len(cur)):
            for j in range(i+1, len(cur)):
                ax,aw2 = cur[i][1], cur[i][3]; bx,bw2 = cur[j][1], cur[j][3]
                ay,ah = cur[i][2], cur[i][4]; by,bh = cur[j][2], cur[j][4]
                ox = min(ax+aw2, bx+bw2) - max(ax,bx); oy = min(ay+ah,by+bh) - max(ay,by)
                if ox > 2 and oy > 2: overlap = f"  OVERLAP {ox:.0f}x{oy:.0f}"
    print(f"burst {burst}: " + ", ".join(f"{a} {before.get(a,0):.0f}->{after.get(a,0):.0f}" for a in before) + overlap)
print("\nfinal:", [(a, round(ww)) for a,_,_,ww,_ in tiled()])
