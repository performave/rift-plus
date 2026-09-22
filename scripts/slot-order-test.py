#!/usr/bin/env python3
"""Does a display attach keep a desktop's leaf order, or rebuild it?

The reproduction for the reorder the matrix reported three times. It fires on
the first cycle: the attach rebuilds the tree -- the trace shows it growing
from `[]` to five leaves one window at a time -- and the order that comes out
is whatever insertion gave. The *detach* looks correct only because the
departure record restores it, which is why this went unexplained for so long.

Two things it has to do to mean anything, both learnt the hard way:

- Read the workspace id alongside the order. `query layout --space-id` answers
  for that desktop's *active* workspace, and a churn can change which one that
  is, so two readings from two different workspaces read as one workspace being
  reordered.
- Watch the desktop the windows are actually on, not the shown one. An attach
  switches the main display to a fresh desktop, so the shown one is empty.
"""
import sys, time, json
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, plug, unplug, settle, tile_all, is_tiled

def displays():
    return sorted(rift("displays") or [], key=lambda x: x["frame"]["origin"]["x"])

def busiest():
    best, n = None, -1
    for d in displays():
        for sp in (d.get("space_ids") or []):
            c = len([w for w in (rift("windows", "--space-id", str(sp)) or []) if is_tiled(w)])
            if c > n:
                best, n = sp, c
    return best

def leaves(space):
    """Leaf order, *with the workspace it came from*.

    `query layout --space-id` answers for that desktop's *active* workspace,
    and a churn can change which workspace is active. Without the id in the
    reading, two answers from two different workspaces read as one workspace
    being reordered -- which is a fixture bug wearing a layout bug's clothes,
    and the layout acts said plainly that no frame had moved."""
    out = sh(f"{CLI} query layout --space-id {space}")
    try:
        tree = json.loads(out)
    except Exception:
        return None
    acc = []
    def walk(n):
        if n.get("node_type") == "window":
            acc.append(n["window_id"]["idx"])
        for c in n.get("children") or []:
            walk(c)
    walk(tree.get("container_tree") or {})
    return (tree.get("workspace_id"), acc)

home = busiest()
tile_all(); time.sleep(1)
print("watching desktop", home)
base = leaves(home)
print("  base order:", base)
for cycle in (1, 2, 3, 4):
    plug(); settle(6)
    a = leaves(home)
    unplug(); settle(7)
    b = leaves(home)
    tag = ""
    if a != base:
        tag += f"  attached CHANGED {base} -> {a}"
    if b != base:
        tag += f"  detached CHANGED {base} -> {b}"
    print(f"  cycle {cycle}: attached {a} detached {b}{tag or '  (held)'}")
    if a != base or b != base:
        sh(f"{CLI} execute trace dump /Users/vm/rift-harness/slots-trace.json", timeout=90)
        print("  trace dumped")
        break
