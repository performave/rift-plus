#!/usr/bin/env python3
"""Plug and unplug the probe display several times and count what aftercare
sends home, and where every window ends.

Found after a `straggler-after-return` run: at every return aftercare sent one
window to a desktop made that cycle, and macOS put it back. The first cause --
aftercare not registering its sends -- is fixed; what is left to decide is
whether the resend still happens and whether it does any harm. This counts
`record_aftercare` acts per cycle and checks, after the last cycle, that every
window is tiled on a desktop a display is showing.

    aftercare-drift-test.py [cycles]

Run it straight after `vm-run run straggler-after-return`, which leaves the
guest in the state that triggered it. Guest only.
"""
import sys, time
sys.path.insert(0, "/Users/vm/rift-harness")
from chaos import CLI, all_space_ids, is_tiled, plug, rift, settle, sh, trace_acts, unplug


def shown():
    return {d.get("space") for d in rift("displays") or []}


def main():
    cycles = int(sys.argv[1]) if len(sys.argv) > 1 else 6
    seen = set()
    total = 0
    for n in range(1, cycles + 1):
        plug(); settle(6)
        unplug(); settle(6)
        sends = [a for a in trace_acts({"record_aftercare"}) if a[0] not in seen]
        seen.update(a[0] for a in sends)
        total += len(sends)
        print(f"  cycle {n}: aftercare sent {len(sends)}: {[a[2] for a in sends]}")
    plug(); settle(6)
    unplug(); settle(8)
    on = shown()
    bad = []
    # Unfiltered, `query windows` answers for the active desktop only.
    for space in all_space_ids(rift("displays")):
        for w in rift("windows", "--space-id", str(space)) or []:
            if w.get("app_name") not in ("Safari", "TextEdit"):
                continue
            tiled = is_tiled(w)
            if not tiled or space not in on:
                bad.append((w["id"]["idx"], w.get("app_name"), space,
                            "tiled" if tiled else "NOT tiled"))
    print(f"  shown desktops {sorted(s for s in on if s)}; aftercare sends over {cycles} cycles: {total}")
    if bad:
        print(f"FAIL  windows left untiled or off the shown desktops: {bad}")
        return 1
    if total > 1:
        print(f"FAIL  aftercare kept resending ({total} sends over {cycles} cycles)")
        return 1
    print("PASS  every window tiled on a shown desktop; aftercare did not keep resending")
    return 0


if __name__ == "__main__":
    sys.exit(main())
