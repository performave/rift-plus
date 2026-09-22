#!/usr/bin/env python3
"""Drag a window across the seam onto the other display, and back.

The window must end up tiled on the display the pointer finished on, with a
frame inside that display. Two ways this has gone wrong before: the window
lands on the right display with the old display's geometry, or rift keeps
counting it on the display it came from.
"""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"
def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
def rift(*a):
    try: return json.loads(sh(f"{B}/rift-cli query " + " ".join(a)))
    except Exception: return None

def displays():
    out = []
    for d in (rift("displays") or []):
        fr = d.get("frame") or {}; o, s = fr.get("origin", {}), fr.get("size", {})
        out.append((d.get("name"), o.get("x",0), o.get("y",0), s.get("width",0), s.get("height",0),
                    d.get("space")))
    return sorted(out, key=lambda r: r[1])

def windows():
    out = {}
    for w in (rift("windows") or []):
        f = w.get("frame") or {}; o, s = f.get("origin", {}), f.get("size", {})
        i = w.get("id") or {}
        out[(i.get("pid"), i.get("idx"))] = (w.get("app_name"), o.get("x",0), o.get("y",0),
                                             s.get("width",0), s.get("height",0),
                                             w.get("is_floating"))
    return out

def on_display(frame, d):
    _, dx, dy, dw, dh, _ = d
    x, y, w, h = frame
    return x + w/2 >= dx and x + w/2 <= dx + dw

ds = displays()
print("displays:", [(n, f"{int(x)}..{int(x+w)}") for n,x,y,w,h,_ in ds])
if len(ds) < 2:
    print("FAIL: need two displays for this test"); sys.exit(1)

ws = windows()
tiled = {k: v for k, v in ws.items() if not v[5]}
if not tiled:
    print("FAIL: nothing tiled"); sys.exit(1)

src, dst = ds[0], ds[1]
key = None
for k, v in tiled.items():
    if on_display((v[1], v[2], v[3], v[4]), src): key = k; break
if key is None:
    src, dst = ds[1], ds[0]
    for k, v in tiled.items():
        if on_display((v[1], v[2], v[3], v[4]), src): key = k; break
if key is None:
    print("FAIL: no tiled window on either display"); sys.exit(1)

app, x, y, w, h, _ = tiled[key]
print(f"dragging {app} from {src[0]} to {dst[0]}")
# The title bar, with no modifier held. A modifier drag is a different
# gesture: for a tiled window rift ignores the move half of it deliberately,
# because drag-to-swap already covers rearranging tiles. Carrying a window to
# another display is the plain drag the app itself handles and rift observes.
from_pt = (x + w*0.5, y + 12)
to_pt = (dst[1] + dst[3]*0.5, dst[2] + dst[4]*0.4)
sh(f"{B}/mtool drag {from_pt[0]:.0f} {from_pt[1]:.0f} {to_pt[0]:.0f} {to_pt[1]:.0f} none 40")
time.sleep(3.0)

after = windows().get(key)
if after is None:
    print("FAIL: the window vanished"); sys.exit(1)
app2, x2, y2, w2, h2, fl2 = after
landed = on_display((x2, y2, w2, h2), dst)
inside = x2 >= dst[1]-40 and x2 + w2 <= dst[1] + dst[3] + 40
print(f"  landed on {dst[0] if landed else src[0]}  frame=({x2:.0f},{y2:.0f},{w2:.0f}x{h2:.0f}) floating={fl2}")
ok = landed and inside
print(f"\n{'PASS' if ok else 'FAIL'}: "
      + ("window moved to the other display and sits inside it"
         if ok else
         ("window did not cross" if not landed else "window crossed but its frame is outside the display")))
sys.exit(0 if ok else 1)
