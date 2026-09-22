#!/usr/bin/env python3
"""A rearrangement made at one screen size must survive going back to another.

rift keeps a layout tree per screen size. Going back to a size it had seen
brought back the tree stored there as last left, undoing anything done since
at the other size. In real use the size changes when a display attaching as
main takes the Dock or menu bar off another, or when macOS moves a desktop onto
a display of a different size -- both real, neither controllable from a test:
in the guest macOS sometimes migrated the desktop and sometimes did not, and
left the Dock where it was, so a plug-based version of this reported "no size
change, nothing to test" more often than not.

A resolution change is the same code path with none of that: the same display,
the same desktop, a different size. So:

  1. go to size B and back to A -- both sizes now have a stored tree
  2. go to size B and swap two windows there
  3. go back to A, whose stored tree predates the swap

The swap must still be there. The desktop's size is checked at each step, not
assumed, and the original resolution is always restored.
"""
import sys, time, json
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, sh, rift, tile_all, is_tiled
from chaos import focus as chaos_focus

TOOL = CLI[:CLI.rindex("/")]
A = (1272, 1347)
B = (1024, 1084)

def shown():
    return (rift("displays") or [{}])[0].get("space")

def size_now():
    d = (rift("displays") or [{}])[0]
    return (round(d["frame"]["size"]["width"]), round(d["frame"]["size"]["height"]))

def setmode(wh):
    out = sh(f"{TOOL}/dtool setmode {wh[0]} {wh[1]}")
    # Wait for rift to see it, rather than guessing how long that takes.
    deadline, last = time.time() + 12, None
    while time.time() < deadline:
        time.sleep(0.7)
        now = size_now()
        if now == last:
            break
        last = now
    time.sleep(2.5)
    return out.strip()

def leaves(space):
    tree = json.loads(sh(f"{CLI} query layout --space-id {space}") or "{}")
    acc = []
    def walk(n):
        if n.get("node_type") == "window":
            acc.append(n["window_id"])
        for c in n.get("children") or []:
            walk(c)
    walk(tree.get("container_tree") or {})
    return tree.get("workspace_id"), acc

idx = lambda o: [w["idx"] for w in o]

def main():
    tile_all(); time.sleep(1)
    sp = shown()
    ws, base = leaves(sp)
    if len(base) < 3:
        print(f"FAIL setup: desktop {sp} has {len(base)} tiled window(s); need 3")
        return 2
    print(f"desktop {sp}, workspace {ws}, {len(base)} windows")
    try:
        print(" ", setmode(B)); size_b = size_now()
        print(" ", setmode(A)); size_a = size_now()
        print(f"  sizes: A={size_a} B={size_b}")
        if size_a == size_b:
            print("FAIL precondition: the desktop did not change size, so there is "
                  "only one tree and nothing to test")
            return 2
        print(" ", setmode(B))
        _, before = leaves(sp)
        a, b = before[0], before[-1]
        for w in rift("windows", "--space-id", str(sp)) or []:
            if w["id"] == a:
                chaos_focus(w); time.sleep(1)
                break
        sh(f"{CLI} execute layout swap-windows '{json.dumps(a)}' '{json.dumps(b)}'")
        time.sleep(1.5)
        ws_s, swapped = leaves(sp)
        print(f"  at B, before swap: {idx(before)}")
        print(f"  at B, after swap:  {idx(swapped)}")
        if swapped == before:
            print("FAIL setup: the swap did not take")
            return 2
        print(" ", setmode(A))
        ws_a, after = leaves(sp)
        print(f"  back at A:         {idx(after)}  (workspace {ws_a})")
        ok = after == swapped and ws_a == ws_s
        print(("PASS" if ok else "FAIL") + "  a swap made at one screen size "
              + ("survived going back to the other" if ok else "was undone by going back to the other"))
        return 0 if ok else 1
    finally:
        if size_now() != (A[0] - 56, A[1] - 31) and size_now() != A:
            setmode(A)

if __name__ == "__main__":
    sys.exit(main())
