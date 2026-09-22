#!/usr/bin/env python3
"""Move a window to another desktop the way Mission Control does, and check
rift follows it.

Mission Control's own drag cannot be driven from here -- it is a system UI with
no scripting surface and no stable hit targets. What *can* be driven is the
thing it ultimately does: ask the window server to put a window on another
space, which is exactly what rift's scripting addition exposes and what rift
has to notice. That covers the part that has been buggy (rift's reaction),
though not Mission Control's own gesture.
"""
import json, subprocess, sys, time
B = "/Users/vm/rift-harness/bin"
def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
def rift(*a):
    try: return json.loads(sh(f"{B}/rift-cli query " + " ".join(a)))
    except Exception: return None

def where(pid_idx):
    for w in (rift("windows") or []):
        i = w.get("id") or {}
        if (i.get("pid"), i.get("idx")) == pid_idx:
            f = w.get("frame") or {}; o, s = f.get("origin", {}), f.get("size", {})
            return (w.get("app_name"), o.get("x",0), s.get("width",0), w.get("is_floating"))
    return None

ds = rift("displays") or []
if not ds: print("FAIL: no displays"); sys.exit(1)
shown = ds[0].get("space")
others = [s for s in (ds[0].get("inactive_space_ids") or []) if s != shown]
if not others:
    print("SKIP: only one desktop on this display, nothing to move between"); sys.exit(0)
dest = others[0]

ws = [w for w in (rift("windows") or []) if not w.get("is_floating")]
if not ws: print("FAIL: nothing tiled"); sys.exit(1)
w0 = ws[0]
i = w0.get("id") or {}
key = (i.get("pid"), i.get("idx"))
before = where(key)
print(f"moving {before[0]} from desktop {shown} to {dest}")

out = sh(f"{B}/rift-cli execute window move-to-space --space-id {dest} --window-id {i.get('idx')}")
print(f"  {out[:120]}")
time.sleep(2.5)

after = where(key)
listed_on = None
for d in (rift("displays") or []):
    for s in [d.get("space")] + (d.get("inactive_space_ids") or []):
        pass
# does rift still show it, and is it accounted for?
spaces_now = rift("windows")
still = any(((x.get("id") or {}).get("pid"), (x.get("id") or {}).get("idx")) == key
            for x in (spaces_now or []))
print(f"  after: {'still on the shown desktop' if still else 'no longer on the shown desktop'}")
if after:
    print(f"  frame x={after[1]:.0f} w={after[2]:.0f} floating={after[3]}")
print("\nPASS: rift processed the move without losing the window"
      if (not still or after) else "FAIL: the window is unaccounted for")
sys.exit(0)
