# The VM harness

`just churn` reproduces a display hotplug in software, and it works — but it
runs against the rift serving the machine you are using. Every cycle moves your
windows. That makes the one test worth running continuously the one test you
cannot leave running.

This is the isolated place to run it: a VirtualBuddy guest with its own
WindowServer, its own desktops, and its own rift, reachable over SSH so nothing
about it touches your session.

## Read this first: the guest may not be able to hotplug at all

Virtualization.framework guests are hard-capped at **one display**. The SDK
header says "Maximum of one display is supported", two displays fail
validation, and the private `_setDisplayPortCount:` rejects anything above 1.
It is VZ policy rather than a GPU limit — `PGMaxDisplayPortCount()` reports 8
ports with hot-unplug — but the policy is what we are stuck with. Measured
2026-09-16 on macOS 27.0 (26A428).

So the VM cannot add or remove a *VZ* display. The open question, and the first
thing to settle once the guest boots, is whether a **virtual** display can be
created inside it. BetterDisplay's device comes from its own driver, not from
VZ, so the cap may not apply to it. Nobody has tried.

- If it works, the guest gets a second display, `just churn` runs there, and
  this class of bug becomes testable without touching your machine.
- If it does not, the guest still earns its keep — SIP-off and
  `-arm64e_preview_abi` for scripting-addition work, Dock/SA injection, and
  golden-image resets — and display churn stays a host-only, you-are-away
  activity.

`scripts/vm-display-probe.sh` settles it. Run it first.

## Provisioning

### Image

Restore from an IPSW matching **26A428** (macOS 27.0), the same build as the
host. Today's failures are macOS 27 behaviours — desktops reaped seconds after
a reconfiguration, the OS keeping its own space-to-display table — and a 26.x
guest would be testing a different operating system.

| setting | value | why |
|---|---|---|
| CPUs | 4 | leaves the M3 Pro's remaining cores for your session |
| Memory | 8 GB | enough for a WindowServer plus a dozen test windows |
| Disk | 96 GB | Xcode CLT, Homebrew and a few golden snapshots |
| Display | 2560 × 1440, 1× | big enough that tiling bugs are visible in a screenshot |
| Network | NAT (VirtualBuddy default) | gives the guest an address the host can reach |

### Guest setup, before you snapshot

Do all of this once, then take the golden snapshot — everything below is what a
reset needs to come back to.

1. **User `rift`, with automatic login enabled.** This matters more than it
   looks: rift needs a WindowServer session, and an SSH login does not have
   one. Auto-login means the console session is always up and my SSH commands
   can reach it.
2. **Never let it lock or sleep.** A locked screen reports *zero* screens, and
   rift holds its last display snapshot by design — every test after that point
   would be measuring a frozen state.
   ```bash
   sudo pmset -a sleep 0 displaysleep 0 disksleep 0
   sudo defaults write /Library/Preferences/com.apple.screensaver loginWindowIdleTime 0
   defaults -currentHost write com.apple.screensaver idleTime 0
   ```
3. **Remote Login on, keys only.** System Settings → General → Sharing → Remote
   Login. Then append the host's public key to
   `~rift/.ssh/authorized_keys`.
4. **Quiet the noise:** turn off automatic software updates, and
   `sudo mdutil -a -i off` to stop Spotlight indexing competing for CPU.
5. **SIP off and the arm64e boot-arg**, while you are in recovery anyway.
   Neither is needed for display churn — the virtual-display route needs no
   SIP change, no entitlement, no root and no TCC — but they are the other
   reason to have this VM, and doing it later costs another recovery boot.

   The two are independent, which is the part that catches people: turning SIP
   off does **not** enable the preview arm64e ABI. Without the boot-arg the
   payload injected into Dock will not load, no matter how far SIP is down. You
   need both.

   Boot the guest to recoveryOS from VirtualBuddy, then, in order:

   ```bash
   csrutil disable                          # answer yes; needs reduced security
   nvram boot-args=-arm64e_preview_abi      # needs SIP already down
   ```

   Order matters: NVRAM is itself SIP-protected, so the boot-arg write fails if
   you do it first. Reboot, then confirm both took — a silent failure here
   surfaces much later as the scripting addition simply not loading:

   ```bash
   csrutil status                  # -> "System Integrity Protection status: disabled."
   nvram -p | grep boot-args       # -> boot-args  -arm64e_preview_abi
   ```
6. **Homebrew, then the probe's subject:**
   ```bash
   brew install --cask betterdisplay
   brew install waydabber/betterdisplay/betterdisplaycli
   ```
   Launch BetterDisplay once and leave CLI integration on (the default).
7. **Accessibility for rift.** Copy a Developer ID-signed `rift` in and grant it
   once. Because we sign on the host with a stable identity, the designated
   requirement does not change between rebuilds and the grant survives every
   later deploy — which is the whole reason the deploy path signs at all.
8. **Passwordless `launchctl`.** An SSH session is in a different bootstrap
   namespace from the GUI login session, so without this `rift-cli` cannot even
   find the running rift. Validate before installing — a malformed file in
   `/etc/sudoers.d` breaks `sudo` outright:
   ```bash
   printf 'rift ALL=(ALL) NOPASSWD: /bin/launchctl\n' > /tmp/rift-harness
   sudo visudo -c -f /tmp/rift-harness \
     && sudo install -m 440 -o root -g wheel /tmp/rift-harness /etc/sudoers.d/rift-harness
   ```
9. **Screen Recording for SSH**, if `vm.sh shot` should return anything but a
   black rectangle. System Settings → Privacy & Security → Screen Recording →
   add `/usr/libexec/sshd-keygen-wrapper`.

### Then, on the host

```
Host: ~/.ssh/config
  Host rift-vm
      HostName <guest IP or rift-vm.local>
      User rift
      IdentityFile ~/.ssh/id_rift_vm
      ServerAliveInterval 30
```

## Staying out of each other's way

The isolation is the point, so it is worth being explicit about what enforces
it.

**The guest never touches your session.** It has its own WindowServer. Its rift
manages its own windows. Nothing it does can move a window on your machine.

**I never drive the guest through its screen.** Everything goes over SSH via
`scripts/vm.sh`, which is a thin wrapper so that every command aimed at the VM
looks different from a command aimed at your machine. You can park the
VirtualBuddy window on another desktop, or minimise it; the guest keeps running
either way.

**Builds happen on the host, rift never gets installed there.** `cargo build`
and `codesign` run on your machine because they are much faster there and
because the Developer ID signature is what keeps the guest's Accessibility
grant stable. The deploy path then rsyncs the signed binary into the guest. It
does not go near `brew --prefix`, and it does not restart anything of yours —
`just dev` is the recipe that would, and it is not used here.

**Two things I will not run on this checkout while you are working:** `just
dev`, which swaps the binaries your service runs, and `just fmt`, which
formats every dirty `.rs` file in the tree including ones belonging to another
session. The VM path needs neither.

## Using it

```bash
scripts/vm.sh probe             # the display experiment — run this first
scripts/vm.sh deploy            # build + sign on host, rsync, restart in guest
scripts/vm.sh churn --cycles 50 # the real thing, in the guest
scripts/vm.sh shell             # interactive session in the guest
scripts/vm.sh logs              # tail the guest's rift logs
scripts/vm.sh ip                # the guest's address, from the vmnet leases
scripts/vm.sh shot              # screenshot the guest, copied back to the host
```

## Resetting between runs

Display churn corrupts state that persists — that is the whole bug class — so a
run that ends dirty poisons the next one. Snapshot the guest after setup and
restore before any run whose starting state matters. VirtualBuddy's `.vbst`
save-states restore in seconds, which is fast enough to do per-run rather than
per-session.

The cheap alternative, when a full restore is overkill: stop rift in the guest
and delete `~/.rift/layout.ron`. That clears the persisted layout without
touching the guest's desktops, which is usually the state that actually matters.
