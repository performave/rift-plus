#!/usr/bin/env python3
"""Destroy every empty desktop on the guest's own display except the shown one.

macOS keeps desktops across reboots, and every churn scenario leaves some
behind, so the guest had accumulated about three hundred by the end of one
night of batteries -- and every run then happened in a heavier guest than the
last. The same build's score drifted with it, which is the last thing an A/B
can afford. `vm-ab` runs this after its reboot so every side starts from the
same handful of desktops.

Only desktops with no windows at all are touched, and never the one shown.
Guest only.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, rift, sh


def main():
    """Switch to each empty desktop and destroy it through rift.

    Destroying through the addition's socket directly does nothing for a
    desktop no display is showing -- Dock only removes the one in front -- so
    each is shown first and destroyed with `space destroy`, which acts on the
    desktop under the pointer. Slow the first time; each later run only has
    whatever the previous battery left behind.
    """
    ds = sorted(rift("displays") or [], key=lambda x: x["frame"]["origin"]["x"])
    if not ds:
        print("prune: no displays"); return 1
    d = ds[0]
    keep = d.get("space")
    before = len(d.get("space_ids") or [])
    f = d["frame"]
    cx = f["origin"]["x"] + f["size"]["width"] / 2
    cy = f["origin"]["y"] + f["size"]["height"] / 2
    tool = CLI[:CLI.rindex("/")]
    sh(f"{tool}/mtool move {cx:.0f} {cy:.0f}")
    # Which desktops are empty is decided once: nothing else moves windows
    # while this runs, and asking per desktop per pass was quadratic -- over
    # an hour for three hundred.
    initial = d.get("space_ids") or []
    empty = {s for s in initial if s != keep
             and not (rift("windows", "--space-id", str(s)) or [])}
    destroyed, stuck = 0, 0
    while empty:
        cur = next((x for x in rift("displays") or [] if x["uuid"] == d["uuid"]), None)
        ids = (cur or {}).get("space_ids") or []
        empty &= set(ids)
        if not empty:
            break
        target = max(empty, key=ids.index)
        sh(f"{CLI} execute space switch-to {ids.index(target) + 1}")
        time.sleep(0.6)
        if (next((x for x in rift("displays") or [] if x["uuid"] == d["uuid"]), {}) or {}).get("space") != target:
            stuck += 1
            if stuck > 5:
                break
            continue
        sh(f"{CLI} execute space destroy")
        time.sleep(0.8)
        empty.discard(target)
        destroyed += 1
    # Back to the desktop the windows are on.
    ids = (next((x for x in rift("displays") or [] if x["uuid"] == d["uuid"]), {}) or {}).get("space_ids") or []
    if keep in ids:
        sh(f"{CLI} execute space switch-to {ids.index(keep) + 1}")
        time.sleep(1)
    print(f"prune: {before} desktops, destroyed {destroyed} empty, {len(ids)} left")
    return 0

if __name__ == "__main__":
    sys.exit(main())
