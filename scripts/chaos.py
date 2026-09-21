#!/usr/bin/env python3
"""
rift display-churn chaos harness. Runs INSIDE the guest's Aqua login session.

Architecture note: this script is launched as a LaunchAgent in `gui/<uid>` so
that every rift-cli call is an ordinary subprocess in the right mach bootstrap
namespace. Driving it from an SSH session instead costs a launchd bootstrap per
command, which is far too slow for loops that tile a dozen windows.

A virtual display from CGVirtualDisplay lives exactly as long as the process
holding it, so plugging a monitor in is `launchctl bootstrap` and yanking it is
`launchctl bootout`. Verified against tests/traces/display_churn_abuse.trace:
that produces the same reconfiguration flags as real hardware
(MOVED|SET_MODE|ADD|ENABLED|SHAPE_CHANGED on attach,
REMOVE|DISABLED|SHAPE_CHANGED on detach). The one real-hardware pattern it does
NOT reproduce on its own is SET_MAIN, so `make_main()` forces that separately.

  chaos.py list
  chaos.py baseline
  chaos.py run [name ...]
"""
import json
import os
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
BIN = f"{HOME}/rift-harness/bin"
CLI = f"{BIN}/rift-cli"
DTOOL = f"{BIN}/dtool"
UID = os.getuid()
LAYOUT = f"{HOME}/.rift/layout.ron"
VDISP_PLIST = f"{HOME}/Library/LaunchAgents/vdisp.plist"

# Four windows, all of which resize freely. Calculator and Chess are
# effectively fixed-size: they refuse their slot and the overflow reads as rift
# overlapping windows. Four also keeps every slot far above any app minimum.
TEST_APPS = ["TextEdit", "Safari"]
CONFIG = f"{HOME}/.config/rift/config.toml"


class Violation(Exception):
    """An invariant broke."""


# ---------------------------------------------------------------- primitives

def sh(cmd: str, timeout: int = 20) -> str:
    try:
        r = subprocess.run(["/bin/bash", "-lc", cmd], capture_output=True,
                           text=True, timeout=timeout)
        return r.stdout.strip()
    except subprocess.TimeoutExpired:
        return ""


def rift(*args: str):
    raw = sh(f"{CLI} query " + " ".join(args))
    if not raw:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw


def rift_exec(*args: str) -> None:
    sh(f"{CLI} execute " + " ".join(args))


def display_count() -> int:
    try:
        return int(sh(f"{DTOOL} count"))
    except ValueError:
        return -1


# ------------------------------------------------------------ display plumbing

def write_vdisp_plist(width: int, height: int, serial: int) -> None:
    with open(VDISP_PLIST, "w") as fh:
        fh.write(f"""<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>Label</key><string>vdisp</string>
  <key>ProgramArguments</key><array>
    <string>{BIN}/vdisp</string><string>{width}</string>
    <string>{height}</string><string>{serial}</string>
  </array>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>/tmp/vdisp.out</string>
  <key>StandardErrorPath</key><string>/tmp/vdisp.err</string>
</dict></plist>""")


def plug(width: int = 1920, height: int = 1080, serial: int = 1) -> None:
    unplug(quiet=True)
    write_vdisp_plist(width, height, serial)
    sh(f"launchctl bootstrap gui/{UID} {VDISP_PLIST} 2>/dev/null; true")
    if not wait_for_displays(2):
        raise Violation(f"virtual display never attached ({sh('cat /tmp/vdisp.err')})")


def unplug(quiet: bool = False) -> None:
    sh(f"launchctl bootout gui/{UID}/vdisp 2>/dev/null; true")
    if not quiet and not wait_for_displays(1):
        raise Violation("virtual display never detached")


def wait_for_displays(want: int, deadline: float = 12.0) -> bool:
    end = time.time() + deadline
    while time.time() < end:
        if display_count() == want:
            return True
        time.sleep(0.2)
    return False


def make_main(display_id: int) -> None:
    """Move a display to the origin, which is what makes it the main display.

    Real hardware fires SET_MAIN on a replug when the external takes over the
    menu bar; a bare CGVirtualDisplay attach never does, so the display-order
    path stays untested unless we force it.
    """
    sh(f"{DTOOL} setmain {display_id}")


def settle(seconds: float = 4.0) -> None:
    time.sleep(seconds)


# --------------------------------------------------------- restoration modes

def set_displaced_windows(mode: str) -> None:
    """Switch `settings.displaced_windows` and let hot-reload pick it up.

    There is no runtime setter for this one, so the config file is the only
    lever. The three modes are genuinely different code paths -- `spaces`
    (default) keeps windows on their own desktops and moves them home through
    the scripting addition, `float` drops them onto the survivor untiled, and
    `tile` merges them into the survivor's tree as one cluster -- so a suite
    that only ever runs the default tests a third of the restoration logic.
    """
    with open(CONFIG) as fh:
        lines = fh.readlines()
    out, placed, in_settings = [], False, False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[settings]"):
            in_settings = True
            out.append(line)
            out.append(f'displaced_windows = "{mode}"\n')
            placed = True
            continue
        if in_settings and stripped.startswith("displaced_windows"):
            continue          # drop any previous value; ours is already in
        out.append(line)
    if not placed:
        out.insert(0, f'[settings]\ndisplaced_windows = "{mode}"\n')
    with open(CONFIG, "w") as fh:
        fh.writelines(out)
    time.sleep(3)             # hot_reload watches the directory


def current_displaced_mode() -> str:
    with open(CONFIG) as fh:
        for line in fh:
            if line.strip().startswith("displaced_windows"):
                return line.split("=", 1)[1].strip().strip('"')
    return "spaces (default)"


# ------------------------------------------------------------------- windows

def spawn_windows() -> None:
    for i in (1, 2, 3):
        with open(f"/tmp/doc{i}.txt", "w") as fh:
            fh.write(f"doc {i}\n")
    sh("open -a TextEdit /tmp/doc1.txt /tmp/doc2.txt /tmp/doc3.txt")
    for app in TEST_APPS[1:]:
        sh(f'open -a "{app}"')
    time.sleep(10)


def focus(w: dict) -> bool:
    """Focus one window. `--window-id` is required -- passing only
    `--window-server-id` is rejected outright, which fails silently in a loop
    and leaves every toggle landing on whatever happened to be focused."""
    wid = w.get("id") or {}
    pid, idx = wid.get("pid"), wid.get("idx")
    if pid is None or idx is None:
        return False
    arg = json.dumps({"pid": pid, "idx": idx})
    extra = ""
    if w.get("window_server_id") is not None:
        extra = f" --window-server-id {w['window_server_id']}"
    out = sh(f"{CLI} execute window focus --window-id '{arg}'{extra}")
    return "success" in out.lower()


def tile_all() -> int:
    """Tile every floating window. The config is tile-on-demand (its catch-all
    rule floats everything), so a layout only exists if we build one."""
    tiled = 0
    for w in (rift("windows") or []):
        if not w.get("is_floating"):
            continue
        if not focus(w):
            continue
        time.sleep(0.25)
        rift_exec("window toggle-float")
        time.sleep(0.25)
        tiled += 1
    time.sleep(1.5)
    return tiled


# ----------------------------------------------------------------- snapshots

def all_space_ids(displays) -> list:
    out = []
    for d in displays or []:
        for key in ("active_space_ids", "inactive_space_ids"):
            for sid in (d.get(key) or []):
                if sid not in out:
                    out.append(sid)
    return out


def window_map(displays) -> dict:
    """ident -> (space, app, tiled, frame). Per-desktop, because an unfiltered
    `query windows` only reports the active desktop -- the blindness that let a
    scattered-window bug pass as healthy."""
    located = {}
    for sid in all_space_ids(displays):
        for w in (rift("windows", "--space-id", str(sid)) or []):
            ident = w.get("window_server_id")
            if ident is None:
                wid = w.get("id") or {}
                ident = f"{wid.get('pid')}:{wid.get('idx')}"
            fr = w.get("frame") or {}
            located[str(ident)] = (
                sid, w.get("app_name") or "?", not w.get("is_floating"),
                (round(fr.get("origin", {}).get("x", 0)), round(fr.get("origin", {}).get("y", 0)),
                 round(fr.get("size", {}).get("width", 0)), round(fr.get("size", {}).get("height", 0))),
            )
    return located


def tree_shape(space_id) -> dict:
    """The structural skeleton of a layout: orientation and leaf order.

    Compared across a churn, this is what catches "the windows all came back
    but two of them swapped slots" and "the split flipped from vertical to
    horizontal" -- neither of which a window-count or grouping check sees.
    """
    lay = rift("layout", "--space-id", str(space_id))
    if not isinstance(lay, dict) or "container_tree" not in lay:
        # rift lists the desktop but has no layout to give for it -- a departed
        # display's archived desktop looks exactly like this. Distinguish it
        # from a real layout, or every unplug reads as "mode changed to None".
        return {"absent": True}

    def walk(node, depth=0):
        if not isinstance(node, dict):
            return None
        kids = node.get("children") or []
        entry = {
            "type": node.get("node_type"),
            "layout": node.get("layout_kind"),
            "depth": depth,
        }
        if node.get("window_id"):
            entry["window"] = f"{node['window_id'].get('pid')}:{node['window_id'].get('idx')}"
        entry["children"] = [c for c in (walk(k, depth + 1) for k in kids) if c]
        return entry

    return {
        "mode": lay.get("mode"),
        "tree": walk(lay.get("container_tree")),
        "floating": len(lay.get("floating_windows") or []),
    }


def leaf_order(shape: dict) -> list:
    """Left-to-right leaf sequence -- slot identity, independent of geometry."""
    out = []

    def walk(n):
        if not n:
            return
        if n.get("window"):
            out.append(n["window"])
        for c in n.get("children") or []:
            walk(c)

    walk(shape.get("tree"))
    return out


def stacks(shape: dict) -> list:
    """Stacked containers as (ordered member windows, axis).

    Only `layout_kind` ending in `_stack` is a stack. `role == "stack"` is
    master-stack's name for its secondary column, which is an ordinary split --
    treating it as a stack would report a stack that was never there.
    """
    found = []

    def walk(n):
        if not n:
            return
        kind = n.get("layout") or ""
        if kind.endswith("_stack"):
            members = [c.get("window") for c in (n.get("children") or []) if c.get("window")]
            if members:
                found.append((tuple(members), kind))
        for c in n.get("children") or []:
            walk(c)

    walk(shape.get("tree"))
    return sorted(found)


def check_stacks(before: dict, after: dict, phase: str) -> None:
    for sid, shape in before.items():
        if shape.get("absent") or sid not in after or after[sid].get("absent"):
            continue
        was, now = stacks(shape), stacks(after[sid])
        if was != now:
            raise Violation(f"{phase}: desktop {sid} stacks changed\n"
                            f"      before: {was}\n      after:  {now}")


def orientations(shape: dict) -> list:
    """Every container's split kind, in tree order."""
    out = []

    def walk(n):
        if not n:
            return
        if n.get("children"):
            out.append(n.get("layout"))
        for c in n.get("children") or []:
            walk(c)

    walk(shape.get("tree"))
    return out


def workspace_names(displays) -> dict:
    out = {}
    for sid in all_space_ids(displays):
        ws = rift("workspaces", "--space-id", str(sid))
        if isinstance(ws, list):
            out[sid] = sorted(str(w.get("name")) for w in ws)
    return out


def workspace_total(displays) -> int:
    return sum(len(v) for v in workspace_names(displays).values())


def snapshot(label: str = "") -> dict:
    displays = rift("displays") or []
    spaces = all_space_ids(displays)
    return {
        "label": label,
        "displays": displays,
        "cg_count": display_count(),
        "spaces": spaces,
        "windows": window_map(displays),
        "shapes": {sid: tree_shape(sid) for sid in spaces},
        "workspaces": workspace_names(displays),
        "workspace_total": workspace_total(displays),
        "layout_mtime": os.path.getmtime(LAYOUT) if os.path.exists(LAYOUT) else 0.0,
    }


def render_displays(snap) -> str:
    return " | ".join(
        f"{d.get('name','?')}(id={d.get('screen_id')} space={d.get('space')})"
        for d in snap["displays"]) or "(none)"


# ---------------------------------------------------------------- invariants

def grouping(located: dict, keep=None) -> set:
    by_space = {}
    for ident, rec in located.items():
        if keep is not None and ident not in keep:
            continue
        by_space.setdefault(rec[0], set()).add(ident)
    return {frozenset(g) for g in by_space.values() if g}


def check_windows(before: dict, after: dict, phase: str) -> None:
    lost = set(before) - set(after)
    if lost:
        names = ", ".join(sorted(before[i][1] for i in lost))
        raise Violation(f"{phase}: {len(lost)} window(s) on no desktop at all ({names})")
    survivors = set(before) & set(after)
    was, now = grouping(before, survivors), grouping(after, survivors)
    if was != now:
        def render(part, loc):
            return " | ".join(sorted("+".join(sorted(loc[i][1] for i in g)) for g in part))
        raise Violation(f"{phase}: windows that shared a desktop no longer do\n"
                        f"      before: {render(was, before)}\n"
                        f"      after:  {render(now, after)}")


def check_tiled_stayed_tiled(before: dict, after: dict, phase: str,
                             mode: str = "spaces") -> None:
    """A tiled window must not come back floating -- that is silent layout loss.

    Except under `displaced_windows = "float"`, where a window from the display
    that left is *supposed* to end up floating on the survivor. Flagging that
    would be reporting the feature as the bug.
    """
    if mode == "float":
        return
    fell_out = sorted(before[i][1] for i in (set(before) & set(after))
                      if before[i][2] and not after[i][2])
    if fell_out:
        raise Violation(f"{phase}: tiled window(s) came back floating: {', '.join(fell_out)}")


def check_slot_order(before: dict, after: dict, phase: str) -> None:
    """Windows may all survive and still have swapped places in the tree."""
    for sid, shape in before.items():
        if shape.get("absent") or sid not in after or after[sid].get("absent"):
            continue
        was, now = leaf_order(shape), leaf_order(after[sid])
        if was and set(was) == set(now) and was != now:
            raise Violation(f"{phase}: desktop {sid} kept every window but reordered "
                            f"their slots\n      before: {was}\n      after:  {now}")


def check_orientation(before: dict, after: dict, phase: str) -> None:
    for sid, shape in before.items():
        if shape.get("absent") or sid not in after or after[sid].get("absent"):
            continue
        was, now = orientations(shape), orientations(after[sid])
        if was and was != now:
            raise Violation(f"{phase}: desktop {sid} split orientation changed\n"
                            f"      before: {was}\n      after:  {now}")


def check_mode(before: dict, after: dict, phase: str) -> None:
    """Only compare desktops that have a real layout at both ends.

    A desktop whose display went away still gets listed, but has no layout to
    report; that is not a mode change and flagging it buries the real ones.
    """
    for sid, shape in before.items():
        if shape.get("absent") or sid not in after or after[sid].get("absent"):
            continue
        if shape.get("mode") and shape["mode"] != after[sid].get("mode"):
            raise Violation(f"{phase}: desktop {sid} layout mode changed "
                            f"{shape['mode']} -> {after[sid].get('mode')}")


def _rects(snap: dict):
    """Tiled windows as (ident, app, x, y, w, h), grouped by desktop."""
    by_space = {}
    for ident, (sid, app, tiled, fr) in snap["windows"].items():
        if tiled:
            by_space.setdefault(sid, []).append((ident, app, *fr))
    return by_space


def check_frames(snap: dict, phase: str, tolerance: int = 2) -> None:
    """Tiled windows must actually tile: no overlap, nothing degenerate.

    The tree can be structurally perfect while the frames on screen are piled
    on top of each other -- a valid `mode=bsp` with 8 leaves and alternating
    splits told us nothing about whether the pixels were right. This is the
    check that looks at the geometry rift actually produced.
    """
    for sid, rects in _rects(snap).items():
        for ident, app, x, y, w, h in rects:
            if w <= 1 or h <= 1:
                raise Violation(f"{phase}: desktop {sid}: {app} has a degenerate frame {w}x{h}")

        for i in range(len(rects)):
            ia, aa, ax, ay, aw, ah = rects[i]
            for j in range(i + 1, len(rects)):
                ib, ab, bx, by, bw, bh = rects[j]
                ox = min(ax + aw, bx + bw) - max(ax, bx)
                oy = min(ay + ah, by + bh) - max(ay, by)
                if ox > tolerance and oy > tolerance:
                    raise Violation(
                        f"{phase}: desktop {sid}: tiled windows overlap by {ox}x{oy}px -- "
                        f"{aa} at ({ax},{ay},{aw},{ah}) vs {ab} at ({bx},{by},{bw},{bh})")


def check_frames_within_display(snap: dict, phase: str, slack: int = 40) -> None:
    """A tiled window must sit on a display, not off the edge of the world."""
    bounds = []
    for d in snap["displays"]:
        fr = d.get("frame") or {}
        o, sz = fr.get("origin", {}), fr.get("size", {})
        bounds.append((o.get("x", 0), o.get("y", 0), sz.get("width", 0), sz.get("height", 0)))
    if not bounds:
        return
    for sid, rects in _rects(snap).items():
        for ident, app, x, y, w, h in rects:
            if not any(x >= bx - slack and y >= by - slack
                       and x + w <= bx + bw + slack and y + h <= by + bh + slack
                       for bx, by, bw, bh in bounds):
                raise Violation(f"{phase}: desktop {sid}: {app} at ({x},{y},{w},{h}) "
                                f"is not inside any display {bounds}")


def check_display_count(snap: dict, phase: str) -> None:
    if snap["cg_count"] >= 0 and len(snap["displays"]) != snap["cg_count"]:
        raise Violation(f"{phase}: rift lists {len(snap['displays'])} display(s), "
                        f"window server has {snap['cg_count']}")


def empty_shells(snap: dict) -> list:
    """Desktops rift still lists that hold nothing and have no layout.

    Each missed remap leaves one of these behind: macOS hands the returning
    display a brand-new desktop, the old one keeps its entry but loses its
    windows and its layout, and nothing ever reaps it. They accumulate one per
    churn cycle, which is what makes this worth counting rather than eyeballing.
    """
    occupied = {rec[0] for rec in snap["windows"].values()}
    return sorted(sid for sid, shape in snap["shapes"].items()
                  if sid not in occupied and shape.get("absent"))


def check_no_desktop_leak(base: dict, now: dict, phase: str) -> None:
    was, is_ = empty_shells(base), empty_shells(now)
    leaked = [sid for sid in is_ if sid not in was]
    if leaked:
        raise Violation(f"{phase}: leaked {len(leaked)} empty desktop(s) {leaked} "
                        f"-- was {len(was)}, now {len(is_)}")


def check_no_workspace_leak(base: dict, now: dict, phase: str) -> None:
    if now["workspace_total"] > base["workspace_total"]:
        raise Violation(f"{phase}: workspace leak {base['workspace_total']} -> "
                        f"{now['workspace_total']} (a missed remap orphans the old one)")


def check_full(base: dict, now: dict, phase: str) -> None:
    check_display_count(now, phase)
    check_frames(now, phase)
    check_frames_within_display(now, phase)
    check_windows(base["windows"], now["windows"], phase)
    check_tiled_stayed_tiled(base["windows"], now["windows"], phase,
                             current_displaced_mode())
    check_slot_order(base["shapes"], now["shapes"], phase)
    check_orientation(base["shapes"], now["shapes"], phase)
    check_mode(base["shapes"], now["shapes"], phase)


# ------------------------------------------------------------------ sampling

class Sampler:
    """Poll frames continuously through a churn and keep the worst thing seen.

    A before/after pair cannot see a layout that was wrong for half a second
    and then corrected itself -- and a glitch that brief is still a glitch the
    user watches happen. This samples throughout instead, so a transient
    overlap or a window thrown off-screen is caught even when the final state
    is clean.
    """

    def __init__(self, label: str):
        self.label = label
        self.worst: list = []
        self.samples = 0
        self._stop = False
        self._thread = None

    def _loop(self):
        """One cheap query per sample.

        A full snapshot costs a rift-cli call per desktop for windows and
        another for the layout -- twenty-odd round trips once churn has left a
        pile of desktops behind, which cannot finish inside the sampling
        interval and starves the scenario it is supposed to be watching. The
        unfiltered window list is one call and carries the frames, which is all
        an overlap check needs.
        """
        while not self._stop:
            try:
                ws = rift("windows") or []
                self.samples += 1
                rects = []
                for w in ws:
                    if w.get("is_floating"):
                        continue
                    fr = w.get("frame") or {}
                    o, sz = fr.get("origin", {}), fr.get("size", {})
                    rects.append((w.get("app_name") or "?",
                                  round(o.get("x", 0)), round(o.get("y", 0)),
                                  round(sz.get("width", 0)), round(sz.get("height", 0))))
                for i in range(len(rects)):
                    an, ax, ay, aw, ah = rects[i]
                    if aw <= 1 or ah <= 1:
                        self._note(f"{an} degenerate {aw}x{ah}")
                    for j in range(i + 1, len(rects)):
                        bn, bx, by, bw, bh = rects[j]
                        ox = min(ax + aw, bx + bw) - max(ax, bx)
                        oy = min(ay + ah, by + bh) - max(ay, by)
                        if ox > 2 and oy > 2:
                            self._note(f"{an} and {bn} overlap by {ox}x{oy}px")
            except Exception:
                pass
            time.sleep(0.5)

    def _note(self, text: str) -> None:
        if text not in self.worst:
            self.worst.append(text)

    def __enter__(self):
        import threading
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop = True
        if self._thread:
            self._thread.join(timeout=5)
        return False


# ----------------------------------------------------------------- scenarios

SCENARIOS = {}


def scenario(name, doc=""):
    def wrap(fn):
        fn.doc = doc
        SCENARIOS[name] = fn
        return fn
    return wrap


@scenario("plain-replug", doc="unplug, let the survivor absorb the desktop, replug")
def s_plain_replug(base):
    for i in (1, 2, 3):
        plug(); settle()
        attached = snapshot(f"c{i} attached")
        unplug(); settle()
        check_full(attached, snapshot(f"c{i} detached"), f"cycle {i} detached")
        plug(); settle()
        check_full(attached, snapshot(f"c{i} replugged"), f"cycle {i} replugged")
        unplug(); settle()
    final = snapshot("final")
    check_no_workspace_leak(base, final, "plain-replug")
    check_no_desktop_leak(base, final, "plain-replug")


@scenario("fast-churn", doc="plug/unplug faster than stabilization converges")
def s_fast_churn(base):
    write_vdisp_plist(1920, 1080, 1)
    for _ in range(10):
        sh(f"launchctl bootstrap gui/{UID} {VDISP_PLIST} 2>/dev/null; true")
        time.sleep(0.3)
        sh(f"launchctl bootout gui/{UID}/vdisp 2>/dev/null; true")
        time.sleep(0.3)
    unplug(quiet=True); settle(10)
    check_full(base, snapshot("after fast churn"), "fast-churn")
    final = snapshot("final")
    check_no_workspace_leak(base, final, "fast-churn")
    check_no_desktop_leak(base, final, "fast-churn")


@scenario("short-unplug", doc="unplug briefly enough macOS may reuse the space id")
def s_short_unplug(base):
    plug(); settle()
    a = snapshot("attached")
    unplug(quiet=True); time.sleep(1.0)
    plug(); settle()
    check_full(a, snapshot("replugged fast"), "short-unplug")
    unplug(); settle()


@scenario("different-monitor", doc="a different display identity returns on the same port")
def s_different_monitor(base):
    plug(serial=0x1); settle()
    a = snapshot("monitor A")
    unplug(); settle()
    plug(width=2560, height=1440, serial=0x99); settle()
    check_full(a, snapshot("monitor B"), "different-monitor")
    unplug(); settle()
    check_no_workspace_leak(base, snapshot("final"), "different-monitor")


@scenario("become-main", doc="the external takes over as main display, then leaves")
def s_become_main(base):
    plug(); settle()
    a = snapshot("attached")
    ext = next((d for d in a["displays"] if d.get("name") == "rift-vm-probe"), None)
    if not ext:
        raise Violation("external display not visible to rift")
    make_main(int(ext["screen_id"])); settle()
    check_full(a, snapshot("external is main"), "become-main")
    unplug(); settle(6)
    check_full(base, snapshot("main display left"), "become-main unplugged")


@scenario("churn-during-space-switch", doc="unplug while a space switch is in flight")
def s_churn_during_switch(base):
    plug(); settle()
    a = snapshot("attached")
    sh(f"{CLI} execute space switch right &")
    time.sleep(0.15)
    unplug(quiet=True); settle(10)
    check_full(a, snapshot("unplugged mid-switch"), "churn-during-space-switch")


@scenario("fullscreen-across-churn",
          doc="fullscreen a window, churn, unfullscreen -- slots must not swap")
def s_fullscreen_churn(base):
    plug(); settle()
    a = snapshot("attached tiled")
    rift_exec("window toggle-fullscreen"); settle(3)
    unplug(); settle()
    plug(); settle()
    rift_exec("window toggle-fullscreen"); settle(3)
    check_full(a, snapshot("after fullscreen round trip"), "fullscreen-across-churn")
    unplug(); settle()


@scenario("fullscreen-roundtrip", doc="fullscreen/unfullscreen with no churn at all")
def s_fullscreen_roundtrip(base):
    a = snapshot("tiled")
    for _ in range(3):
        rift_exec("window toggle-fullscreen"); settle(2.5)
        rift_exec("window toggle-fullscreen"); settle(2.5)
    check_full(a, snapshot("after fullscreen cycles"), "fullscreen-roundtrip")


@scenario("resolution-churn", doc="SET_MODE churn: no ADD/REMOVE, full churn machinery")
def s_resolution_churn(base):
    for w, h in ((1920, 1080), (2560, 1440), (1280, 800)):
        plug(width=w, height=h); settle(3)
        check_display_count(snapshot(f"{w}x{h}"), f"resolution {w}x{h}")
    unplug(); settle()
    check_full(base, snapshot("final"), "resolution-churn")


@scenario("stack-across-churn", doc="a stack keeps its members and axis across a churn")
def s_stack_churn(base):
    # bsp has no stacked containers -- `apply_stacking_to_parent_of_selection`
    # is a hard no-op there, and scrolling moves the window to the next column
    # instead. Only traditional (and the `stack` mode itself) can hold one.
    rift_exec("workspace set-layout traditional")
    settle(2)
    # toggle-stack acts on the selected *container*, not on a window, so the
    # selection has to be walked up to the parent split first -- without the
    # ascend the command is a no-op and the scenario silently tests nothing.
    rift_exec("layout ascend")
    settle(1)
    rift_exec("layout toggle-stack")
    settle(2)
    a = snapshot("stacked")
    if not any(stacks(sh) for sh in a["shapes"].values()):
        raise Violation("no stack was created -- toggle-stack is a no-op in this "
                        f"layout mode ({[sh.get('mode') for sh in a['shapes'].values()]})")
    plug(); settle()
    check_stacks(a["shapes"], snapshot("stack + display")["shapes"], "stack-across-churn attach")
    unplug(); settle()
    after = snapshot("stack after churn")
    check_stacks(a["shapes"], after["shapes"], "stack-across-churn detach")
    check_full(a, after, "stack-across-churn")
    rift_exec("layout toggle-stack")
    settle(2)


@scenario("stack-swallow", doc="displaced windows must not be absorbed into an existing stack")
def s_stack_swallow(base):
    plug(); settle()
    rift_exec("workspace set-layout traditional")
    settle(2)
    # toggle-stack acts on the selected *container*, not on a window, so the
    # selection has to be walked up to the parent split first -- without the
    # ascend the command is a no-op and the scenario silently tests nothing.
    rift_exec("layout ascend")
    settle(1)
    rift_exec("layout toggle-stack")
    settle(2)
    a = snapshot("stacked with display")
    before_stacks = {sid: stacks(sh) for sid, sh in a["shapes"].items()}
    unplug(); settle(6)
    after = snapshot("display gone")
    for sid, was in before_stacks.items():
        if sid not in after["shapes"] or after["shapes"][sid].get("absent"):
            continue
        now = stacks(after["shapes"][sid])
        for (members, kind) in now:
            grew = [w for (old, _) in was if set(members) > set(old) for w in members if w not in old]
            if grew:
                raise Violation(
                    f"stack-swallow: desktop {sid}: a stack absorbed the displaced "
                    f"window(s) {grew} -- the re-anchor split a neighbour that sits "
                    f"in a stack")


@scenario("orientation-portrait",
          doc="a tall display must not flip orientations -- nothing derives them from geometry")
def s_orientation_portrait(base):
    a = snapshot("before portrait")
    plug(width=1080, height=1920); settle(5)
    check_orientation(a["shapes"], snapshot("portrait attached")["shapes"],
                      "orientation-portrait")
    unplug(); settle()
    check_orientation(a["shapes"], snapshot("portrait gone")["shapes"],
                      "orientation-portrait detached")


@scenario("transient-glitch",
          doc="sample frames continuously through a churn, not just before and after")
def s_transient(base):
    with Sampler("churn") as sampler:
        plug(); settle()
        unplug(); settle()
        plug(); settle()
        unplug(); settle()
    if sampler.worst:
        raise Violation(f"transient glitch(es) seen in {sampler.samples} samples "
                        f"mid-churn, even though the settled state may be fine:\n      "
                        + "\n      ".join(sampler.worst[:3]))


@scenario("straggler-after-return",
          doc="a window on the external's NON-shown desktop is left behind by the return")
def s_straggler(base):
    """The Back pass declares itself finished before the window server has
    finished reassigning window->space membership, and then stops watching.

    `finish_pass` takes the record on Stage::Back, and every straggler guard
    (CHURN_SETTLE, PLACEMENT_AFTER_CHURN) reads through that record, so they all
    die with it -- the log says `windows_waited_for=0`. A window the window
    server re-homes a second later is then filed as an ordinary user action and
    never brought back.

    Three preconditions, all load-bearing:
      * the survivor has exactly one desktop with a tiled window, so macOS
        destroys it on unplug and rift has to mint a stand-in;
      * the external owns two desktops, is SHOWING an empty one, and holds the
        tiled pair on the other -- the straggler comes off the non-shown one;
      * the unplug lasts ~2.8s: long enough for a full Away pass, short enough
        that macOS is still migrating display_space_ids when Back completes.
    """
    plug(); settle()

    # `space create` acts on the display that owns the menu bar, so the
    # external has to be made main first or the extra desktop lands on the
    # survivor and the scenario tests nothing. setmain is permanent, so the
    # original is put back before leaving.
    before_main = None
    for line in sh(f"{DTOOL} list").splitlines():
        parts = line.split()
        if len(parts) > 3 and "main=1" in line:
            before_main = parts[0]
            break
    ext = next((d for d in (rift("displays") or [])
                if d.get("name") == "rift-vm-probe"), None)
    if not ext:
        raise Violation("external display not visible to rift")
    make_main(int(ext["screen_id"]))
    settle(3)

    rift_exec("space create")
    settle(3)
    rift_exec("space switch right")
    settle(3)
    a = snapshot("attached, external showing an empty desktop")

    external = [d for d in a["displays"] if d.get("name") == "rift-vm-probe"]
    if not external:
        raise Violation("external display not visible to rift")
    desktops = len(external[0].get("active_space_ids") or []) \
        + len(external[0].get("inactive_space_ids") or [])
    if desktops < 2:
        raise Violation(
            "external needs two desktops for this scenario and has "
            f"{desktops}. `space create` needs the scripting addition and acts "
            "on whichever display owns the menu bar -- check `rift sa status`.")

    with Sampler("straggler") as sampler:
        unplug(quiet=True)
        time.sleep(2.8)          # deliberately not settle(): the timing is the bug
        plug()
        settle(8.0)              # past pass_done and CHURN_SETTLE

    try:
        after = snapshot("returned")
        # The tell: a window that was on the external is now on the survivor.
        moved = []
        for ident, rec in after["windows"].items():
            if ident in a["windows"] and rec[0] != a["windows"][ident][0]:
                moved.append((rec[1], a["windows"][ident][0], rec[0]))
        if moved:
            raise Violation("straggler-after-return: window(s) left behind by "
                            "the return and never brought home: "
                            + ", ".join(f"{app} desktop {was} -> {now}"
                                        for app, was, now in moved))
        check_full(a, after, "straggler-after-return")
    finally:
        # setmain is permanent, so this has to run even when the assertion
        # above fires -- otherwise every later scenario measures geometry
        # against a display arrangement this one skewed.
        if before_main:
            make_main(int(before_main))
            settle(2)
    if sampler.worst:
        raise Violation("straggler-after-return: transient breakage mid-return:\n      "
                        + "\n      ".join(sampler.worst[:2]))


@scenario("long-absence", doc="stay away past GIVE_UP_ON_DISPLAY (120s)")
def s_long_absence(base):
    plug(); settle()
    a = snapshot("attached")
    unplug(); settle()
    time.sleep(130)
    plug(); settle(8)
    check_full(a, snapshot("back after long absence"), "long-absence")
    unplug(); settle()


# --------------------------------------------------------------------- main

def main() -> int:
    argv = sys.argv[1:]
    cmd = argv[0] if argv else "run"

    if cmd == "list":
        for n, fn in SCENARIOS.items():
            print(f"  {n:28} {fn.doc}")
        return 0

    if cmd == "setup":
        spawn_windows()
        n = tile_all()
        print(f"spawned apps, tiled {n} window(s)")
        snap = snapshot("after setup")
        print(f"displays: {render_displays(snap)}")
        print(f"windows : {len(snap['windows'])}")
        for sid, shape in snap["shapes"].items():
            print(f"  desktop {sid}: mode={shape.get('mode')} "
                  f"leaves={leaf_order(shape)} splits={orientations(shape)}")
        return 0

    if cmd == "thin":
        # Close windows down to N so every slot is far larger than any app's
        # minimum size. An app that refuses to shrink overflows into its
        # neighbour on its own, which looks exactly like rift mis-tiling -- with
        # 8 windows in a bsp spiral the deep slots are 46px tall, so that
        # confound has to be removed before an overlap means anything.
        want = int(argv[1]) if len(argv) > 1 else 4
        for app in ("Chess", "Font Book", "Contacts", "Dictionary", "Calculator"):
            ws = rift("windows") or []
            if len(ws) <= want:
                break
            for w in ws:
                if w.get("app_name") == app:
                    sh(f'osascript -e \'quit app "{app}"\' 2>/dev/null')
                    time.sleep(1.5)
                    break
        time.sleep(2)
        n = tile_all()
        snap = snapshot("thinned")
        print(f"now {len(snap['windows'])} window(s), re-tiled {n}")
        for sid, rects in sorted(_rects(snap).items()):
            for ident, app, x, y, w, h in sorted(rects, key=lambda r: (r[3], r[2])):
                print(f"    {app:14} ({x:5},{y:5}) {w:5}x{h:<5}")
        try:
            check_frames(snap, "thinned")
            print("  overlap check at rest: OK")
        except Violation as exc:
            print(f"  overlap check at rest: VIOLATION\n    {exc}")
        return 0

    if cmd == "frames":
        snap = snapshot("frames")
        print(f"displays: {render_displays(snap)}")
        for sid, rects in sorted(_rects(snap).items()):
            print(f"  desktop {sid}: {len(rects)} tiled")
            for ident, app, x, y, w, h in sorted(rects, key=lambda r: (r[3], r[2])):
                print(f"    {app:14} ({x:5},{y:5}) {w:5}x{h:<5}")
        for fn, label in ((check_frames, "overlap/degenerate"),
                          (check_frames_within_display, "within display")):
            try:
                fn(snap, "frames")
                print(f"  {label}: OK")
            except Violation as exc:
                print(f"  {label}: VIOLATION\n    {exc}")
        return 0

    if cmd == "matrix":
        # The same scenarios under each restoration mode. Any mode-specific
        # failure is a bug the default-only run cannot see.
        core = ["plain-replug", "short-unplug", "fast-churn", "fullscreen-across-churn"]
        wanted = argv[1:] or core
        grid = {}
        for mode in ("spaces", "float", "tile"):
            set_displaced_windows(mode)
            print(f"\n########## displaced_windows = {mode} ##########", flush=True)
            unplug(quiet=True); settle(2)
            # Re-tile between modes. `float` deliberately leaves windows
            # floating, so without this the next mode starts with an empty tree
            # and every one of its results is vacuous.
            retiled = tile_all()
            base = snapshot(f"baseline {mode}")
            if retiled:
                print(f"  (re-tiled {retiled} window(s) first)", flush=True)
            print(f"  baseline: {len(base['windows'])} window(s), "
                  f"{sum(1 for r in base['windows'].values() if r[2])} tiled", flush=True)
            for name in wanted:
                started = time.time()
                try:
                    SCENARIOS[name](base)
                    verdict, detail = "PASS", ""
                except Violation as exc:
                    verdict, detail = "FAIL", str(exc)
                except Exception as exc:
                    verdict, detail = "ERROR", f"{type(exc).__name__}: {exc}"
                finally:
                    unplug(quiet=True); settle(2)
                grid[(mode, name)] = (verdict, detail)
                print(f"  {verdict:6} {name:28} {time.time()-started:.0f}s"
                      + (f"\n         {detail}" if detail else ""), flush=True)
        print("\n=== matrix ===")
        print(f"{'scenario':30}" + "".join(f"{m:10}" for m in ("spaces", "float", "tile")))
        for name in wanted:
            print(f"{name:30}" + "".join(
                f"{grid[(m, name)][0]:10}" for m in ("spaces", "float", "tile")))
        return 0

    if cmd == "control":
        # Does leaf order hold still when NOTHING happens? If it drifts at rest,
        # every slot-order failure the suite reports is noise and the check is
        # worthless until that is understood.
        snaps = []
        for i in range(4):
            snaps.append(snapshot(f"t{i}"))
            print(f"t{i}: " + " ".join(
                f"[{sid}] {leaf_order(sh_)}" for sid, sh_ in snaps[-1]["shapes"].items()), flush=True)
            if i < 3:
                time.sleep(6)
        first = snaps[0]["shapes"]
        drift = 0
        for j, later in enumerate(snaps[1:], 1):
            for sid, shape in first.items():
                if sid in later["shapes"] and leaf_order(shape) != leaf_order(later["shapes"][sid]):
                    print(f"  DRIFT at t{j} on desktop {sid} with no churn at all")
                    drift += 1
        print("CONTROL: " + ("UNSTABLE AT REST -- slot-order check is unreliable"
                             if drift else "stable at rest -- slot-order check is meaningful"))
        return 0

    if cmd == "baseline":
        snap = snapshot("baseline")
        print(f"displays : {render_displays(snap)}  (cg={snap['cg_count']})")
        print(f"spaces   : {snap['spaces']}")
        print(f"windows  : {len(snap['windows'])}")
        for ident, rec in sorted(snap["windows"].items(), key=lambda kv: kv[1]):
            print(f"    space {rec[0]}: {rec[1]:14} tiled={rec[2]} frame={rec[3]}")
        for sid, shape in snap["shapes"].items():
            print(f"  desktop {sid}: mode={shape.get('mode')} "
                  f"leaves={leaf_order(shape)} splits={orientations(shape)}")
        print(f"workspaces: {snap['workspace_total']}")
        shells = empty_shells(snap)
        print(f"empty shell desktops: {len(shells)} {shells}")
        return 0

    names = argv[1:] if len(argv) > 1 else list(SCENARIOS)
    unknown = [n for n in names if n not in SCENARIOS]
    if unknown:
        print(f"unknown scenario(s): {', '.join(unknown)}", file=sys.stderr)
        return 2

    unplug(quiet=True); settle(2)
    base = snapshot("baseline")
    print(f"baseline: {render_displays(base)}")
    print(f"          {len(base['windows'])} window(s), "
          f"{sum(1 for r in base['windows'].values() if r[2])} tiled, "
          f"{base['workspace_total']} workspace(s)\n", flush=True)

    results = []
    for name in names:
        print(f"--- {name} ---", flush=True)
        started = time.time()
        try:
            SCENARIOS[name](base)
            res = (name, "PASS", f"{time.time()-started:.0f}s", "")
        except Violation as exc:
            res = (name, "FAIL", f"{time.time()-started:.0f}s", str(exc))
        except Exception as exc:
            res = (name, "ERROR", f"{time.time()-started:.0f}s", f"{type(exc).__name__}: {exc}")
        finally:
            unplug(quiet=True); settle(2)
        results.append(res)
        print(f"    {res[1]} ({res[2]})" + (f"\n    {res[3]}" if res[3] else ""), flush=True)

    print("\n=== summary ===")
    for name, status, took, _ in results:
        print(f"  {status:6} {name:28} {took}")
    failed = [r for r in results if r[1] != "PASS"]
    if failed:
        print(f"\n{len(failed)} of {len(results)} did not pass:")
        for name, status, _, detail in failed:
            print(f"\n  [{status}] {name}\n    {detail}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
