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
# Kept across reboots, unlike /tmp; one flight-recorder dump per failed scenario.
TRACES = f"{HOME}/rift-harness/traces"

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


def trace_acts(kinds, path: str = "/tmp/chaos-trace.json") -> list:
    """Every flight-recorder entry of the given kinds, oldest first.

    The recorder is always on and rings; `execute trace dump` writes it out
    retroactively. It is the only way to assert on what rift *decided* rather
    than on the state the queries can still see afterwards -- a decision that
    was right and a decision that was wrong can leave the same snapshot when
    macOS happens to clean up after the wrong one.
    """
    sh(f"{CLI} execute trace dump {path}", timeout=60)
    out = []
    try:
        with open(path) as fh:
            for line in fh:
                if not line.startswith("Act "):
                    continue
                try:
                    entry = json.loads(line[4:])
                except ValueError:
                    continue
                if entry.get("kind") in kinds:
                    out.append((entry.get("ms"), entry.get("kind"), entry.get("detail")))
    except OSError:
        return []
    return out


def fullscreen_key(app: str, want: bool, window: dict = None, tries: int = 4) -> bool:
    """Put `app`'s front window into macOS's own fullscreen, or take it out.

    Not rift's `window toggle-fullscreen`: that keeps the window in the tree,
    while this is the green button, which moves the window to a space of its
    own and takes it out -- the path `fullscreen_slots.rs` exists to undo.

    `osascript` hangs indefinitely inside a LaunchAgent in the guest, for both
    `activate` and System Events keystrokes, and goes on hanging with every
    relevant TCC grant in place. `dtool fullscreen` posts Ctrl-Cmd-F with
    CGEventPost instead, underneath Apple Events. A posted key goes to whatever
    is frontmost and a LaunchAgent is not an app, so the app has to be brought
    up first -- and `open -a` does not reliably win that race, so this checks
    for what it asked for and asks again.

    The check is only made *after* the app has been brought up, because what
    can be observed is whether a display is showing a fullscreen space, not
    whether a given window is fullscreen. Fronting a different app switches
    away from the fullscreen space and makes the state look cleared while
    nothing has changed -- which is how a stuck Safari survived every attempt
    to clear it and poisoned the runs that followed.
    """
    for _ in range(tries):
        sh(f'open -a "{app}"')
        time.sleep(2.5)
        if fullscreen_state(app, window) == want:
            return True
        # Ask twice. Fronting an app that is already fullscreen does not always
        # take the display to its space within one read, and treating that as
        # "the key did not land" posts it again -- which toggles back out, then
        # in, leaving a trail of extra slot records for the assertions to trip
        # over.
        time.sleep(2.0)
        if fullscreen_state(app, window) == want:
            return True
        # `open -a` picks the app, not the window, and three TextEdit documents
        # make "the front window" a coin toss. rift's own focus names the one
        # meant, and has to be redone on every attempt because `open -a` moves
        # the front window back.
        if window is not None:
            focus(window)
            time.sleep(1.0)
        sh(f"{DTOOL} fullscreen")
        time.sleep(2.5)
    sh(f'open -a "{app}"')
    time.sleep(2.0)
    return fullscreen_state(app, window) == want


def cg_display_bounds() -> list:
    """Full CoreGraphics bounds per display, menu bar and dock included.

    rift's own `frame` is the *visible* frame, inset by both. A natively
    fullscreen window covers the whole screen, so it matches these and not
    those -- which is what makes this the one test for fullscreen that does
    not depend on the space being shown.
    """
    out = []
    for line in sh(f"{DTOOL} list").splitlines():
        parts = line.split()
        if len(parts) < 2 or "x" not in parts[1] or "@" not in parts[1]:
            continue
        size, origin = parts[1].split("@", 1)
        try:
            w, h = (float(v) for v in size.split("x", 1))
            x, y = (float(v) for v in origin.split(",", 1))
        except ValueError:
            continue
        out.append((x, y, w, h))
    return out


def fullscreen_windows(app: str = None) -> list:
    """Windows whose frame covers a whole display -- a supplement, not a test.

    It would be convenient if this could stand in for
    `displays_showing_fullscreen`, since that one only answers for a space some
    display is currently showing. It cannot: rift drops the windows of a
    fullscreen space from `query windows`, sometimes even while that space is
    shown, so a genuinely fullscreen app is frequently absent here. Measured on
    2026-09-21: Safari fullscreen and in front listed nothing at all.

    Nothing observable distinguishes "no app is fullscreen" from "an app is
    fullscreen on a space nothing is showing", which is the state a reboot
    restores. Fronting each app in turn is the only way to find out, so
    `reset_between_scenarios` clears unconditionally instead of asking first.
    """
    bounds = cg_display_bounds()
    if not bounds:
        return []
    found = []
    for w in (rift("windows") or []):
        if app is not None and w.get("app_name") != app:
            continue
        fr = w.get("frame") or {}
        o, sz = fr.get("origin", {}), fr.get("size", {})
        x, y = o.get("x", 0), o.get("y", 0)
        cw, ch = sz.get("width", 0), sz.get("height", 0)
        for bx, by, bw, bh in bounds:
            if abs(x - bx) <= 2 and abs(y - by) <= 2 \
                    and abs(cw - bw) <= 2 and abs(ch - bh) <= 2:
                found.append(w)
                break
    return found


def app_fullscreen(app: str) -> bool:
    return bool(fullscreen_windows(app)) or bool(displays_showing_fullscreen())


def window_fullscreen(window: dict):
    """Whether this one window is in a fullscreen space, from the window
    server; None when the window has no server id to ask about."""
    wsid = window.get("window_server_id") if window else None
    if wsid is None:
        return None
    return sh(f"{DTOOL} winfs {wsid}") == "1"


def fullscreen_state(app: str, window: dict = None) -> bool:
    """The window's own state when there is a window to ask about. The app-level
    reading answers for whichever of its windows the app put in front, and
    with two Safari windows that was the other one: a window still in
    fullscreen read as out of it, and the scenario measured a stuck window."""
    per_window = window_fullscreen(window)
    return app_fullscreen(app) if per_window is None else per_window


def display_count() -> int:
    try:
        return int(sh(f"{DTOOL} count"))
    except ValueError:
        return -1


# ------------------------------------------------------------ display plumbing

def write_vdisp_plist(width: int, height: int, serial: int, hidpi: bool = False,
                      label: str = "vdisp", path: str = None) -> None:
    extra = "<string>hidpi</string>" if hidpi else ""
    with open(path or VDISP_PLIST, "w") as fh:
        fh.write(f"""<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
    <string>{BIN}/vdisp</string><string>{width}</string>
    <string>{height}</string><string>{serial}</string>{extra}
  </array>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>/tmp/{label}.out</string>
  <key>StandardErrorPath</key><string>/tmp/{label}.err</string>
</dict></plist>""")


def plug(width: int = 1920, height: int = 1080, serial: int = 1, hidpi: bool = False) -> None:
    unplug(quiet=True)
    write_vdisp_plist(width, height, serial, hidpi)
    sh(f"launchctl bootstrap gui/{UID} {VDISP_PLIST} 2>/dev/null; true")
    if not wait_for_displays(2):
        raise Violation(f"virtual display never attached ({sh('cat /tmp/vdisp.err')})")


def unplug(quiet: bool = False) -> None:
    sh(f"launchctl bootout gui/{UID}/vdisp 2>/dev/null; true")
    if not quiet and not wait_for_displays(1 + len(EXTRA_MONITORS)):
        raise Violation("virtual display never detached")


# Monitors beyond the probe, by label: a dock with two of them, or the office
# and home monitors of a commute. Each is its own vdisp job with its own
# identity (serial), size and scale, so macOS keeps a separate arrangement and
# rift a separate record for each -- which is what makes them different
# monitors rather than the same one again.
EXTRA_MONITORS: dict = {}


def display_ids() -> list:
    return [int(line.split()[0]) for line in sh(f"{DTOOL} list").splitlines()
            if line.split() and line.split()[0].isdigit()]


def attach(label: str, width: int = 1920, height: int = 1080, serial: int = 0x40,
           hidpi: bool = False, wait: bool = True) -> int:
    """Plug in one more monitor; returns its display id (0 when not waited for)."""
    detach(label, quiet=True)
    before = set(display_ids())
    # In /tmp, not LaunchAgents: vm-ab refuses to run over stray agents there,
    # and a scenario that dies mid-way must not block the next battery.
    path = f"/tmp/{label}.plist"
    write_vdisp_plist(width, height, serial, hidpi, label=label, path=path)
    sh(f"launchctl bootstrap gui/{UID} {path} 2>/dev/null; true")
    EXTRA_MONITORS[label] = 0
    if label in MONITOR_SPECS:
        note_monitor_used(label, True)
    if not wait:
        return 0
    end = time.time() + 12
    while time.time() < end:
        new = set(display_ids()) - before
        if new:
            EXTRA_MONITORS[label] = new.pop()
            return EXTRA_MONITORS[label]
        time.sleep(0.2)
    raise Violation(f"monitor {label} never attached ({sh(f'cat /tmp/{label}.err')})")


def detach(label: str, quiet: bool = False, wait: bool = True) -> None:
    was = EXTRA_MONITORS.pop(label, None)
    count = display_count()
    sh(f"launchctl bootout gui/{UID}/{label} 2>/dev/null; true")
    sh(f"rm -f /tmp/{label}.plist")
    if was is None or quiet or not wait:
        return
    if not wait_for_displays(count - 1):
        raise Violation(f"monitor {label} never detached")


def detach_all_monitors() -> None:
    for label in list(EXTRA_MONITORS):
        detach(label, quiet=True)
    # And any a crashed run left behind: vdisp jobs other than the probe's.
    for line in sh(f"launchctl list | grep -o 'mon-[a-z0-9-]*'").splitlines():
        sh(f"launchctl bootout gui/{UID}/{line} 2>/dev/null; rm -f /tmp/{line}.plist")


def place(display: int, x: int, y: int) -> None:
    sh(f"{DTOOL} place {display} {x} {y}")


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


def probe_display(displays, before=None):
    """The probe display among `displays`: the one whose uuid is new.

    Found by name it went missing. rift reports the name macOS gives the
    screen, and in the guest both displays have come back as "Apple Virtual"
    -- sometimes, not always, so `become-main` and `clamshell` failed with
    "external display not visible to rift" in a run where `straggler-after-
    return` found it by the same name a few minutes later. A display that was
    not there before the plug is the probe whatever it is called. `before` is
    the display list from before the plug; without one, fall back to the name.
    """
    displays = displays or []
    if before is not None:
        old = {d.get("uuid") for d in before}
        new = [d for d in displays if d.get("uuid") not in old]
        if new:
            return new[0]
    return next((d for d in displays if d.get("name") == "rift-vm-probe"), None)


def settle(seconds: float = 4.0, converge: float = 10.0) -> None:
    """Wait out a display change: the fixed pause, then until rift agrees.

    The pause alone was a guess. A replug sometimes needs longer than four
    seconds for rift to see the display the window server already has, and a
    snapshot taken in between reported "rift lists 1 display(s), window server
    has 2" -- a reading of the moment, not a fault -- and every other check in
    that snapshot was then judging a one-display layout on a two-display
    machine. So after the pause, keep waiting (up to `converge` seconds) until
    the two counts agree. If they never do, the snapshot that follows says so,
    which is the honest outcome.
    """
    time.sleep(seconds)
    deadline = time.time() + converge
    while time.time() < deadline:
        want = display_count()
        if want < 0 or len(rift("displays") or []) == want:
            return
        time.sleep(0.5)


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

def shown_windows() -> list:
    """Every window on a desktop some display is showing.

    Unfiltered, `query windows` answers for the active display only. With the
    probe attached and active, tiling from it left the laptop's windows
    floating -- setup came back with two windows tiled instead of five.
    """
    out = []
    for d in rift("displays") or []:
        sid = d.get("space")
        if sid is not None:
            out.extend(rift("windows", "--space-id", str(sid)) or [])
    return out


def spawn_windows() -> None:
    for i in (1, 2, 3):
        with open(f"/tmp/doc{i}.txt", "w") as fh:
            fh.write(f"doc {i}\n")
    sh("open -a TextEdit /tmp/doc1.txt /tmp/doc2.txt /tmp/doc3.txt")
    for app in TEST_APPS[1:]:
        sh(f'open -a "{app}"')
    time.sleep(10)
    # Safari opens with however many windows it had when it last quit, so the
    # baseline came out at four windows on one run and five on the next, and
    # two sides of an A/B were measuring different layouts. Top it up to two.
    for _ in range(3):
        safari = [w for w in shown_windows() if w.get("app_name") == "Safari"]
        if len(safari) >= 2 or not safari:
            break
        focus(safari[0])
        time.sleep(0.5)
        sh(f"{DTOOL} key 45 cmd")
        time.sleep(3)


def scenario_start(name: str, base: dict) -> dict:
    """The state a scenario should be judged against: the one it starts in.

    `base` is taken once, before the first scenario. Judging the ninth
    scenario against it charges that scenario with everything the previous
    eight left behind -- which is how a run reported the same slot reordering
    three times over, once for the churn that caused it and twice for churns
    that merely inherited it. The reset above re-establishes the baseline as
    far as anything short of a reboot can; what it could not undo belongs in
    the record, not in the next scenario's verdict.
    """
    pre = snapshot(f"{name} pre")
    drift = []
    was = sum(1 for r in base["windows"].values() if r[2])
    now = sum(1 for r in pre["windows"].values() if r[2])
    if was != now:
        drift.append(f"{was} tiled -> {now}")
    if len(base["windows"]) != len(pre["windows"]):
        drift.append(f"{len(base['windows'])} window(s) -> {len(pre['windows'])}")
    try:
        check_frames_within_display(pre, "pre")
    except Violation as exc:
        drift.append(str(exc).split(": ", 1)[-1])
    if drift:
        print(f"    (residue the reset could not clear: {'; '.join(drift)})",
              flush=True)
    return pre


def reset_between_scenarios() -> str:
    """Put the guest back to the state the baseline was taken in.

    Without this a run's later scenarios inherit whatever the earlier ones left
    -- a skewed split ratio, a display arrangement `setmain` wrote permanently,
    windows parked on desktops nothing is showing, an app still fullscreen --
    and the results stop being about the build and start being about the order.
    Two passes of the same fifteen scenarios, same build, disagreed on six of
    them; that was this.

    A full guest reboot is the only thing that also clears the leaked desktops,
    but it costs three minutes a scenario. This is the cheap part: everything
    that can be undone without one.
    """
    notes = []
    # Anything else with a window changes the slot sizes every scenario is
    # measured against.
    if sh('pgrep -x "System Settings"'):
        sh('killall "System Settings" 2>/dev/null')
        notes.append("quit System Settings")
    detach_all_monitors()
    unplug(quiet=True)
    settle(2)
    # A fresh rift. The unplug above leaves a display record standing, and the
    # next scenario's first plug is then a return that puts back whatever the
    # last scenario left -- a window it moved to the external went back to it,
    # out of the stack the next scenario had just built round it, and
    # `stack-across-churn` failed on another scenario's residue. The layout
    # file goes too, or the restart would restore the same state. macOS keeps
    # its own memory of which display a window was on; that is not cleared,
    # and handling it is part of what is being measured.
    # Fetch before the restart, not after. Plugging a monitor in and out to
    # fetch its windows leaves rift a display record, and one left standing
    # into the scenario made the scenario's own monitor a stranger to it: its
    # return was not restored and a split came back flipped
    # (`monitor-moved`, `rearranged-while-attached`). The restart below
    # clears it.
    fetched = fetch_windows_left_on_the_probe() + fetch_windows_left_on_monitors()
    if fetched:
        notes.append(f"fetched {fetched} window(s) back from the probe's desktops")
    restart_rift()
    notes.append("rift restarted")
    gathered = gather_test_windows()
    if gathered:
        notes.append(f"gathered {gathered} window(s) onto one desktop")
    # Unconditionally, and it is worth the ten seconds. A fullscreen window on
    # a space nothing is showing cannot be detected at all: rift drops it from
    # `query windows` along with the rest of that space, so neither the shown-
    # space test nor the frame test sees anything. What *does* reveal it is
    # fronting the app, which is the first thing the clear does anyway --
    # so guarding the clear on a detector is guarding it on the one question
    # that cannot be answered before running it.
    if clear_native_fullscreen():
        notes.append("fullscreen clear ok")
    else:
        notes.append("STILL FULLSCREEN")
    for name, was, now in show_the_desktop_holding_the_windows():
        notes.append(f"{name}: showed empty {was}, switched to {now}")
    # `setmain` is permanent, so an earlier scenario's arrangement outlives it.
    for line in sh(f"{DTOOL} list").splitlines():
        parts = line.split()
        if len(parts) > 3 and "main=1" in line and parts[0] != "1":
            make_main(1)
            notes.append(f"main display was {parts[0]}, put back to 1")
            settle(2)
            break
    # Back to the baseline's layout mode. A scenario that switches mode and
    # fails before switching back hands every later one a different layout
    # system -- stack-across-churn's traditional, with its stack, failed the
    # three after it on stacked frames.
    for sid in shown_desktops(snapshot("reset")):
        mode = (tree_shape(sid) or {}).get("mode")
        if mode and mode != "bsp":
            rift_exec("workspace set-layout bsp")
            notes.append(f"desktop {sid} was {mode}, set back to bsp")
            settle(1.5)
    rift_exec("layout balance")
    settle(1.5)
    retiled = tile_all()
    if retiled:
        notes.append(f"re-tiled {retiled}")
    # Checked, not assumed. A scenario that fails can leave windows anywhere
    # -- floating on a desktop nobody shows, or on a monitor that is gone --
    # and one failed `commute` took the eight scenarios after it down with
    # "could not float a window": each reset came back with two of five
    # tiled and carried on. Short of the full set, start the apps over.
    tiled_now = sum(1 for w in shown_windows()
                    if w.get("app_name") in TEST_APPS and is_tiled(w))
    if tiled_now < 5:
        sh("killall Safari TextEdit 2>/dev/null")
        time.sleep(4)
        spawn_windows()
        clear_native_fullscreen()
        for name, was, now in show_the_desktop_holding_the_windows():
            notes.append(f"{name}: showed empty {was}, switched to {now}")
        rift_exec("layout balance")
        settle(1.5)
        n = tile_all()
        notes.append(f"only {tiled_now} tiled; relaunched the apps and tiled {n}")
    return "; ".join(notes)


def listed_test_windows() -> int:
    return sum(1 for sid in all_space_ids(rift("displays") or [])
               for w in rift("windows", "--space-id", str(sid)) or []
               if w.get("app_name") in ("Safari", "TextEdit"))


def fetch_windows_left_on_the_probe(expected: int = 5) -> int:
    """Bring back windows macOS is holding on the unplugged probe's desktops.

    A scenario that ends with a window on the external -- `straggler-after-
    return` does, on purpose -- leaves it on a desktop macOS keeps for the
    display while it is away and lists under no display. A running rift's
    record knows where it is; the rift the reset has just restarted does not,
    so the next scenario started one window short ("5 window(s) -> 4") and
    failed on the window turning up when the probe was plugged. Plug the probe,
    move its windows to the main display, unplug.
    """
    if listed_test_windows() >= expected:
        return 0
    plug(); settle(4)
    ds = rift("displays") or []
    main = next((d for d in ds if (d.get("frame") or {}).get("origin", {}).get("x") in (0, 0.0, 56, 56.0)), None)
    if main is None:
        unplug(quiet=True); settle(2)
        return 0
    moved = 0
    for d in ds:
        if d["uuid"] == main["uuid"]:
            continue
        for sid in (d.get("space_ids") or []):
            for w in rift("windows", "--space-id", str(sid)) or []:
                if w.get("app_name") not in ("Safari", "TextEdit"):
                    continue
                sh(f"{CLI} execute display move-window --uuid {main['uuid']} "
                   f"--window-id {w['id']['idx']}")
                time.sleep(1)
                moved += 1
    unplug(); settle(4)
    return moved


def bring_windows_to_the_laptop() -> int:
    """Move every test window on another display's desktops to the laptop's
    (display 1), with whatever monitors are attached right now."""
    ds = rift("displays") or []
    laptop = next((d for d in ds if int(d.get("screen_id", -1)) == 1), None)
    if laptop is None:
        return 0
    moved = 0
    for d in ds:
        if d["uuid"] == laptop["uuid"]:
            continue
        for sid in (d.get("space_ids") or []):
            for w in rift("windows", "--space-id", str(sid)) or []:
                if w.get("app_name") not in ("Safari", "TextEdit"):
                    continue
                sh(f"{CLI} execute display move-window --uuid {laptop['uuid']} "
                   f"--window-id {w['id']['idx']}")
                time.sleep(1)
                moved += 1
    return moved


# Every monitor a scenario plugs in, so the reset can go and fetch windows a
# failed run left on one: macOS keeps a monitor's desktops, and the windows on
# them, while it is away, and lists them under no display.
# Listed up front, not learnt as they attach: the run that left a window on
# one is usually a different process from the one that has to fetch it.
MONITOR_SPECS: dict = {
    "mon-office": dict(width=3440, height=1440, serial=0x52),
    "mon-home": dict(width=2560, height=1440, serial=0x51),
    "mon-swap": dict(width=2560, height=1440, serial=0x53),
    "mon-moved": dict(width=1920, height=1080, serial=0x54),
    "mon-rearrange": dict(width=1920, height=1080, serial=0x55),
    "mon-scale": dict(width=1920, height=1080, serial=0x56),
    "mon-dock-a": dict(width=1920, height=1080, serial=0x57),
    "mon-dock-b": dict(width=2560, height=1440, serial=0x58),
    "mon-pair-a": dict(width=1920, height=1080, serial=0x59),
    "mon-pair-b": dict(width=2560, height=1440, serial=0x5a),
    "mon-projector": dict(width=1920, height=1080, serial=0x5b),
}


# Which monitors have been plugged in since they were last emptied. Visiting
# all eleven on every reset cost three minutes a scenario whenever a window was
# unaccounted for -- usually one that was never on a monitor at all.
MONITORS_USED = f"{HOME}/rift-harness/monitors-used"


def monitors_used() -> list:
    try:
        with open(MONITORS_USED) as fh:
            return [l.strip() for l in fh if l.strip() in MONITOR_SPECS]
    except OSError:
        return []


def note_monitor_used(label: str, used: bool) -> None:
    labels = [l for l in monitors_used() if l != label] + ([label] if used else [])
    with open(MONITORS_USED, "w") as fh:
        fh.write("".join(f"{l}\n" for l in dict.fromkeys(labels)))


def fetch_windows_left_on_monitors(expected: int = 5) -> int:
    moved = 0
    for label in monitors_used():
        if listed_test_windows() >= expected:
            break
        try:
            attach(label, **MONITOR_SPECS[label])
        except Violation:
            continue
        settle(4)
        moved += bring_windows_to_the_laptop()
        detach(label, quiet=True)
        note_monitor_used(label, False)
        settle(4)
    return moved


def gather_test_windows() -> int:
    """Put every test window on one desktop of the main display, the one that
    already holds most of them, and show it.

    A scenario that ends with a window on another desktop -- a failure, or
    macOS keeping a window where the churn left it -- hands the next one a
    split baseline: `stack-across-churn` then found Safari alone on the shown
    desktop and the TextEdits on another, and built a stack of one. Returns
    how many windows it moved.
    """
    # A rift restarted a moment ago has not yet found the windows on desktops
    # nobody is showing. Gathering straight away saw only the shown desktop's,
    # moved nothing, and every scenario after `commute` ran with its three
    # TextEdits floating out of sight.
    deadline = time.time() + 12
    while listed_test_windows() < 5 and time.time() < deadline:
        time.sleep(1)
    ds = rift("displays") or []
    main = next((d for d in ds if (d.get("frame") or {}).get("origin", {}).get("x") in (0, 0.0, 56, 56.0)), None) \
        or (ds[0] if ds else None)
    if not main:
        return 0
    ids = (main.get("active_space_ids") or []) + (main.get("inactive_space_ids") or [])
    ids = main.get("space_ids") or ids
    where = {}
    for sid in all_space_ids(ds):
        for w in rift("windows", "--space-id", str(sid)) or []:
            if w.get("app_name") in ("Safari", "TextEdit"):
                where[str(w.get("window_server_id"))] = (sid, w)
    if not where:
        return 0
    counts = {}
    for sid, _ in where.values():
        if sid in ids:
            counts[sid] = counts.get(sid, 0) + 1
    if not counts:
        return 0
    target = max(counts, key=counts.get)
    f = main.get("frame") or {}
    o, sz = f.get("origin", {}), f.get("size", {})
    tool = CLI[:CLI.rindex("/")]
    sh(f"{tool}/mtool move {o.get('x', 0) + sz.get('width', 0) / 2:.0f} "
       f"{o.get('y', 0) + sz.get('height', 0) / 2:.0f}")
    moved = 0
    for sid, w in where.values():
        if sid == target:
            continue
        focus(w)
        time.sleep(0.8)
        sh(f"{CLI} execute space move-window {ids.index(target) + 1}")
        time.sleep(0.8)
        moved += 1
    sh(f"{CLI} execute space switch-to {ids.index(target) + 1}")
    time.sleep(1.5)
    return moved


def restart_rift() -> None:
    """Restart rift on a clean layout and wait until it answers again."""
    sh(f"launchctl bootout gui/{UID}/rift-harness 2>/dev/null; true", timeout=30)
    time.sleep(2)
    try:
        os.remove(LAYOUT)
    except OSError:
        pass
    sh(f"launchctl bootstrap gui/{UID} {HOME}/Library/LaunchAgents/rift-harness.plist "
       "2>/dev/null; true", timeout=30)
    deadline = time.time() + 30
    while time.time() < deadline:
        time.sleep(1)
        if rift("displays"):
            break
    settle(3)


def show_the_desktop_holding_the_windows() -> list:
    """Switch each display to a desktop that has tiled windows on it.

    Churn leaves windows on desktops nothing is showing -- which is a finding
    in its own right, and also the thing that makes the *next* scenario start
    with an empty tree and fail on its own precondition. A scenario cannot ask
    about restoration if there is nothing tiled in front of it, so the baseline
    goes and finds the windows. Returns what it had to switch, so a run says so
    rather than quietly papering over it.
    """
    switched = []
    for d in (rift("displays") or []):
        shown = d.get("space")
        listed = (d.get("active_space_ids") or []) + (d.get("inactive_space_ids") or [])
        if shown is not None and any(
            is_tiled(w)
            for w in (rift("windows", "--space-id", str(shown)) or [])
        ):
            continue
        for space in listed:
            if space == shown:
                continue
            if not any(is_tiled(w)
                       for w in (rift("windows", "--space-id", str(space)) or [])):
                continue
            order = all_space_ids(rift("displays") or [])
            if space not in order:
                continue
            sh(f"{CLI} execute space switch-to {order.index(space) + 1}")
            time.sleep(2)
            if any(x.get("space") == space for x in (rift("displays") or [])):
                switched.append((d.get("name"), shown, space))
            break
    return switched


def displays_showing_fullscreen() -> list:
    """Displays whose shown desktop is a native-fullscreen space.

    rift reports such a display as `space: null` with no active desktops, which
    is correct -- a fullscreen space is not a desktop it manages -- and reads
    exactly like the wedge where rift has lost track of the display set. The
    difference matters: one is cleared by taking the window out of fullscreen,
    the other only by rebooting the guest, and a whole afternoon can go into
    the wrong one.
    """
    return [d.get("name") for d in (rift("displays") or []) if d.get("space") is None]


def clear_native_fullscreen(tries: int = 3) -> bool:
    # One display only, or this cannot tell whose fullscreen it is seeing:
    # "some display shows fullscreen" stays true after fronting an app that is
    # not the one in fullscreen, and the corrective key then puts *that* app
    # into fullscreen instead. Callers that may have two displays attached
    # should unplug first, or name the app themselves with `fullscreen_key`.
    """Take every test app out of native fullscreen.

    Apps restore their saved window state, so a scenario that leaves Safari
    fullscreen leaves it fullscreen across relaunches and reboots too -- every
    later run then starts on a display with no desktop, tiles nothing, and
    reports an empty layout everywhere. Any baseline has to clear this first.

    Each app is brought up and asked about in turn, because fronting an app is
    what takes the display to that app's fullscreen space: an app nobody
    fronted is indistinguishable from an app that is not fullscreen at all.
    """
    for _ in range(tries):
        still = []
        for app in TEST_APPS:
            if not fullscreen_key(app, want=False, tries=2):
                still.append(app)
        if not still:
            return True
    return not still


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
    # The toggle acts on whatever is focused, and focus crossing to another
    # display lands a beat after the command reports success: a toggle meant
    # for a Safari window on the probe untiled a TextEdit on the laptop, the
    # next toggle tiled it again, and setup counted five while both Safari
    # windows stayed floating. So each toggle is checked, and a window is
    # only counted once rift reports it tiled.
    def floating_now(w):
        ident = w.get("id")
        return any(o.get("id") == ident and o.get("is_floating") for o in shown_windows())

    # A misdirected toggle can also untile a window this loop has already
    # passed, so go round again until nothing is left floating.
    done = set()
    for _pass in range(3):
        # Test apps only. A System Settings window that turned up during a
        # run was tiled as a sixth window, and six slots on this screen are
        # below what the apps accept: every overlap in that run was theirs.
        left = [w for w in shown_windows()
                if w.get("is_floating") and w.get("app_name") in TEST_APPS]
        if not left:
            break
        for w in left:
            for _ in range(3):
                if not focus(w):
                    break
                time.sleep(0.6)
                rift_exec("window toggle-float")
                time.sleep(0.4)
                if not floating_now(w):
                    done.add(json.dumps(w.get("id"), sort_keys=True))
                    break
    time.sleep(1.5)
    return len(done)


# ----------------------------------------------------------------- snapshots

def all_space_ids(displays) -> list:
    out = []
    for d in displays or []:
        for key in ("active_space_ids", "inactive_space_ids"):
            for sid in (d.get(key) or []):
                if sid not in out:
                    out.append(sid)
    return out


def is_tiled(w: dict) -> bool:
    """Whether a window is actually a leaf in its desktop's tree.

    Not `not is_floating`. `is_floating` says only whether the window is in the
    *floating* set, and a window can be in neither: one in native fullscreen,
    or on a desktop rift has not laid out, is in no tree and is not floating
    either. Counted as tiled, a fullscreen window is judged as overlapping the
    windows it left behind on its old desktop -- which it does, at the full
    size of the display, and entirely correctly.

    `is_tiled` is reported by rift itself as of the same change that added this.
    An older rift does not send it, and the fallback is the old reading, so a
    run against one degrades rather than crashing.
    """
    if "is_tiled" in w:
        return bool(w["is_tiled"])
    return not w.get("is_floating")


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
            mn = w.get("min_size") or {}
            located[str(ident)] = (
                sid, w.get("app_name") or "?", is_tiled(w),
                (round(fr.get("origin", {}).get("x", 0)), round(fr.get("origin", {}).get("y", 0)),
                 round(fr.get("size", {}).get("width", 0)), round(fr.get("size", {}).get("height", 0))),
                (round(mn.get("width", 0)), round(mn.get("height", 0))),
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


def stacked_groups(shape: dict) -> list:
    """Every window under each stacked container, nested ones included.

    `stacks` lists a stack's own windows, which is what its order is about.
    For overlap every window beneath the stack counts: a stack's children are
    each drawn at the stack's full size, and a split nested in one divides that
    same area -- so its windows sit over the other members by design. Stacking
    the root of a tree with a split in it makes exactly that.
    """
    groups = []

    def windows_under(n):
        out = [n["window"]] if n.get("window") else []
        for c in n.get("children") or []:
            out += windows_under(c)
        return out

    def walk(n):
        if not n:
            return
        if (n.get("layout") or "").endswith("_stack"):
            groups.append(windows_under(n))
            return
        for c in n.get("children") or []:
            walk(c)

    walk(shape.get("tree"))
    return groups


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
    # A display showing a native-fullscreen space reports no desktop at all.
    # Recorded rather than inferred: every structural check below sees that
    # display's desktops as absent, and the label is the only thing that says
    # why.
    fullscreen = [d.get("name") for d in displays if d.get("space") is None]
    spaces = all_space_ids(displays)
    return {
        "label": label,
        "displays": displays,
        "showing_fullscreen": fullscreen,
        "cg_count": display_count(),
        "spaces": spaces,
        "windows": window_map(displays),
        "shapes": {sid: tree_shape(sid) for sid in spaces},
        "workspaces": workspace_names(displays),
        "workspace_total": workspace_total(displays),
        "layout_mtime": os.path.getmtime(LAYOUT) if os.path.exists(LAYOUT) else 0.0,
    }


def render_displays(snap) -> str:
    def one(d):
        space = d.get("space")
        shown = "FULLSCREEN" if space is None else space
        return f"{d.get('name','?')}(id={d.get('screen_id')} space={shown})"

    return " | ".join(one(d) for d in snap["displays"]) or "(none)"


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

        # Which window, and on which desktop: two TextEdits read the same, and
        # "one of them moved" is not something a failure can be fixed from.
        def detail(loc):
            return ", ".join(f"{loc[i][1]} {i}{'' if loc[i][2] else ' (float)'}@{loc[i][0]}"
                             for i in sorted(survivors, key=lambda i: (str(loc[i][0]), i)))
        raise Violation(f"{phase}: windows that shared a desktop no longer do\n"
                        f"      before: {render(was, before)}\n"
                        f"      after:  {render(now, after)}\n"
                        f"      was: {detail(before)}\n"
                        f"      now: {detail(after)}")


def check_tiled_stayed_tiled(before: dict, after: dict, phase: str,
                             mode: str = "spaces") -> None:
    """A tiled window must not come back floating -- that is silent layout loss.

    Except under `displaced_windows = "float"`, where a window from the display
    that left is *supposed* to end up floating on the survivor. Flagging that
    would be reporting the feature as the bug.
    """
    if mode == "float":
        return
    fell_out = sorted(i for i in (set(before) & set(after))
                      if before[i][2] and not after[i][2])
    if fell_out:
        # Which, and where: "TextEdit" alone could not say whether rift lost
        # the window's leaf or the window sits on a desktop nobody shows.
        raise Violation(f"{phase}: tiled window(s) came back floating: "
                        + ", ".join(f"{before[i][1]} {i} (desktop {before[i][0]} -> {after[i][0]})"
                                    for i in fell_out))


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


def shown_desktops(snap: dict) -> set:
    """The desktops actually on a screen when the snapshot was taken."""
    return {d.get("space") for d in snap["displays"] if d.get("space") is not None}


def _rects(snap: dict, shown_only: bool = False):
    """Tiled windows as (ident, app, x, y, w, h), grouped by desktop.

    `shown_only` is what the geometry checks want, and getting this wrong is
    the single largest source of false failures this harness has produced.
    rift does not arrange a desktop no display is showing -- deliberately, and
    the frames there are simply the last ones applied, which after a churn are
    the ones from the display that has gone. Judged as geometry they read as
    windows stranded off the edge of the world and windows piled on top of each
    other, and they were reported as both for a long time.

    Measured: across six plug/unplug transitions, offences on *shown* desktops
    numbered zero, while a hidden desktop kept a window at x=2600 on a 56..2550
    display -- and switching to that desktop laid it out at x=61 within seconds,
    every time. The layout was never wrong; it had not been applied yet, which
    is not the same thing and is not a fault.
    """
    by_space = {}
    allowed = shown_desktops(snap) if shown_only else None
    for ident, rec in snap["windows"].items():
        sid, app, tiled, fr = rec[0], rec[1], rec[2], rec[3]
        if not tiled:
            continue
        if allowed is not None and sid not in allowed:
            continue
        mn = rec[4] if len(rec) > 4 else (0, 0)
        by_space.setdefault(sid, []).append((ident, app, *fr, mn))
    return by_space


def check_frames(snap: dict, phase: str, tolerance: int = 2) -> None:
    """Tiled windows must actually tile: no overlap, nothing degenerate.

    The tree can be structurally perfect while the frames on screen are piled
    on top of each other -- a valid `mode=bsp` with 8 leaves and alternating
    splits told us nothing about whether the pixels were right. This is the
    check that looks at the geometry rift actually produced.

    Only on the desktops being shown: see `_rects`.

    An overlap is now *classified* rather than just reported. An app given a
    slot smaller than the size it declares it will not go below renders at that
    minimum and spills into its neighbour -- which is the app obeying itself and
    rift obeying the tree, and it reads identically to a layout that overlaps
    windows. Every geometry finding here was ambiguous between the two until
    rift started reporting `min_size`. The violation now says which, so a run
    that trips on the confound is not filed as a layout fault.
    """
    for sid, rects in _rects(snap, shown_only=True).items():
        for ident, app, x, y, w, h, mn in rects:
            if w <= 1 or h <= 1:
                raise Violation(f"{phase}: desktop {sid}: {app} has a degenerate frame {w}x{h}")

        # Windows the tree holds in one stack share a frame by design. Not
        # exempting them made `stack-across-churn` fail exactly when the stack
        # came through the churn intact -- its only passes were runs where the
        # stack's desktop happened not to be the one shown.
        # The tree names windows `pid:idx`, the frame map by window-server
        # id; rift's idx is the window-server id, so both meet on the number.
        stacked = {}
        for n, members in enumerate(stacked_groups((snap.get("shapes") or {}).get(sid) or {})):
            for m in members:
                stacked[str(m).split(":")[-1]] = n

        for i in range(len(rects)):
            ia, aa, ax, ay, aw, ah, amn = rects[i]
            for j in range(i + 1, len(rects)):
                ib, ab, bx, by, bw, bh, bmn = rects[j]
                ka, kb = str(ia).split(":")[-1], str(ib).split(":")[-1]
                if ka in stacked and stacked.get(kb) == stacked[ka]:
                    continue
                ox = min(ax + aw, bx + bw) - max(ax, bx)
                oy = min(ay + ah, by + bh) - max(ay, by)
                if ox <= tolerance or oy <= tolerance:
                    continue
                # Which of the two is bigger than it was asked to be, and by
                # enough to account for the overlap?
                squeezed = []
                for name, (w_, h_), (mw, mh) in ((aa, (aw, ah), amn), (ab, (bw, bh), bmn)):
                    if mw and w_ >= mw - 1 and mw > 0 and ox <= mw:
                        squeezed.append(f"{name} declares a {mw}px minimum width")
                    if mh and h_ >= mh - 1 and mh > 0 and oy <= mh:
                        squeezed.append(f"{name} declares a {mh}px minimum height")
                why = ("  -- likely the app's own floor, not the layout's doing: "
                       + "; ".join(squeezed)) if squeezed else ""
                raise Violation(
                    f"{phase}: desktop {sid}: tiled windows overlap by {ox}x{oy}px -- "
                    f"{aa} at ({ax},{ay},{aw},{ah}) vs {ab} at ({bx},{by},{bw},{bh}){why}")


def check_frames_within_display(snap: dict, phase: str, slack: int = 40) -> None:
    """A tiled window must sit on a display, not off the edge of the world.

    Only on the desktops being shown: see `_rects`. A window on a hidden
    desktop wearing the frame the departed display gave it is the commonest
    thing in a churn snapshot and says nothing about rift.
    """
    bounds = []
    for d in snap["displays"]:
        fr = d.get("frame") or {}
        o, sz = fr.get("origin", {}), fr.get("size", {})
        bounds.append((o.get("x", 0), o.get("y", 0), sz.get("width", 0), sz.get("height", 0)))
    if not bounds:
        return
    for sid, rects in _rects(snap, shown_only=True).items():
        for ident, app, x, y, w, h, _mn in rects:
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
    """Empty shell desktops must not *accumulate*.

    Compared by count, not by id. A churn legitimately destroys a desktop rift
    made and mints another, so the ids of the empty ones turn over even when
    nothing is leaking; comparing ids reported "leaked 5 empty desktop(s)
    [295, 298, 301, 304, 307] -- was 249, now 249" on a run where the count had
    not moved at all. Growth is the thing this exists to catch.
    """
    was, is_ = empty_shells(base), empty_shells(now)
    if len(is_) > len(was):
        new = [sid for sid in is_ if sid not in was]
        raise Violation(f"{phase}: empty desktops grew {len(was)} -> {len(is_)} "
                        f"(new: {new})")


def check_no_workspace_leak(base: dict, now: dict, phase: str, patience: float = 12.0) -> None:
    """The workspace count must come back to where it started.

    Read once, at the end of a scenario, it caught the stand-in desktop rift
    makes at a departure before its scheduled destruction -- it is retired once
    nothing shows it, which is after the snapshot -- and reported +1. The same
    six plug/unplug cycles measured on their own ended exactly level (275 to
    275, one desktop renumbered). So a count above the start is re-read for a
    few seconds; one that stays up is a leak.
    """
    total = now["workspace_total"]
    deadline = time.time() + patience
    while total > base["workspace_total"] and time.time() < deadline:
        time.sleep(1.0)
        total = workspace_total(rift("displays") or [])
    if total > base["workspace_total"]:
        raise Violation(f"{phase}: workspace leak {base['workspace_total']} -> "
                        f"{total} (a missed remap orphans the old one)")


def check_no_limbo(phase: str) -> None:
    """No window on a shown desktop may be neither tiled nor floating.

    That is the state `window-limbo` describes: in no tree, not in the floating
    set, so nothing lays it out and the tile key does nothing for it. It sits
    at whatever size it last had -- in the churn runs, TextEdit's default
    673x439, which is how it kept turning up in overlap reports. Before rift
    reported `is_tiled`, the overlap check counted such a window as tiled and
    flagged it as an overlap; now that the overlap check correctly skips it,
    this is what keeps it from vanishing from view instead.

    A window in native fullscreen is legitimately in neither -- it has a whole
    display of its own -- so one that covers a display is left alone.
    """
    displays = rift("displays") or []
    frames = [(round(d["frame"]["origin"]["x"]), round(d["frame"]["origin"]["y"]),
               round(d["frame"]["size"]["width"]), round(d["frame"]["size"]["height"]))
              for d in displays]
    for d in displays:
        sid = d.get("space")
        if sid is None:
            continue
        for w in (rift("windows", "--space-id", str(sid)) or []):
            if "is_tiled" not in w:
                return      # an older rift: it cannot say, so neither can we
            if w.get("is_tiled") or w.get("is_floating"):
                continue
            f = w.get("frame") or {}
            o, sz = f.get("origin", {}), f.get("size", {})
            x, y = round(o.get("x", 0)), round(o.get("y", 0))
            wd, h = round(sz.get("width", 0)), round(sz.get("height", 0))
            covers = any(abs(x - fx) <= 60 and abs(wd - fw) <= 120 and h >= fh - 120
                         for fx, fy, fw, fh in frames)
            if covers:
                continue
            raise Violation(f"{phase}: desktop {sid}: {w.get('app_name')} "
                            f"#{(w.get('id') or {}).get('idx')} at ({x},{y},{wd},{h}) "
                            f"is neither tiled nor floating")


def live_frames() -> dict:
    """Window server id -> (x, y, w, h), for every window actually on screen."""
    live = {}
    for line in sh(f"{DTOOL} frames").splitlines():
        parts = line.split()
        if len(parts) == 5:
            live[parts[0]] = tuple(int(float(v)) for v in parts[1:])
    return live


def check_on_screen(snap: dict, phase: str, tolerance: int = 2,
                    floating_share: float = 0.97) -> None:
    """Every window a display is showing has to be on it -- floating ones too.

    Eric's report: windows turning up nearly outside the viewport when a
    monitor is connected again. `check_frames_within_display` could not have
    caught that. It reads rift's record of a frame, which is the last one the
    app reported, allows 40px of overhang, and skips floating windows --
    and Eric's config floats everything by default, so most of his windows
    are ones rift does not lay out and macOS moves on its own.

    So this reads the window server's frames, and holds a tiled window to
    lying inside one display and a floating one to having nearly all of
    itself on a single display: a float left hanging over the seam, or mostly
    off the edge, fails.
    """
    bounds = cg_display_bounds()
    if not bounds:
        return
    live = live_frames()
    shown = shown_desktops(snap)
    for ident, rec in snap["windows"].items():
        if rec[0] not in shown or ident not in live:
            continue
        x, y, w, h = live[ident]
        if w <= 1 or h <= 1:
            continue
        app, tiled = rec[1], rec[2]
        if tiled:
            if not any(x >= bx - tolerance and y >= by - tolerance
                       and x + w <= bx + bw + tolerance and y + h <= by + bh + tolerance
                       for bx, by, bw, bh in bounds):
                raise Violation(f"{phase}: tiled {app} at ({x},{y},{w},{h}) is not "
                                f"inside any display {bounds}")
        else:
            best = 0.0
            for bx, by, bw, bh in bounds:
                ix = max(0, min(x + w, bx + bw) - max(x, bx))
                iy = max(0, min(y + h, by + bh) - max(y, by))
                best = max(best, ix * iy / (w * h))
            if best < floating_share:
                raise Violation(f"{phase}: floating {app} at ({x},{y},{w},{h}) has only "
                                f"{best:.0%} of itself on any one display {bounds}")


def check_full(base: dict, now: dict, phase: str) -> None:
    check_display_count(now, phase)
    check_no_limbo(phase)
    check_on_screen(now, phase)
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

    Each overlapping pair is followed as an episode, from the first sample
    that shows it to the first that does not, so a run reports how long every
    overlap lasted and not only that one was seen.
    """

    INTERVAL = 0.1

    def __init__(self, label: str):
        self.label = label
        self.worst: list = []
        self.samples = 0
        self.episodes: list = []   # (description, seconds)
        self._open: dict = {}      # pair -> (first seen, description)
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
                # Where the windows are, from the window server -- not rift's
                # `frame`, which is rift's record and keeps a window's last
                # reported frame until the app answers rift's next write.
                live = {}
                for line in sh(f"{DTOOL} frames").splitlines():
                    parts = line.split()
                    if len(parts) == 5:
                        live[parts[0]] = tuple(int(float(v)) for v in parts[1:])
                now = time.time()
                self.samples += 1
                rects = []
                for w in ws:
                    if not is_tiled(w):
                        continue
                    frame = live.get(str(w.get("window_server_id")))
                    if frame is None:
                        continue
                    rects.append((str(w.get("window_server_id")), w.get("app_name") or "?", *frame))
                seen = {}
                for i in range(len(rects)):
                    ia, an, ax, ay, aw, ah = rects[i]
                    if aw <= 1 or ah <= 1:
                        seen[(ia,)] = f"{an} degenerate {aw}x{ah}"
                    for j in range(i + 1, len(rects)):
                        ib, bn, bx, by, bw, bh = rects[j]
                        ox = min(ax + aw, bx + bw) - max(ax, bx)
                        oy = min(ay + ah, by + bh) - max(ay, by)
                        if ox > 2 and oy > 2:
                            # Ids and frames: "TextEdit and TextEdit" could
                            # not say which pair, or whether a float was among
                            # them.
                            seen[tuple(sorted((ia, ib)))] = (
                                f"{an} {ia} ({ax},{ay} {aw}x{ah}) and {bn} {ib} "
                                f"({bx},{by} {bw}x{bh}) overlap by {ox}x{oy}px")
                for key, text in seen.items():
                    self._note(text)
                    if key not in self._open:
                        self._open[key] = (now, text)
                for key in [k for k in self._open if k not in seen]:
                    first, text = self._open.pop(key)
                    # When, from the sampler's start: lines up with a trace.
                    self.episodes.append((f"{text} from +{first - self._started:.1f}s", now - first))
            except Exception:
                pass
            time.sleep(self.INTERVAL)

    def _note(self, text: str) -> None:
        if text not in self.worst:
            self.worst.append(text)

    def longest(self) -> float:
        return max((d for _, d in self.episodes), default=0.0)

    # How long an overlap may last before it is rift's. Measured on v25 over
    # five runs of `transient-glitch`, every episode was over within 0.64s:
    # macOS moving windows to where they last were on an arriving display, a
    # beat before rift hears of it, and apps taking a couple of hundred ms to
    # apply a new layout. Neither is anything rift can shorten safely (writing
    # during the display change moved a window onto the wrong display), and
    # any overlap still there after a second is rift leaving a layout wrong.
    PERSIST = 1.0

    def persisted(self) -> list:
        return [(t, d) for t, d in self.episodes if d >= self.PERSIST]

    def __enter__(self):
        import threading
        self._started = time.time()
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop = True
        if self._thread:
            self._thread.join(timeout=5)
        now = time.time()
        for first, text in self._open.values():
            self.episodes.append((text + " (still at the end)", now - first))
        self._open.clear()
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
    ext = probe_display(a["displays"], base["displays"])
    if not ext:
        raise Violation("external display not visible to rift")
    make_main(int(ext["screen_id"])); settle()
    try:
        check_full(a, snapshot("external is main"), "become-main")
        unplug(); settle(6)
        check_full(base, snapshot("main display left"), "become-main unplugged")
    finally:
        # Hand the menu bar back while the probe is attached. setmain is
        # permanent and macOS remembers the arrangement for this display set,
        # so left alone every later plug brought the probe up as main and
        # every later scenario ran as an attach-as-main.
        plug(); settle(4)
        make_main(1); settle(3)
        unplug(); settle(4)


@scenario("clamshell", doc="the main display leaves and comes back, as a lid close does")
def s_clamshell(base):
    """The closest this guest gets to closing a laptop lid.

    A lid close is not an ordinary unplug: the display that goes away is the
    one holding the menu bar, so macOS has to move the menu bar, the command
    space and every desktop the display owned onto a survivor, and then undo
    all of it when the lid opens. Eric hit a restore failure doing exactly that
    -- lid shut while unplugging, lid opened before plugging back in -- and
    none of the existing scenarios remove the *main* display, so none of them
    exercise that path.

    The guest cannot remove its own framebuffer, so the probe is made main
    first and removed in that role. What that does not reproduce is the pseudo
    display a real lid reports (see `settle-and-return-in-one-report`) or the
    two overlapping transitions of Eric's case, where the external left while
    the main one was already gone.
    """
    plug(); settle()
    a = snapshot("attached")
    ext = probe_display(a["displays"], base["displays"])
    if not ext:
        raise Violation("external display not visible to rift")
    make_main(int(ext["screen_id"])); settle()
    main_held = snapshot("probe is main")

    # Prove the scenario is doing what its name says before trusting a PASS.
    # A `make_main` that silently did nothing leaves an ordinary unplug wearing
    # a clamshell label, and an ordinary unplug is already covered five times
    # over.
    who = [ln.split()[0] for ln in sh(f"{DTOOL} list").splitlines()
           if "main=1" in ln]
    if who != [str(ext["screen_id"])]:
        raise Violation(f"probe did not become main: dtool says main={who}, "
                        f"probe is {ext['screen_id']} -- the scenario would be "
                        "testing an ordinary unplug")
    print(f"    (probe {ext['screen_id']} is main; removing it)", flush=True)

    # Lid shut: the display owning the menu bar disappears.
    unplug(); settle(8)
    if display_count() != 1:
        raise Violation("the main display did not actually leave")
    check_full(base, snapshot("main display gone"), "clamshell shut")

    # Lid open: it comes back, and should take its desktops with it.
    plug(); settle(8)
    back = snapshot("main display back")
    check_full(main_held, back, "clamshell open")
    make_main(1); settle(3)


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
    # Start from a TextEdit: the three of them share a container, where the
    # window focused last -- after a fresh start, often Safari, which tile_all
    # puts in a container of its own -- can make a stack of one.
    shown = [d.get("space") for d in rift("displays") or [] if d.get("space") is not None]
    for sid in shown:
        te = next((w for w in rift("windows", "--space-id", str(sid)) or []
                   if w.get("app_name") == "TextEdit" and is_tiled(w)), None)
        if te:
            focus(te)
            settle(1)
            break
    # toggle-stack acts on the selected *container*, not on a window, so the
    # selection has to be walked up to the parent split first -- without the
    # ascend the command is a no-op and the scenario silently tests nothing.
    rift_exec("layout ascend")
    settle(1)
    rift_exec("layout toggle-stack")
    settle(2)
    a = snapshot("stacked")
    # A stack of one is no stack: on a freshly started rift the selection can
    # sit on a window's own container, and stacking that makes a container a
    # normalising pass is entitled to flatten -- which the attach check then
    # reports as a stack lost. Climb until the stack holds more than one.
    for _ in range(2):
        largest = max((len(m) for sh in a["shapes"].values() for m, _ in stacks(sh)), default=0)
        if largest >= 2:
            break
        if largest:
            rift_exec("layout toggle-stack")
            settle(1)
        rift_exec("layout ascend")
        settle(1)
        rift_exec("layout toggle-stack")
        settle(2)
        a = snapshot("stacked")
    if not any(len(m) >= 2 for sh in a["shapes"].values() for m, _ in stacks(sh)):
        # Last resort: undo whatever stack of one there is, climb to the root
        # -- which holds every window -- and stack that.
        if any(stacks(sh) for sh in a["shapes"].values()):
            rift_exec("layout toggle-stack")
            settle(1)
        for _ in range(3):
            rift_exec("layout ascend")
            settle(0.5)
        rift_exec("layout toggle-stack")
        settle(2)
        a = snapshot("stacked")
    if not any(len(m) >= 2 for sh in a["shapes"].values() for m, _ in stacks(sh)):
        raise Violation("no stack of two or more windows was created -- toggle-stack is "
                        "a no-op in this layout mode "
                        f"({[sh.get('mode') for sh in a['shapes'].values()]})")
    try:
        plug(); settle()
        check_stacks(a["shapes"], snapshot("stack + display")["shapes"], "stack-across-churn attach")
        unplug(); settle()
        after = snapshot("stack after churn")
        check_stacks(a["shapes"], after["shapes"], "stack-across-churn detach")
        check_full(a, after, "stack-across-churn")
    finally:
        # Unstacked whatever the verdict. Left behind by a failure, the stack
        # failed every scenario after this one on its frames -- stacked
        # windows are drawn 36px apart, and read as tiles they overlap.
        unplug(quiet=True); settle(2)
        # toggle-stack acts on the selected container, and after a churn the
        # selection may be a window inside the stack rather than the stack.
        for step in ("layout ascend", None):
            if not any(stacks(sh) for sh in snapshot("unstack")["shapes"].values()):
                break
            if step:
                rift_exec(step)
                settle(1)
            rift_exec("layout toggle-stack")
            settle(2)
        rift_exec("workspace set-layout bsp")
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
    for text, secs in sampler.episodes:
        print(f"      overlap for {secs:.2f}s: {text}", flush=True)
    stuck = sampler.persisted()
    if stuck:
        raise Violation(f"overlap(s) lasting {Sampler.PERSIST:.0f}s or more in "
                        f"{sampler.samples} samples mid-churn:\n      "
                        + "\n      ".join(f"{t} for {d:.2f}s" for t, d in stuck[:3]))
    print(f"      {len(sampler.episodes)} overlap episode(s), longest "
          f"{sampler.longest():.2f}s, none past {Sampler.PERSIST:.0f}s", flush=True)


# ------------------------------------------------------- real-world displays
#
# Eric does not plug the same monitor into the same port all day. Today's log
# alone: a 2560-wide monitor with the laptop to its LEFT and lower, then an
# ultrawide with the laptop BELOW it, each made main. These scenarios are that
# life: different monitors, sizes, scales and arrangements, monitors swapped,
# two at once, rearranged while attached. Each floats one window,
# because his config floats by default and those are the windows macOS moves
# by itself, and each samples frames throughout.

OFFICE = dict(width=3440, height=1440, serial=0x52)   # ultrawide, laptop below
HOME_MONITOR = dict(width=2560, height=1440, serial=0x51)  # laptop left, lower


def float_one() -> str:
    """Float one TextEdit on a shown desktop; returns its window server id."""
    for w in shown_windows():
        if w.get("app_name") != "TextEdit" or w.get("is_floating"):
            continue
        for _ in range(3):
            if not focus(w):
                break
            time.sleep(0.6)
            rift_exec("window toggle-float")
            time.sleep(0.6)
            now = [o for o in shown_windows() if o.get("id") == w.get("id")]
            if now and now[0].get("is_floating"):
                return str(w.get("window_server_id"))
    raise Violation("could not float a window to test with")


def rift_display_for(screen_id: int, snap: dict = None):
    for d in (snap or {}).get("displays") or rift("displays") or []:
        if int(d.get("screen_id", -1)) == int(screen_id):
            return d
    return None


def send_to_display(window_server_id: str, screen_id: int, snap: dict) -> None:
    d = rift_display_for(screen_id)
    rec = snap["windows"].get(str(window_server_id))
    if d is None or rec is None:
        raise Violation(f"cannot send {window_server_id} to display {screen_id}")
    for w in shown_windows():
        if str(w.get("window_server_id")) == str(window_server_id):
            sh(f"{CLI} execute display move-window --uuid {d['uuid']} "
               f"--window-id {w['id']['idx']}")
            return


def is_main(display: int) -> bool:
    return any(line.split()[0] == str(display) and "main=1" in line
               for line in sh(f"{DTOOL} list").splitlines() if line.split())


def arrange(ext: int, laptop_at: tuple) -> None:
    """The external as main, the laptop at `laptop_at` relative to it --
    unless macOS already remembers exactly that for this monitor, which is
    what a real reconnect relies on."""
    if not is_main(ext):
        make_main(ext); settle(3)
    for line in sh(f"{DTOOL} list").splitlines():
        parts = line.split()
        if parts and parts[0] == "1":
            at = parts[1].split("@")[1]
            if at != f"{laptop_at[0]},{laptop_at[1]}":
                place(1, *laptop_at); settle(3)
            break


def place_right_of_laptop(ext: int) -> None:
    """Put a monitor to the laptop's right, top-aligned, before a scenario
    starts from there. macOS remembers where each monitor was, across runs:
    one scenario moved a monitor left, and the next run of it started with the
    monitor already on the left and tested nothing."""
    for line in sh(f"{DTOOL} list").splitlines():
        parts = line.split()
        if parts and parts[0] == "1":
            width = int(parts[1].split("x")[0])
            place(ext, width, 0)
            settle(4)
            return


def on_display(snap: dict, idents, screen_id: int) -> list:
    """Of `idents`, the ones not on a desktop `screen_id` owns."""
    d = rift_display_for(screen_id, snap)
    owned = set(all_space_ids([d])) if d else set()
    return [snap["windows"][i][1] for i in idents
            if i in snap["windows"] and snap["windows"][i][0] not in owned]


def check_sampler(sampler: "Sampler", phase: str) -> None:
    for text, secs in sampler.episodes:
        if secs >= 0.3:
            print(f"      overlap for {secs:.2f}s: {text}", flush=True)
    stuck = sampler.persisted()
    if stuck:
        raise Violation(f"{phase}: overlap(s) lasting {Sampler.PERSIST:.0f}s or more:\n      "
                        + "\n      ".join(f"{t} for {d:.2f}s" for t, d in stuck[:3]))


def restore_arrangement() -> None:
    # Home first, while the monitors are still here to take them from.
    bring_windows_to_the_laptop()
    settle(2)
    detach_all_monitors()
    unplug(quiet=True)
    settle(3)
    if not is_main(1):
        make_main(1)
    place(1, 0, 0)
    settle(2)


@scenario("commute", doc="office ultrawide (laptop below) -> laptop -> home monitor "
                         "(laptop left) -> laptop -> office again")
def s_commute(base):
    floated = float_one()
    try:
        with Sampler("commute") as sampler:
            office = attach("mon-office", **OFFICE); settle(6)
            arrange(office, (1000, 1440)); settle(4)
            at_office = snapshot("office")
            safari = [i for i, r in at_office["windows"].items() if r[1] == "Safari"]
            for ident in safari:
                send_to_display(ident, office, at_office)
                time.sleep(1.0)
            settle(4)
            at_office = snapshot("office, Safari moved to the ultrawide")
            check_full(at_office, at_office, "office")
            if on_display(at_office, safari, office):
                raise Violation("commute: could not put the Safari windows on the office monitor")

            detach("mon-office"); settle(6)
            check_full(at_office, snapshot("left the office"), "left the office")

            home = attach("mon-home", **HOME_MONITOR); settle(6)
            arrange(home, (-1502, 700)); settle(4)
            at_home = snapshot("home")
            check_on_screen(at_home, "arrived home")
            check_no_limbo("arrived home")
            check_frames(at_home, "arrived home")
            check_windows(at_office["windows"], at_home["windows"], "arrived home")
            te = [i for i, r in at_home["windows"].items()
                  if r[1] == "TextEdit" and i != floated and r[2]]
            if te:
                send_to_display(te[0], home, at_home)
                settle(4)
            at_home = snapshot("home, a TextEdit on the monitor")
            check_full(at_home, at_home, "home")

            detach("mon-home"); settle(6)
            check_full(at_home, snapshot("left home"), "left home")

            office = attach("mon-office", **OFFICE); settle(8)
            arrange(office, (1000, 1440)); settle(4)
            back = snapshot("office again")
            check_on_screen(back, "office again")
            check_no_limbo("office again")
            check_frames(back, "office again")
            away = on_display(back, safari, office)
            if away:
                raise Violation(f"commute: back at the office, {away} did not return "
                                "to the ultrawide")
            detach("mon-office"); settle(6)
            check_on_screen(snapshot("home time"), "home time")
        check_sampler(sampler, "commute")
    finally:
        restore_arrangement()


@scenario("hot-swap", doc="one monitor out and another in within a fraction of a second")
def s_hot_swap(base):
    floated = float_one()
    try:
        with Sampler("hot-swap") as sampler:
            plug(); settle()
            a = snapshot("monitor A")
            ids = [i for i, r in a["windows"].items() if r[1] == "Safari"]
            ext = probe_display(a["displays"], base["displays"])
            for ident in ids:
                send_to_display(ident, int(ext["screen_id"]), a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("monitor A, Safari on it")
            unplug(quiet=True)
            time.sleep(0.3)
            other = attach("mon-swap", width=2560, height=1440, serial=0x53); settle(8)
            check_full(a, snapshot("swapped to B"), "hot-swap to B")
            detach("mon-swap", wait=False)
            time.sleep(0.3)
            plug(); settle(8)
            back = snapshot("swapped back to A")
            check_full(a, back, "hot-swap back to A")
            away = on_display(back, ids, int(probe_display(back["displays"], base["displays"])["screen_id"]))
            if away:
                raise Violation(f"hot-swap: {away} did not return to monitor A")
        check_sampler(sampler, "hot-swap")
    finally:
        restore_arrangement()


@scenario("monitor-moved", doc="the same monitor comes back on the other side of the laptop")
def s_monitor_moved(base):
    float_one()
    try:
        with Sampler("monitor-moved") as sampler:
            ext = attach("mon-moved", serial=0x54); settle(6)
            place_right_of_laptop(ext)
            a = snapshot("right of the laptop")
            for ident in [i for i, r in a["windows"].items() if r[1] == "Safari"]:
                send_to_display(ident, ext, a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("right of the laptop, Safari on it")
            detach("mon-moved"); settle(6)
            ext = attach("mon-moved", serial=0x54); settle(4)
            # Back where it was first, as macOS would put it -- then moved.
            place_right_of_laptop(ext)
            # Left of the laptop and bottom-aligned with it.
            place(ext, -1920, 886 - 1080); settle(8)
            check_full(a, snapshot("left of the laptop"), "monitor-moved")
        check_sampler(sampler, "monitor-moved")
    finally:
        restore_arrangement()


@scenario("rearranged-while-attached",
          doc="the display is dragged to the other side in Settings; no plug at all")
def s_rearranged(base):
    float_one()
    try:
        with Sampler("rearranged") as sampler:
            ext = attach("mon-rearrange", serial=0x55); settle(6)
            place_right_of_laptop(ext)
            a = snapshot("right of the laptop")
            for ident in [i for i, r in a["windows"].items() if r[1] == "Safari"]:
                send_to_display(ident, ext, a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("right, Safari on it")
            place(ext, -1920, 0); settle(6)
            check_full(a, snapshot("moved left"), "moved left")
            place(ext, 0, -1080); settle(6)
            check_full(a, snapshot("moved above"), "moved above")
            place(ext, 1502, 0); settle(6)
            check_full(a, snapshot("moved back"), "moved back")
        check_sampler(sampler, "rearranged")
    finally:
        restore_arrangement()


@scenario("scale-change", doc="the same monitor comes back at 2x instead of 1x")
def s_scale_change(base):
    float_one()
    try:
        with Sampler("scale-change") as sampler:
            ext = attach("mon-scale", width=1920, height=1080, serial=0x56); settle(6)
            a = snapshot("1x")
            for ident in [i for i, r in a["windows"].items() if r[1] == "Safari"]:
                send_to_display(ident, ext, a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("1x, Safari on it")
            detach("mon-scale"); settle(6)
            attach("mon-scale", width=1280, height=720, serial=0x56, hidpi=True); settle(8)
            check_full(a, snapshot("back at 2x"), "scale-change")
        check_sampler(sampler, "scale-change")
    finally:
        restore_arrangement()


@scenario("dock-two-monitors", doc="two monitors arrive together, leave together, return")
def s_dock_two(base):
    float_one()
    try:
        with Sampler("dock") as sampler:
            attach("mon-dock-a", serial=0x57, wait=False)
            attach("mon-dock-b", width=2560, height=1440, serial=0x58, wait=False)
            if not wait_for_displays(3, 15):
                raise Violation("the dock's two monitors never both attached")
            settle(8)
            ids = [i for i in display_ids() if i != 1]
            a = snapshot("docked")
            safari = [i for i, r in a["windows"].items() if r[1] == "Safari"]
            for ident, target in zip(safari, ids):
                send_to_display(ident, target, a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("docked, a Safari on each monitor")
            check_full(a, a, "docked")
            for label in ("mon-dock-a", "mon-dock-b"):
                detach(label, wait=False)
            if not wait_for_displays(1, 15):
                raise Violation("the dock's monitors never both detached")
            settle(8)
            check_full(a, snapshot("undocked"), "undocked")
            attach("mon-dock-a", serial=0x57, wait=False)
            attach("mon-dock-b", width=2560, height=1440, serial=0x58, wait=False)
            if not wait_for_displays(3, 15):
                raise Violation("the dock's two monitors never both came back")
            settle(10)
            check_full(a, snapshot("docked again"), "docked again")
        check_sampler(sampler, "dock-two-monitors")
    finally:
        restore_arrangement()


@scenario("one-of-two-drops", doc="one of two monitors drops out and comes back")
def s_one_of_two(base):
    float_one()
    try:
        with Sampler("one-of-two") as sampler:
            first = attach("mon-pair-a", serial=0x59); settle(4)
            second = attach("mon-pair-b", width=2560, height=1440, serial=0x5a); settle(8)
            a = snapshot("both")
            safari = [i for i, r in a["windows"].items() if r[1] == "Safari"]
            for ident, target in zip(safari, (first, second)):
                send_to_display(ident, target, a)
                time.sleep(1.0)
            settle(4)
            a = snapshot("both, a Safari on each")
            detach("mon-pair-b"); settle(8)
            check_full(a, snapshot("one dropped"), "one dropped")
            attach("mon-pair-b", width=2560, height=1440, serial=0x5a); settle(10)
            check_full(a, snapshot("it came back"), "it came back")
        check_sampler(sampler, "one-of-two-drops")
    finally:
        restore_arrangement()


def sa_move_space_after(space: int, after: int) -> None:
    """What a Mission Control drag of a desktop onto another display asks for:
    the addition's SPACE_MOVE, spoken over its socket. The drag itself cannot
    be scripted."""
    import socket, struct
    body = bytes([0x05]) + struct.pack("=QQQB", space, after, 0, 1)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(3)
    sock.connect("/tmp/rift-sa_vm.socket")
    sock.sendall(struct.pack("=h", len(body)) + body)
    try:
        while sock.recv(64):
            pass
    except OSError:
        pass
    sock.close()


@scenario("desktop-to-new-monitor",
          doc="plug in, move the laptop's desktop onto the monitor (Mission Control "
              "drag), in both of Eric's arrangements")
def s_desktop_to_new_monitor(base):
    """Eric's first seam report: plug the monitor in, drag desktops onto it,
    and some windows end up "way over the seam". `desktop-move-seam-test.py`
    did this with the probe to the right and top-aligned, which is neither of
    his setups, and it passed on the old build too -- so it never reproduced
    the report. This does it in both: laptop left of and lower than a 2560
    monitor, and laptop below an ultrawide, each made main.
    """
    for label, spec, laptop_at in (("mon-home", HOME_MONITOR, (-1502, 700)),
                                   ("mon-office", OFFICE, (1000, 1440))):
        float_one()
        try:
            with Sampler(label) as sampler:
                ext = attach(label, **spec); settle(6)
                arrange(ext, laptop_at); settle(4)
                snap = snapshot(f"{label} attached")
                laptop = rift_display_for(1, snap)
                monitor = rift_display_for(ext, snap)
                home_desktop = laptop.get("space")
                if not any(r[0] == home_desktop for r in snap["windows"].values()):
                    home_desktop = None
                # The laptop must show the desktop holding the windows, and
                # keep another behind when that one moves away.
                held = {}
                for r in snap["windows"].values():
                    if r[0] in (laptop.get("space_ids") or []):
                        held[r[0]] = held.get(r[0], 0) + 1
                # `space create` and `space switch-to` act on the active
                # display, which with the monitor as main is not the laptop:
                # the pointer goes on the laptop first, for both.
                f = laptop.get("frame") or {}
                o, sz = f.get("origin", {}), f.get("size", {})
                sh(f"{BIN}/mtool move {o.get('x', 0) + sz.get('width', 0) / 2:.0f} "
                   f"{o.get('y', 0) + sz.get('height', 0) / 2:.0f}")
                time.sleep(0.5)
                if held:
                    want = max(held, key=held.get)
                    if want != laptop.get("space"):
                        rift_exec(f"space switch-to {laptop['space_ids'].index(want) + 1}")
                        settle(2)
                if len(laptop.get("space_ids") or []) < 2:
                    rift_exec("space create")
                    settle(3)
                snap = snapshot(f"{label} attached, laptop showing the windows")
                laptop = rift_display_for(1, snap)
                home_desktop = laptop.get("space")
                if not any(r[0] == home_desktop for r in snap["windows"].values()):
                    home_desktop = None
                if home_desktop is None or len(laptop.get("space_ids") or []) < 2:
                    raise Violation(f"{label}: the laptop needs a desktop holding the "
                                    f"windows and another to keep ({laptop.get('space_ids')})")
                sa_move_space_after(home_desktop, monitor["space"])
                settle(6)
                moved = snapshot(f"{label}: desktop moved onto the monitor")
                if not any(home_desktop in (d.get("space_ids") or []) and
                           int(d.get("screen_id", -1)) == ext for d in moved["displays"]):
                    raise Violation(f"{label}: desktop {home_desktop} did not move to the monitor")
                check_on_screen(moved, f"{label}: desktop moved")
                check_no_limbo(f"{label}: desktop moved")
                check_frames(moved, f"{label}: desktop moved")
                check_windows(snap["windows"], moved["windows"], f"{label}: desktop moved")
                detach(label); settle(6)
                gone = snapshot(f"{label}: monitor unplugged")
                check_on_screen(gone, f"{label}: monitor unplugged")
                check_no_limbo(f"{label}: monitor unplugged")
                check_frames(gone, f"{label}: monitor unplugged")
            check_sampler(sampler, label)
        finally:
            restore_arrangement()
            tile_all()


@scenario("native-fullscreen-across-churn",
          doc="macOS fullscreen held across a display churn; the slot must survive")
def s_native_fullscreen_churn(base):
    """The window is in macOS's own fullscreen when a display comes and goes.

    `fullscreen_slots.rs` records where a window sat before native fullscreen
    took it out of the tree, and puts it back there on the way out. A churn in
    between is the hard case: the recorded desktop can be destroyed and
    replaced underneath the slot, and the window server moves windows around
    while the tree is following it, so the slot can be re-read from a moment
    when the window is somewhere it does not belong.

    The assertions are on the trace as well as the end state. Three of the nine
    `fullscreen_slot` outcomes are failures -- "landed elsewhere; dropped",
    "nothing matched", and a "stale slot replaced" during a churn, which is
    exactly what "churn settling; slot kept" exists to prevent -- and each of
    them leaves a window the queries can still show as tiled somewhere
    plausible.

    Entering fullscreen is checked against the tree rather than against the
    trace: `record_fullscreen_slot` says nothing at all when the window already
    has a slot for the same desktop, so a silent trace does not mean the key
    missed. The window leaving its desktop's leaves does.
    """
    plug()
    # Longer than the usual settle on purpose: a slot taken while rift still
    # considers the churn to be settling is kept rather than recorded, and the
    # scenario would then be testing the previous run's leftovers.
    settle(8)
    # The plug gives the external a fresh, empty desktop and shows it, so the
    # windows are on a desktop nothing is showing -- and a fullscreen has to
    # start from a window whose tree can be read.
    for name, was, now in show_the_desktop_holding_the_windows():
        print(f"      ({name} was showing empty desktop {was}; switched to {now})",
              flush=True)
    settle(3)

    # Safari for preference: it has one window, and a posted key goes to
    # whichever window of the app is frontmost, so an app with three of them
    # (TextEdit here) makes the target a guess.
    # One Safari window, as the scenario was written for. A posted key goes to
    # the app's front window, and a window in native fullscreen cannot be
    # focused through rift: with setup's second Safari window open, the exit
    # key went to that one and put it into fullscreen instead, and the target
    # never came back (every run since setup opened two). The reset opens it
    # again for the next scenario.
    safari = [w for w in shown_windows() if w.get("app_name") == "Safari"]
    for extra in safari[1:]:
        if focus(extra):
            time.sleep(0.6)
            sh(f"{DTOOL} key 13 cmd")
            time.sleep(1.5)
    settle(2)
    a = snapshot("attached, tiled")
    # The target has to sit on a desktop whose tree can be read, and rift only
    # answers `query layout` for a desktop some display is showing right now --
    # every other one comes back with no container_tree, which `tree_shape`
    # marks `absent`. Picking blind lands on one of those about as often as
    # not, and the scenario then fails on its own precondition. Safari for
    # preference among the candidates: it has one window, and a posted key goes
    # to whichever window of the app is frontmost, so an app with three of them
    # (TextEdit here) makes the target a guess.
    #
    # The two identifier namespaces have to be kept apart here: `window_map`
    # keys on the window server id, and a tree's leaves are rift's own
    # `pid:idx`. Comparing one against the other silently finds nothing.
    shown = [d.get("space") for d in a["displays"] if d.get("space") is not None]
    candidates = []
    for space in shown:
        shape = tree_shape(space)
        leaves = leaf_order(shape)
        for w in (rift("windows", "--space-id", str(space)) or []):
            wid = w.get("id") or {}
            if not is_tiled(w) or f"{wid.get('pid')}:{wid.get('idx')}" not in leaves:
                continue
            candidates.append((w.get("app_name"), str(w.get("window_server_id")),
                               wid.get("idx"), space, shape, w))
    target = (next((c for c in candidates if c[0] == "Safari"), None)
              or next(iter(candidates), None))
    if target is None:
        raise Violation("native-fullscreen-across-churn: no tiled window on any "
                        f"shown desktop ({shown}) to fullscreen")
    app, ident, idx, home, before_shape, window = target

    # By timestamp, not by index: the flight recorder is a ring, and a churn
    # writes enough to wrap it. Slicing `[mark:]` off a list that has lost its
    # head silently drops exactly the entries the scenario is here to read --
    # it reported "no slot outcomes at all" for a window whose slot was
    # recorded and restored perfectly.
    seen = trace_acts({"fullscreen_slot"})
    mark_ms = max((ms for ms, _, _ in seen), default=-1)
    if not fullscreen_key(app, want=True, window=window):
        raise Violation(f"native-fullscreen-across-churn: {app} would not go "
                        "fullscreen (dtool posts Ctrl-Cmd-F to whatever is "
                        "frontmost -- check the app came up)")
    settle(5)
    # The trace, not the tree: while the window is fullscreen its display shows
    # the fullscreen space, so the home desktop is not shown and has no
    # readable tree at all -- which made "the window is no longer a leaf" true
    # of every window, including the ones that never moved.
    recorded = [d for _, _, d in trace_acts({"fullscreen_slot"})
                if isinstance(d, list) and len(d) > 1 and d[0] == idx
                and d[1] == "recorded"]
    if not recorded:
        others = [d for _, _, d in trace_acts({"fullscreen_slot"})
                  if isinstance(d, list) and len(d) > 1]
        raise Violation(
            f"native-fullscreen-across-churn: no slot was recorded for {app} "
            f"({idx}); some other window went fullscreen instead: {others[-4:]}")

    # The churn, with the window still away in its own space.
    churn_ms = max((ms for ms, _, _ in trace_acts({"fullscreen_slot"})), default=mark_ms)
    unplug(); settle(3)
    plug(); settle(5)

    left = fullscreen_key(app, want=False, window=window)
    settle(8)
    # Nothing is posted from here on. A posted key is not a transaction:
    # `open -a` can front the app a beat after the state was read, and a
    # "corrective" second key then toggles the window back *into* fullscreen --
    # the harness manufacturing the very race it goes on to measure. Measuring
    # in that window showed the tree without the window and called a correct
    # restore a failure; at rest the tree was identical to before, with the
    # window back in its slot. So: settle, measure, and clean up at the end.
    if not left:
        print("      (the exit key needed more than one pass)", flush=True)
    settle(10)

    slots = [(ms, d) for ms, _, d in trace_acts({"fullscreen_slot"})
             if ms > mark_ms and isinstance(d, list) and len(d) > 1]
    mine = [d[1] for _, d in slots if d[0] == idx]
    # "stale slot replaced" is only a failure once the churn is under way --
    # that is the window rift is meant to answer with "churn settling; slot
    # kept". Before the churn it is the mechanism working: a slot left behind
    # by an earlier run, on a desktop that no longer means anything, being
    # dropped in favour of this one.
    during = [d[1] for ms, d in slots if d[0] == idx and ms > churn_ms]
    bad = [o for o in mine if o in ("landed elsewhere; dropped", "nothing matched")]
    bad += [o for o in during if o == "stale slot replaced"]
    if bad:
        raise Violation("native-fullscreen-across-churn: the slot did not survive "
                        f"the churn: {bad}\n      this window: {mine}"
                        f"\n      every window: {[d for _, d in slots]}")
    # The slot has to be *used*, not merely survive. A window that comes back
    # with no restore at all leaves the same trace as one nobody asked about,
    # and the tree check downstream only says the order is wrong.
    if not any(o in ("restored", "re-anchored", "ordered in; restoring") for o in mine):
        raise Violation(
            "native-fullscreen-across-churn: the window came back without its "
            f"slot being used at all\n      this window: {mine}"
            f"\n      every window: {[d for _, d in slots]}")

    # Re-read rather than judge on one look. macOS is still finishing the
    # fullscreen exit for a few seconds after it reports itself done, and a
    # window that is briefly out of its tree on the way back is not the failure
    # this scenario is about -- `transient-glitch` and the Sampler are where
    # sub-second breakage is caught, deliberately and with frames.
    def settled_tree():
        for attempt in range(3):
            snap = snapshot("after native fullscreen + churn")
            rec = snap["windows"].get(ident)
            if rec is not None and rec[2]:
                shape = tree_shape(rec[0])
                if shape.get("absent") or any(l.endswith(f":{idx}")
                                              for l in leaf_order(shape)):
                    return snap, rec, shape
            if attempt < 2:
                settle(6)
        return snap, rec, (None if rec is None else tree_shape(rec[0]))

    after, back, now_shape = settled_tree()
    if back is None:
        raise Violation(f"native-fullscreen-across-churn: {app} is on no desktop at all")
    if not back[2]:
        raise Violation(f"native-fullscreen-across-churn: {app} came back floating "
                        f"(slot outcomes: {mine})")
    # The desktop is a new id if the churn replaced it; the tree's shape is
    # what has to match, and check_full covers the rest. A desktop no display
    # is showing has no readable tree, so there is nothing to compare -- the
    # grouping and frame checks in check_full still apply to it.
    was, now = leaf_order(before_shape), leaf_order(now_shape)
    # A subsequence, not an equality: the churn legitimately moves other
    # windows onto this desktop, and the question here is whether the windows
    # that were already in the tree kept their order relative to each other --
    # including the one that went away to fullscreen and came back. Demanding
    # the whole list match called a correct restore a failure whenever the
    # churn brought a neighbour along.
    def is_subsequence(small, big):
        it = iter(big)
        return all(any(x == y for y in it) for x in small)

    if now_shape.get("absent"):
        print(f"      (desktop {back[0]} is not shown; tree order not compared)",
              flush=True)
    elif not is_subsequence(was, now):
        raise Violation(
            "native-fullscreen-across-churn: the windows that were in the tree "
            f"came back in a different order\n      this window: {mine}"
            f"\n      every window: {[d for _, d in slots]}"
            f"\n      before: {was}"
            f"\n      after:  {now}")
    check_full(a, after, "native-fullscreen-across-churn")
    # Last thing, and only now, and only for the app this scenario fullscreened.
    # `clear_native_fullscreen` walks every test app, and with two displays
    # attached "some display is showing fullscreen" stays true after fronting an
    # app that is not the one in fullscreen -- so it posts the key and puts
    # *that* app into fullscreen instead. The trace showed a second window
    # picking up a slot it had no business having. Unplug first, so the question
    # has one display to be about.
    unplug(); settle()
    fullscreen_key(app, want=False, window=window, tries=2)


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
    ext = probe_display(rift("displays"), base["displays"])
    if not ext:
        raise Violation("external display not visible to rift")
    make_main(int(ext["screen_id"]))
    settle(3)

    # And the pointer on it. `space create` acts on the display under the
    # pointer by default (`space_target = "pointer"`), not the menu bar's --
    # the rule this scenario was written against -- so with the mouse left on
    # the original display the desktop was created there, the external kept
    # one desktop, and the precondition check below stopped the run.
    fr = ext.get("frame") or {}
    o, sz = fr.get("origin", {}), fr.get("size", {})
    cx = o.get("x", 0) + sz.get("width", 0) / 2
    cy = o.get("y", 0) + sz.get("height", 0) / 2
    sh(f"{os.path.dirname(CLI)}/mtool move {cx:.0f} {cy:.0f}")
    time.sleep(0.5)

    rift_exec("space create")
    settle(3)
    rift_exec("space switch right")
    settle(3)
    a = snapshot("attached, external showing an empty desktop")

    found = probe_display(a["displays"], base["displays"])
    external = [found] if found else []
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
        # The tell is a desktop's windows SPLITTING UP, not their desktop id
        # changing. macOS destroys desktops across a churn and rift's record
        # substitutes a replacement for each one, so every window of a desktop
        # legitimately comes back on a new id -- together, with its tree. An
        # earlier version of this check compared raw ids and called that a
        # straggler, which made a working substitution read as the bug.
        # `grouping` is id-free: it asks only who still shares a desktop with
        # whom, which is exactly what a window left behind breaks.
        survivors = set(a["windows"]) & set(after["windows"])
        was, now = grouping(a["windows"], survivors), grouping(after["windows"], survivors)
        if was != now:
            def render(part, loc):
                return " | ".join(sorted("+".join(sorted(loc[i][1] for i in g))
                                         for g in part))
            strays = [f"{after['windows'][i][1]} desktop "
                      f"{a['windows'][i][0]} -> {after['windows'][i][0]}"
                      for i in sorted(survivors)
                      if a["windows"][i][0] != after["windows"][i][0]]
            raise Violation(
                "straggler-after-return: the return left a desktop's windows "
                "split across two\n"
                f"      before: {render(was, a['windows'])}\n"
                f"      after:  {render(now, after['windows'])}\n"
                f"      moved:  {', '.join(strays)}")
        check_full(a, after, "straggler-after-return")
    finally:
        # setmain is permanent, so this has to run even when the assertion
        # above fires -- otherwise every later scenario measures geometry
        # against a display arrangement this one skewed.
        if before_main:
            make_main(int(before_main))
            settle(2)
    # The same bar as `transient-glitch`: what outlives the churn is rift's.
    stuck = sampler.persisted()
    if stuck:
        raise Violation("straggler-after-return: breakage mid-return lasting "
                        f"{Sampler.PERSIST:.0f}s or more:\n      "
                        + "\n      ".join(f"{t} for {d:.2f}s" for t, d in stuck[:2]))


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

    if cmd == "normalize":
        # The probe comes up wherever macOS last had it for this display set;
        # one attached as main makes every scenario an attach-as-main. Put the
        # menu bar back on the main display with the probe attached.
        plug(); settle(4)
        make_main(1); settle(3)
        unplug(); settle(4)
        print(sh(f"{DTOOL} list"))
        return 0

    if cmd == "setup":
        spawn_windows()
        # Before anything is measured. An app restores its own saved window
        # state, so a fullscreen left behind by an earlier run comes back with
        # the app -- and a display showing a fullscreen space has no desktop to
        # tile into, which makes every later result vacuous.
        if not clear_native_fullscreen():
            print("  WARNING: still showing a fullscreen space on "
                  f"{displays_showing_fullscreen()}; nothing below means much")
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
            for ident, app, x, y, w, h, _mn in sorted(rects, key=lambda r: (r[3], r[2])):
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
        on_screen = shown_desktops(snap)
        for sid, rects in sorted(_rects(snap).items()):
            # Say which are shown. The frames on a hidden desktop are whatever
            # was last applied to it, not what rift would lay out now, and
            # reading them as geometry is how "windows stranded off-screen"
            # got reported for weeks.
            print(f"  desktop {sid}: {len(rects)} tiled"
                  f"{'' if sid in on_screen else '  (hidden -- frames are stale, not wrong)'}")
            for ident, app, x, y, w, h, _mn in sorted(rects, key=lambda r: (r[3], r[2])):
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
        # Whatever mode the config arrived in is the one to leave it in. A
        # matrix run used to stop on "tile", and every later single-scenario
        # run then silently exercised a mode nobody chose -- `spaces` is the
        # only one with a display record at all, so the record's own scenarios
        # tested nothing and said PASS.
        was_mode = current_displaced_mode().split()[0]
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
            tiled_now = sum(1 for r in base["windows"].values() if r[2])
            print(f"  baseline: {len(base['windows'])} window(s), "
                  f"{tiled_now} tiled", flush=True)
            if tiled_now == 0:
                print(f"  SKIPPING {mode}: nothing is tiled, every result "
                      "below would be vacuous", flush=True)
                continue
            for name in wanted:
                reset = reset_between_scenarios()
                if reset:
                    print(f"    (reset: {reset})", flush=True)
                pre = scenario_start(name, base)
                started = time.time()
                try:
                    SCENARIOS[name](pre)
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
        set_displaced_windows(was_mode)
        print(f"\n(displaced_windows put back to {was_mode})")
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
    # Two things a previous run can leave behind that make every scenario after
    # it vacuous: an app still in native fullscreen (its display then shows no
    # desktop at all) and the windows sitting on a desktop nothing is showing.
    # Neither is a failure of the scenario about to run, so both are put right
    # here, out loud.
    if not clear_native_fullscreen():
        print(f"baseline: WARNING still fullscreen on {displays_showing_fullscreen()}")
    found = show_the_desktop_holding_the_windows()
    for name, was, now in found:
        print(f"baseline: {name} was showing empty desktop {was}; "
              f"switched to {now}, which holds the windows")
    base = snapshot("baseline")
    print(f"baseline: {render_displays(base)}")
    print(f"          {len(base['windows'])} window(s), "
          f"{sum(1 for r in base['windows'].values() if r[2])} tiled, "
          f"{base['workspace_total']} workspace(s)\n", flush=True)

    results = []
    for name in names:
        print(f"--- {name} ---", flush=True)
        reset = reset_between_scenarios()
        if reset:
            print(f"    (reset: {reset})", flush=True)
        pre = scenario_start(name, base)
        started = time.time()
        try:
            SCENARIOS[name](pre)
            res = (name, "PASS", f"{time.time()-started:.0f}s", "")
        except Violation as exc:
            res = (name, "FAIL", f"{time.time()-started:.0f}s", str(exc))
        except Exception as exc:
            # With where it happened: an ERROR is a fault in the harness, and
            # "TypeError: unhashable type" alone does not say which line.
            import traceback
            where = traceback.extract_tb(exc.__traceback__)[-3:]
            res = (name, "ERROR", f"{time.time()-started:.0f}s",
                   f"{type(exc).__name__}: {exc} at "
                   + " <- ".join(f"{f.name}:{f.lineno}" for f in reversed(where)))
        finally:
            # The flight recorder, taken before the cleanup unplug adds its own
            # churn. Every failure's cause used to be unreadable after the
            # fact: the ring is overwritten by the next scenario and /tmp by
            # the next reboot, so this goes where neither reaches.
            if res[1] != "PASS":
                os.makedirs(TRACES, exist_ok=True)
                sh(f"{CLI} execute trace dump {TRACES}/{name}.trace", timeout=60)
                res = res[:3] + (res[3] + f"\n    trace: {TRACES}/{name}.trace",)
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
