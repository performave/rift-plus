#!/usr/bin/env python3
"""Move desktops through the scripting addition, within a display and between
two, and check that the window server and Dock both agree where they went.

The window server's order is what `dtool spaces` prints. Dock keeps a model
of its own, and on macOS 27.2 it no longer moves desktops itself: the addition
moves them with a SkyLight transaction and then has Dock re-read the layout.
Were that re-read missing, nothing would look wrong until Dock acted on its
stale copy -- so besides the window server's order, the test steps through
desktops with ⌃→, which walks Dock's own order, and focuses each moved desktop
through the addition, which finds it only in Dock's model of the display it
now belongs to.

rift is stopped for the run, since its display record would otherwise put a
reordered display back. Run in the guest's Aqua session:

    scripts/vm-run1 python3 ~/rift-harness/desktop-move-test.py

Exits 0 when every check passed, 1 otherwise; SKIP lines are not failures.
"""
import ctypes
import os
import socket
import struct
import subprocess
import sys
import time

B = os.path.expanduser("~/rift-harness/bin")
DTOOL = f"{B}/dtool"
UID = os.getuid()
SOCKET = f"/tmp/rift-sa_{os.environ.get('USER', 'vm')}.socket"
MONITOR = "mon-move"
SHOTS = "/tmp/desktop-move"

failures = []


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()


def check(ok, what):
    print(f"  {'ok  ' if ok else 'FAIL'} {what}")
    if not ok:
        failures.append(what)
    return ok


# ------------------------------------------------------------ window server

CF = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
SL = ctypes.CDLL("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight")
CF.CFStringCreateWithCString.restype = ctypes.c_void_p
CF.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]
CF.CFRelease.argtypes = [ctypes.c_void_p]
SL.SLSMainConnectionID.restype = ctypes.c_int
SL.SLSManagedDisplayGetCurrentSpace.restype = ctypes.c_uint64
SL.SLSManagedDisplayGetCurrentSpace.argtypes = [ctypes.c_int, ctypes.c_void_p]


def current(uuid):
    s = CF.CFStringCreateWithCString(None, uuid.encode(), 0x08000100)
    try:
        return SL.SLSManagedDisplayGetCurrentSpace(SL.SLSMainConnectionID(), s)
    finally:
        CF.CFRelease(s)


def layout():
    """[(display uuid, [desktop ids in the window server's order])]"""
    out = []
    for line in sh(f"{DTOOL} spaces").splitlines():
        parts = line.split()
        if parts:
            out.append((parts[0], [int(p) for p in parts[1:]]))
    return out


def ids_on(uuid):
    return dict(layout()).get(uuid, [])


def display_of(space):
    for uuid, ids in layout():
        if space in ids:
            return uuid
    return None


def dock_pid():
    return sh("pgrep -x Dock")


# ------------------------------------------------------------ the addition

def sa(op, args):
    body = bytes([op]) + args
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5)
    s.connect(SOCKET)
    s.sendall(struct.pack("<h", len(body)) + body)
    reply = b""
    while chunk := s.recv(64):
        reply += chunk
    s.close()
    return reply


def move(space, after, focus=False):
    sa(0x05, struct.pack("<QQQ?", space, after, 0, focus))
    time.sleep(1.5)


def focus(space):
    verdict = sa(0x02, struct.pack("<Q", space))
    time.sleep(1.0)
    return verdict in (b"", b"\x01")


def ctrl(direction):
    sh(f"{DTOOL} key {124 if direction > 0 else 123} ctrl")
    time.sleep(1.2)


def shot(name):
    # One file per display, main first; given one name, screencapture
    # captures the main display only.
    files = " ".join(f"{SHOTS}/{name}-d{i}.png" for i in range(1, len(layout()) + 1))
    sh(f"mkdir -p {SHOTS}; rm -f {SHOTS}/{name}-d*.png; /usr/sbin/screencapture -x {files}")


# ------------------------------------------------------------ monitors

def attach():
    plist = f"/tmp/{MONITOR}.plist"
    with open(plist, "w") as fh:
        fh.write(f"""<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>Label</key><string>{MONITOR}</string>
  <key>ProgramArguments</key><array>
    <string>{B}/vdisp</string><string>1920</string><string>1080</string><string>81</string>
  </array>
  <key>RunAtLoad</key><true/><key>KeepAlive</key><false/>
  <key>StandardErrorPath</key><string>/tmp/{MONITOR}.err</string>
</dict></plist>""")
    before = len(layout())
    sh(f"launchctl bootstrap gui/{UID} {plist}")
    end = time.time() + 15
    while time.time() < end and len(layout()) <= before:
        time.sleep(0.3)
    # macOS 27 settles a new display's desktops over a few seconds.
    time.sleep(6)
    return len(layout()) > before


def detach():
    sh(f"launchctl bootout gui/{UID}/{MONITOR} 2>/dev/null; rm -f /tmp/{MONITOR}.plist")
    time.sleep(4)


# ------------------------------------------------------------ the run

def walk_order(uuid, ids):
    """Step from the first desktop through the rest with ⌃→ and say whether
    Dock visits them in `ids` order."""
    if not focus(ids[0]) or current(uuid) != ids[0]:
        return None
    seen = [ids[0]]
    for _ in ids[1:]:
        ctrl(+1)
        seen.append(current(uuid))
    return seen


def same_display():
    print("\n== reorder within a display")
    uuid, ids = layout()[0]
    if len(ids) < 3:
        print(f"  SKIP: {len(ids)} desktops on {uuid}, need 3")
        return
    print(f"  {uuid}: {ids}")

    baseline = walk_order(uuid, ids)
    dock_walk = baseline == ids
    if not dock_walk:
        print(f"  SKIP Dock-order checks: ⌃→ walked {baseline}, not {ids}, before anything moved")

    moved, first = ids[-1], ids[0]
    move(moved, first)
    want = [first, moved] + ids[1:-1]
    shot("1-reordered")
    check(ids_on(uuid) == want, f"window server order after moving {moved} behind {first}: {ids_on(uuid)}, want {want}")
    if dock_walk:
        walked = walk_order(uuid, want)
        check(walked == want, f"Dock walks {walked}, want {want}")

    move(moved, ids[-2])
    check(ids_on(uuid) == ids, f"window server order after moving it back: {ids_on(uuid)}, want {ids}")
    if dock_walk:
        walked = walk_order(uuid, ids)
        check(walked == ids, f"Dock walks {walked} after moving back, want {ids}")
    focus(ids[0])


def across_displays():
    print("\n== move to another display and back")
    if not attach():
        check(False, "virtual display attached")
        return
    try:
        main_uuid, main_ids = layout()[0]
        ext_uuid, ext_ids = next(((u, i) for u, i in layout() if u != main_uuid), (None, []))
        print(f"  main {main_uuid}: {main_ids}  current {current(main_uuid)}")
        print(f"  ext  {ext_uuid}: {ext_ids}  current {current(ext_uuid) if ext_uuid else None}")
        if not ext_ids or len(main_ids) < 2:
            print("  SKIP: need a desktop on the new display and two on the main one")
            return

        shown = current(main_uuid)
        moved = next(s for s in reversed(main_ids) if s != shown)
        home_after = main_ids[main_ids.index(moved) - 1] if main_ids.index(moved) > 0 else None
        anchor = ext_ids[-1]

        move(moved, anchor)
        shot("2-moved-to-ext")
        check(display_of(moved) == ext_uuid, f"window server puts {moved} on the new display: {layout()}")
        check(moved not in ids_on(main_uuid), f"{moved} gone from the main display: {ids_on(main_uuid)}")
        check(focus(moved) and current(ext_uuid) == moved, f"Dock finds {moved} on the new display and shows it")
        shot("3-ext-shows-moved")
        focus(anchor)

        if home_after is not None:
            move(moved, home_after)
            check(ids_on(main_uuid) == main_ids, f"back on the main display in place: {ids_on(main_uuid)}, want {main_ids}")
            check(focus(moved) and current(main_uuid) == moved, f"Dock finds {moved} back on the main display")
            focus(shown)

        print("\n== move with focus (rift's create-and-place)")
        move(moved, anchor, focus=True)
        shot("4-moved-with-focus")
        check(display_of(moved) == ext_uuid and current(ext_uuid) == moved,
              f"{moved} on the new display and shown there (current {current(ext_uuid)})")
        check(current(main_uuid) in ids_on(main_uuid), f"main display still shows one of its own ({current(main_uuid)})")
        if home_after is not None:
            move(moved, home_after)
            check(ids_on(main_uuid) == main_ids, f"back in place: {ids_on(main_uuid)}, want {main_ids}")
            focus(shown)
    finally:
        detach()


def main():
    if not os.path.exists(SOCKET):
        print(f"FAIL: no scripting addition socket at {SOCKET}")
        return 1
    sh(f"launchctl bootout gui/{UID}/rift-harness 2>/dev/null")
    time.sleep(2)
    pid = dock_pid()
    print(f"Dock {pid}; displays: {layout()}")

    same_display()
    across_displays()

    check(dock_pid() == pid, f"Dock never restarted (was {pid}, now {dock_pid()})")
    print(f"\nscreenshots in {SHOTS}")
    print("PASS" if not failures else f"FAIL: {len(failures)} check(s)")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
