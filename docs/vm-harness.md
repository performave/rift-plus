# The VM harness

`just churn` reproduces a display hotplug in software, and it works — but it
runs against the rift serving the machine you are using. Every cycle moves your
windows. That makes the one test worth running continuously the one test you
cannot leave running.

This is the isolated place to run it: a VirtualBuddy guest with its own
WindowServer, its own desktops, and its own rift, reachable over SSH so nothing
about it touches your session.

## The guest can hotplug, just not through VZ

Virtualization.framework guests are hard-capped at **one VZ display**. The SDK
header says "Maximum of one display is supported", two displays fail
validation, and the private `_setDisplayPortCount:` rejects anything above 1.

The cap does not reach `CGVirtualDisplay`, which is a different driver. Inside
the guest, `scripts/vdisp.m` holds a virtual display open for as long as its
process lives, and macOS treats it as a genuine hotplug: a desktop of its own,
the same reconfiguration flags as real hardware (`ADD|ENABLED|…` on attach,
`REMOVE|DISABLED|…` on detach). Attaching is bootstrapping a `vdisp`
LaunchAgent and detaching is booting it out — process lifetime is the one
teardown that stays reliable, since `-[CGVirtualDisplay terminate]` is gone on
macOS 27. The guest holds several at once, each with its own serial, so macOS
keeps an arrangement per "monitor" and rift keeps a record per monitor. The one
real-hardware pattern it does not produce on its own is `SET_MAIN`, so the
harness forces that separately (`dtool setmain`).

`scripts/vm-display-probe.sh` answers the same question with BetterDisplay's
device, and is still the quickest check on a fresh guest.

## Provisioning

### Image

Restore from an IPSW matching the host's macOS build. The failures worth
chasing are OS behaviours — macOS 27 reaps desktops seconds after a
reconfiguration and keeps its own space-to-display table — and an older guest
would be testing a different operating system.

| setting | value | why |
|---|---|---|
| CPUs | 4 | leaves the host's remaining cores for your session |
| Memory | 8 GB | enough for a WindowServer plus a dozen test windows |
| Disk | 96 GB | Xcode CLT, Homebrew and a few golden snapshots |
| Display | 2560 × 1440, 1× | big enough that tiling bugs are visible in a screenshot |
| Network | NAT (VirtualBuddy default) | gives the guest an address the host can reach |

The guest's display follows the size of the VirtualBuddy window on the host. It
has come back from a restart 1216px wide, and five windows' minimum sizes do
not fit in that — every geometry result is then about the confound. Check
`rift query displays` reports the width you expect before trusting geometry.

### Guest setup, before you snapshot

Do all of this once, then take the golden snapshot.

1. **User `vm`, with automatic login enabled.** rift needs a WindowServer
   session, and an SSH login does not have one. Auto-login means the console
   session is always up for the harness to reach.
2. **Never let it lock or sleep.** A locked screen reports *zero* screens, and
   rift holds its last display snapshot by design — every test after that point
   would be measuring a frozen state.
   ```bash
   sudo pmset -a sleep 0 displaysleep 0 disksleep 0
   sudo defaults write /Library/Preferences/com.apple.screensaver loginWindowIdleTime 0
   defaults -currentHost write com.apple.screensaver idleTime 0
   ```
3. **Remote Login on, keys only.** System Settings → General → Sharing → Remote
   Login, then append the host's `~/.ssh/id_rift_vm.pub` to
   `~vm/.ssh/authorized_keys`.
4. **Quiet the noise:** turn off automatic software updates, and
   `sudo mdutil -a -i off` to stop Spotlight indexing competing for CPU.
5. **SIP off and the arm64e boot-arg**, for the scripting addition, which most
   scenarios depend on. The two are independent: turning SIP off does **not**
   enable the preview arm64e ABI, and without the boot-arg the payload will not
   load into Dock however far SIP is down. From recoveryOS, in this order —
   NVRAM is itself SIP-protected, so the boot-arg write fails if done first:

   ```bash
   csrutil disable
   nvram boot-args=-arm64e_preview_abi
   ```

   Reboot and confirm both (`csrutil status`, `nvram -p | grep boot-args`); a
   silent failure here surfaces much later as the addition simply not loading.
6. **Accessibility for rift, keyed on the path.** Insert the TCC row by hand
   with `client_type = 1`, which keys the grant on the absolute path
   `~/rift-harness/bin/rift` rather than on a signing identity. A binary
   replaced at that path keeps the grant whatever it is signed with, which is
   why the deploy scripts can ad-hoc sign in the guest. To check before
   assuming it is gone:
   ```bash
   sudo sqlite3 '/Library/Application Support/com.apple.TCC/TCC.db' \
     "select service, client, client_type, auth_value from access where client like '%rift%';"
   ```
7. **Name the binary by path in the guest's `run_on_start`.** rift reloads the
   scripting addition after a Dock restart by replaying `run_on_start`, and bare
   `rift` is not on launchd's PATH. With it, every reload fails silently and a
   battery runs without the addition.
8. **Passwordless `sudo`** for `launchctl` (used by `vm.sh`) and for `rift sa
   load`. Validate before installing — a malformed file in `/etc/sudoers.d`
   breaks `sudo` outright (`sudo visudo -c -f <file>`).
9. **Screen Recording for SSH**, if `vm.sh shot` should return anything but a
   black rectangle: add `/usr/libexec/sshd-keygen-wrapper`.
10. **The helpers.** `vdisp`, `dtool`, `mtool` and `laggyapp` are single
    Objective-C files in `scripts/`; each has its `clang` line at the top.
    Build them in the guest into `~/rift-harness/bin/`.

The scripts reach the guest at `vm@192.168.64.6` with `~/.ssh/id_rift_vm` and
their own known-hosts file, and do not read or edit `~/.ssh/config`.
`RIFT_VM_IP`, `RIFT_VM_HOST` and `RIFT_VM_USER` override that.

## How the harness reaches rift

rift-cli finds rift by **mach bootstrap lookup**, and an SSH session is in a
different bootstrap namespace from the GUI login. `launchctl asuser` bridges
them but needs root; bootstrapping a LaunchAgent into `gui/<uid>` reaches the
same namespace with no privilege. A bootstrap costs one to two seconds, so the
harness runs *whole* as one such job rather than dispatching per command.

- `scripts/vm-ssh` — a plain SSH to the guest.
- `scripts/vm-run1 <cmd>` — one command in the Aqua session, output printed.
- `scripts/vm-run <chaos.py args>` — pushes the host's `chaos.py` into the
  guest (the guest's own copy otherwise goes stale, and a run then measures
  nobody knows which revision), runs it as a LaunchAgent and tails its log.
- `scripts/vm-deploy <build>` — installs a build and restarts rift on a clean
  footing, refusing to report success unless the running version is the one
  asked for.
- `scripts/vm-ab <build> [matrix] <scenario…>` — the controlled run; see
  below.
- `scripts/vm.sh` — the older host-side wrapper: `deploy` (build and sign on
  the host, rsync), `probe`, `churn`, `shell`, `logs`, `ip`, `shot`.

Builds go into `~/rift-harness/builds/` in the guest as `<name>` and
`<name>-cli`. **Deploy `rift` and `rift-cli` together, never one alone:** a new
client still reaches an old rift, but an old client against a new rift looks up
the wrong service name and gets empty answers with no error — which looks
exactly like rift having lost its displays.

Two CLI details that fail silently in a loop: `execute window focus` needs
`--window-id '{"pid":P,"idx":I}'` (only `--window-server-id` exits 2), and
queries need the literal `query` subcommand.

A launchd job stays registered after its process exits, so `launchctl print`
succeeding says nothing about whether it is still running; poll its `state`.
And every one-shot the harness bootstraps is a `RunAtLoad` plist, so one left
in `~/Library/LaunchAgents` fires again at every login. `vm-run` deletes each
plist as soon as it is loaded, and `vm-ab` refuses to start when anything
other than `rift-harness` and `vdisp` is there. A guest has been caught running
five stale jobs behind a battery, one of them dragging the very windows the
scenarios were measuring.

## Running a battery

```bash
scripts/vm-run list                       # the scenarios
scripts/vm-run control                    # no churn: is the tree stable at rest?
scripts/vm-ab <build> <scenario…>         # one controlled side
scripts/vm-ab <build> matrix <scenario…>  # once per displaced_windows mode
```

`vm-ab` gives every side the same start, because anything less compares two
piles of residue rather than two builds: it takes a lock (two runs racing the
single guest overwrite each other invisibly), reboots, prunes empty desktops,
installs the build and *checks* the running version against it, deletes
`layout.ron`, re-checks the scripting addition, puts the display arrangement
back, waits for the guest's load to drop, and builds a baseline of five tiled
windows. Between scenarios, `chaos.py` restarts rift on a fresh layout file,
detaches every probe, fetches windows back from any desktop macOS keeps for an
absent monitor, clears native fullscreen, and gathers the test windows onto one
desktop. Each failure leaves its flight recorder in `~/rift-harness/traces/`.

Run `matrix` for anything touching the display record: `spaces` is the only
`displaced_windows` mode that has one, so a record scenario run in `float` or
`tile` passes vacuously.

### What a number is worth

The same build's score moves by about **two scenarios** between sittings, and
two agreeing runs in one sitting are not a baseline for another sitting. So:

- Judge a build only against a control run **in the same sitting**, both sides
  through `vm-ab`. The control costs one run; twice here a "regression" was
  bisected and a "fix" celebrated before anyone re-ran the old build, and both
  were noise.
- Read the cells, not the count. Two builds with the same total can differ in
  which scenarios failed.
- When a suite stops repeating itself, suspect the environment before the code
  — and look for something running that nobody started.
- If a script prints a value it is supposed to be guarding, make it fail on the
  value instead. Printed guardrails were not read, twice.

## False failures: read this before adding a check

Each of these looked like a rift bug and was not.

- **`space: null` is almost always native fullscreen.** A display showing a
  fullscreen space has no desktop rift manages, and reads exactly like rift
  having lost it. Apps restore their own window state, so a Safari left
  fullscreen stays fullscreen across relaunches and guest reboots. It cannot be
  detected reliably either: rift drops a fullscreen space's windows from `query
  windows`, and fronting another app hides the state without clearing it. The
  harness therefore runs `clear_native_fullscreen` unconditionally.
- **Stale frames on hidden desktops.** rift deliberately does not arrange a
  desktop no display is showing, so after a churn its windows keep whatever
  frames the departed display gave them — at x=2600 on a 2494-wide display, or
  piled on each other. Switch to it and they are laid out at once. Geometry
  checks take shown desktops only, and read the window server's frames (`dtool
  frames`), not rift's record, which trails until the app answers.
- **App minimum sizes.** A slot narrower than an app's minimum makes it overflow
  into its neighbour. Eight windows in a bsp spiral drives slots under
  TextEdit's floor; four or five do not. Safari's floor is 574px. Prefer apps
  that resize freely — Calculator and Chess will not grow either.
- **rift's own ratio floor.** bsp clamps every split ratio to [0.05, 0.95]
  (`bsp.rs`), so a boundary stops at 5% of its parent, about 124px on this
  display. A resize stopping there is the clamp, not an app refusing.
- **A saved split ratio.** A `ratio:0.95` in `layout.ron` survives reboots and
  makes every geometry scenario fail on overlap. `rift execute layout balance`
  resets the active workspace; `grep -oE 'ratio:[0-9.]+' ~/.rift/layout.ron`
  shows it.
- **Desktop ids renumber across a churn as a matter of course**, and rift's
  record substitutes a replacement for each destroyed desktop. Compare who
  shares a desktop with whom, never raw ids.
- **Mid-churn overlap.** macOS moves windows to their remembered spots on an
  arriving display a beat before rift hears of the change, and rift lays them
  out only once the change has settled. Overlap inside that window (measured
  under 0.7 s) is expected; the sampler fails only overlap that persists past
  it.
- **Modes working as designed.** A tiled window coming back floating under
  `displaced_windows = "float"`; a desktop whose display has left having no
  layout to report; a visible frame smaller than the display by exactly the
  menu bar and Dock.
- **rift reporting no displays after a long session.** Reboot the guest before
  debugging anything else. It has cleared every time.

The discipline: re-run a geometry finding with fewer windows; run `control`
before reporting a structural one; A/B a suspected regression against `HEAD`
built from a detached worktree, not whatever old binary is lying in the guest.

## Traps in writing scenarios

Make a new scenario fail on purpose first. Several passed for weeks while
testing nothing.

- **`toggle-stack` is a no-op in bsp** and acts on the selected *container*: set
  the workspace to `traditional` and `layout ascend` first.
- **`space create` acts on the display that owns the menu bar**, shows the
  desktop it makes, and needs the scripting addition. macOS 27 may reap an
  empty desktop seconds after a display change.
- **`space switch-to N` is a global index** across every display, in Mission
  Control order. Use `query displays`' `space_ids`, not active-then-inactive.
- **`query windows` with no `--space-id` answers for one display's shown
  desktop**, and `query layout --space-id` answers only for a desktop some
  display is showing. Ask per desktop.
- **Two identifier namespaces:** window-server ids and rift's `pid:idx` tree
  leaves. Comparing one against the other finds nothing.
- **`role == "stack"` is master-stack's secondary column**, an ordinary split.
  Only a `layout_kind` ending in `_stack` is a stack.
- **The flight recorder is a ring.** Slice a dump by timestamp, not index; a
  churn can wrap it.
- **Restore what a scenario moved after its verdicts**, never between them.
- **`dtool setmain` configures permanently**, so an arrangement outlives a
  reboot. Put it back before a baseline.
- **Do not bulk-destroy leaked desktops** with `space switch-to` + `space
  destroy`. It left the window server showing a desktop rift could not name,
  and needed a reboot. `vm-ab`'s prune destroys only empty ones.
- **Native fullscreen:** `osascript` hangs indefinitely inside a LaunchAgent
  here, whatever TCC grants, so `dtool fullscreen` posts Ctrl-Cmd-F with
  `CGEventPost`. It goes to whatever is frontmost, so focus the target window
  through rift first, and always pass `--space-id` when reading across the
  round trip. Do not read geometry while an app might be mid-transition.

## Pointer input

The churn battery never touches the pointer, so it will pass a build that has
broken every mouse gesture — which an upstream merge once did, by taking
upstream's event-tap mask without `*MouseDragged`. Anything that changes
`src/actor/input.rs` goes through `scripts/handson.py`, which posts real
pointer input with `mtool` and reads the frames back. Its traps:

- In a bsp spiral the two left-most windows are usually stacked vertically;
  pick a pair that is horizontally adjacent *and* overlaps vertically.
- Drag a boundary in the direction that grows. A window at its minimum refuses
  to shrink.
- The modifier's left and right buttons are move and resize respectively in
  this config.
- Modifier gestures act on floating windows; float first, and grab
  three-quarters across so the resize has a nearer edge.
- Count windows across every desktop, not the one you started on.

Resize bugs appear only against an app that cannot keep up; TextEdit and Safari
apply a frame within a millisecond. `laggyapp` blocks its main thread in bursts
the way a heavy renderer does. `mtool spam` sends whole press cycles at a click
rate and `mtool thrash` releases mid-swing, which is what reaches the rates
(eight or nine a second) where writes and notifications are still in flight
across the next press.

## What the guest cannot test

- **Gestures.** There is no multitouch device, so the swipe and scroll paths
  rift reads from IOHID never fire. `mtool` cannot manufacture one.
- **Mirroring.** `CGConfigureDisplayMirrorOfDisplay` on a virtual display never
  returns and drops the guest to zero displays until a reboot.
- **Real hardware:** mixed scale factors, a real EDID, a lid's pseudo display,
  sleep across a change, one display asleep while another is awake.

## Resetting between runs

Display churn corrupts state that persists — that is the whole bug class — so a
run that ends dirty poisons the next. `vm-ab` resets by reboot and does not
need a snapshot. For a full reset, VirtualBuddy's `.vbst` save-states restore in
seconds. The cheap alternative: stop rift in the guest and delete
`~/.rift/layout.ron`.

The churn-created desktops are macOS state and survive a reboot, renumbered.
`vm-ab` prunes the empty ones before each side.
