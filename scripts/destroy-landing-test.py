#!/usr/bin/env python3
"""Deleting a desktop must not rearrange the desktop the display lands on.

Reported from real use: delete a space with the keybind, and the tiles on the
space landed on right after swap. The remap that carries a replaced desktop's
layout onto its replacement fired for a destroy as well -- a destroy looks the
same from a snapshot -- and overwrote the landing desktop's own tree.

The landing desktop is given an order the tree would never produce by itself
(its first and last windows swapped), so a rebuild cannot pass by accident.
"""
import json, sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, rift_exec, tile_all
from chaos import focus as chaos_focus

def shown():
    return (rift("displays") or [{}])[0].get("space")

def order():
    o = []
    for d in rift("displays") or []:
        o += [s for s in (d.get("space_ids") or []) if s not in o]
    return o

def leaves(space):
    t = json.loads(sh(f"{CLI} query layout --space-id {space}") or "{}")
    acc = []
    def walk(n):
        if n.get("node_type") == "window":
            acc.append(n["window_id"])
        for c in n.get("children") or []:
            walk(c)
    walk(t.get("container_tree") or {})
    return t.get("workspace_id"), acc

idx = lambda o: [w["idx"] for w in o]

def main():
    tile_all(); time.sleep(1)
    home = shown()
    ws, before = leaves(home)
    if len(before) < 3:
        print(f"FAIL setup: {len(before)} tiled window(s) on {home}; need 3"); return 2
    a, b = before[0], before[-1]
    for w in rift("windows", "--space-id", str(home)) or []:
        if w["id"] == a:
            chaos_focus(w); time.sleep(0.8); break
    sh(f"{CLI} execute layout swap-windows '{json.dumps(a)}' '{json.dumps(b)}'")
    time.sleep(1.5)
    _, arranged = leaves(home)
    if arranged == before:
        print("FAIL setup: the swap did not take"); return 2
    print(f"desktop {home}, arranged: {idx(arranged)}")

    known = set(order())
    rift_exec("space create"); time.sleep(3)
    made = [s for s in order() if s not in known]
    if not made:
        print("FAIL setup: space create made nothing (is the scripting addition loaded?)"); return 2
    target = made[0]
    sh(f"{CLI} execute space switch-to {order().index(target) + 1}"); time.sleep(3)
    if shown() != target:
        print(f"FAIL setup: could not switch to the new desktop {target} (showing {shown()})"); return 2
    print(f"on the new desktop {target}; destroying it")
    rift_exec("space destroy"); time.sleep(5)
    landed = shown()
    ws2, after = leaves(home)
    print(f"landed on {landed}; desktop {home} now: {idx(after)} (workspace {ws2})")
    if landed != home:
        print(f"note: landed on {landed}, not {home}")
    ok = after == arranged
    print(("PASS" if ok else "FAIL") + "  the desktop landed on "
          + ("kept its arrangement" if ok else "was rearranged by the delete"))
    return 0 if ok else 1

if __name__ == "__main__":
    sys.exit(main())
