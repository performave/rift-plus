# The scripting addition

Three things rift can do have no unprivileged API left on macOS 26 — moving a
window to another space, creating a space, destroying one. Every route that does
not need elevated privileges was probed and is gone:

| route | result on macOS 26 |
|---|---|
| `SLSPerformAsynchronousBridgedWindowManagementOperation` | symbol removed (its Objective-C operation class still exists, which is a red herring) |
| `SLSSetWindowListWorkspace` + `SLSSpaceSetCompatID` | returns 1006, `kCGErrorNotImplemented` |
| `SLSMoveWindowsToManagedSpace` | callable, returns void, the window verifiably stays put |
| `CGSAddWindowsToSpaces` / `CGSRemoveWindowsFromSpaces` | symbols removed from SkyLight |

What is left is code running *inside* Dock, which is what a scripting addition
is. rift ships its own: `src/osax/payload.m` is compiled into a bundle that gets
`dlopen`ed inside Dock, where it listens on `/tmp/rift-sa_$USER.socket`, and
`src/osax/loader.m` is the small executable that puts it there.

Everything here is optional. rift uses its own route wherever one exists, and
every command that needs the addition reports failure rather than pretending, so
a machine without it simply does not get those three keys and nothing else
changes.

Both files are vendored from [yabai](https://github.com/koekeishiya/yabai) (MIT,
see `src/osax/LICENSE-yabai`), as is the wire protocol; the injection path for
arm64e is originally Jeremy Legendre's work. rift's changes are its own bundle
path, socket name and sudoers rule, so nothing of yabai's needs to be installed.

## What it needs from the machine

Injecting into another process is not something macOS allows by default, and
neither requirement can be worked around from inside rift:

- **SIP** with filesystem protections and debugging restrictions off. From
  recovery: `csrutil enable --without fs --without debug --without nvram`.
- **The arm64e preview ABI**, on Apple Silicon: `sudo nvram
  boot-args=-arm64e_preview_abi`, then reboot. Dock is arm64e, and a
  third-party arm64e binary will not run without it.

`rift sa load` checks both and says which is missing rather than half-failing.

## Setup

```sh
sudo rift sa load             # install the bundle and inject it into Dock
rift sa status                # ask the payload what it is; no root needed
```

`status` asks the payload over its socket rather than looking at the
filesystem, which is the only honest test: the bundle stays installed and
looking fine across a Dock restart that dropped the payload out of memory.

The payload belongs to Dock once loaded, so it keeps answering while rift is not
running — but it does not survive Dock restarting or the machine rebooting. To
re-inject on every start without a password prompt (launchd has no tty to type
one at):

```sh
sudo rift sa install-sudoers  # pins the rule to this binary's sha256 and `sa load`
```

then in `rift.toml`:

```toml
run_on_start = ["sudo rift sa load"]
```

The rule authorizes exactly one command line, from exactly the binary that
installed it: rebuild, upgrade or move `rift` and it stops authorizing, and
`sudo rift sa install-sudoers` has to run again. It is validated with
`visudo -c` before it is moved into place, so a malformed rule can never lock
sudo.

That failure is a quiet one -- launchd has nowhere to type the password, so the
addition simply is not there after the next Dock restart -- which is why rift
checks the rule itself. `rift sa status` reports it on a second line, and rift
warns in its log at startup whenever `run_on_start` contains a
`sudo ... sa load` and the rule is missing or pinned to another build. The check
reads `sudo -l` (the rule file itself is root-only) and compares the digest the
rule names against the running binary, so it answers for the binary you ask,
not for a path.

## Uninstalling

`brew uninstall` removes the keg and nothing else: the bundle and the sudoers
rule are root-owned and outside Homebrew's prefix, and a formula has no hook that
runs on uninstall. Take them out first, while the binary is still there to do
it:

```sh
sudo rift sa uninstall --all   # the bundle and the sudoers rule
brew services stop rift-plus
brew uninstall rift-plus
```

The payload already inside Dock is untouched by any of this; it goes when Dock
does (`killall Dock`), and is harmless until then.

## Commands

| command | root | what it does |
|---|---|---|
| `rift sa status` | no | handshakes with the payload inside Dock; reports the sudoers rule |
| `rift sa load` | yes | installs if needed, injects, reports health |
| `rift sa install` | yes | writes the bundle without injecting |
| `rift sa uninstall` | yes | removes `/Library/ScriptingAdditions/rift.osax` |
| `rift sa uninstall --all` | yes | that, and the sudoers rule: everything outside Homebrew's prefix |
| `rift sa install-sudoers` | yes | passwordless `sudo rift sa load` |
| `rift sa uninstall-sudoers` | yes | removes that rule |

## Attributes, and why `0x3f` is healthy

The handshake reports which Dock internals the payload managed to resolve, one
flag each. rift checks only the ones its own commands need
(`attrib::REQUIRED`): `dock.spaces`, the desktop picture manager, and the add /
remove / move space functions. Two flags are deliberately not required —
`SET_WINDOW` belongs to a yabai command rift does not send, and `ANIM_TIME` is
the patch that zeroes Dock's space-change animation.

`ANIM_TIME` is the one to know about. Finding it means matching an instruction
pattern in Dock, and *finding it also overwrites that pattern* — the payload
patches `animation_time_addr` at load time. So the first payload in a Dock
reports `0x7f` and every payload loaded after it reports `0x3f`, having searched
for bytes the first one already replaced. On a machine that also has yabai's
addition loaded, `0x3f` is the expected, healthy answer, and
`move_window_to_space` — the command that matters most — needs no flag at all.

`SPACE_STEP` (`0x80`) is rift's own and also optional. It says the payload
found the routine Dock steps the trackpad space-switch animation with, which
the `space_switch_animation` setting hooks (see below). Unlike `ANIM_TIME`,
finding it changes nothing: the routine is only patched once rift asks.

## The space switch animation hook

When a swipe between spaces is released, Dock finishes the slide with a
velocity spring that a routine on `DockCore.SpaceSwitcher` steps from a timer:
read the target space index and the scroll position, integrate, store the
position, push it to the window server, answer whether it is done. There is no
duration in it, and its coefficients sit in a literal pool three other Dock
animations share, so the payload hooks the routine's body instead. The
prologue and the tail stay; the math between them becomes a jump into the
payload, which computes the position from a duration and a cubic bezier and
jumps back into the tail, so Dock stores, applies and commits as it always
did. Finger tracking before the release is untouched.

`SA_OPCODE_SPACE_SWITCH_ANIMATION` (`0x14`) carries the duration in seconds and
the four bezier control points as `f64`s. A positive duration installs the
detour and sets the curve; zero restores the original sixteen bytes. rift sends
it at startup and on every config reload, from `space_switch_animation` in the
config. The payload keeps the setting until it is told otherwise or Dock
restarts.

The pattern for the routine is in `arm64_payload.m` next to the others, with a
delta from the match to the tail; the payload refuses to hook unless the
instruction at that delta is the store it expects (`str d1, [x20,
#scrollPosition]`, with the ivar offset taken from the runtime), so a Dock the
pattern happens to match but the delta does not is left alone. Only macOS 26 on
Apple silicon is listed so far.

## What the payload answers

Every command is one short-lived connection: rift writes the frame, the
payload acts, then closes. Most commands write nothing back, and the close is
the acknowledgement. Two answer first: the handshake, with the version and
attribute flags described above, and `SPACE_FOCUS`, with one byte saying
whether the switch was issued (`1`) or refused (`0`, meaning Dock did not
know the space). That byte is a rift addition to yabai's protocol. It exists
because `auto` switching falls back to a synthetic swipe when the addition
does not act, and a swipe posted on top of a switch that did happen lands one
space too far — so the fallback decision has to rest on the payload's own
word, not on rift reading the window server back afterwards, which can stall
under a fullscreen game long enough to look like a miss.

`SPACE_FOCUS` also does one thing beyond yabai's version of it: after the
window server has switched, it hands the new space to Dock's visibility
controller (`-[DockBar setDockVisibleOnSpace:alreadyLocked:]`, reached through
the spaces object's `delegate`), which is what Dock's own space-change listener
does. That controller keeps its own idea of which space the bar is on, and it
is what hides the Dock on a fullscreen space and brings it back on a desktop;
without the call, a teleport out of a fullscreen space arrived with no Dock
and one into it left the Dock up over the app.

## Versioning

`OSAX_VERSION` appears twice — in `src/osax/common.h`, compiled into the
payload, and in `sys::osax`, compiled into rift — and a unit test fails if they
drift apart. Bump it whenever `payload.m` changes in a way a running Dock has to
pick up: `sa load` compares its own version against what the payload answers
with, and only a mismatch is worth restarting Dock over, since `dlopen` of a
path already loaded is a no-op and the old code would otherwise keep the socket.

## Where the pieces live

| file | what it is |
|---|---|
| `src/osax/payload.m` | runs inside Dock; serves the socket |
| `src/osax/loader.m` | allocates shellcode in Dock's task and spawns a thread on it |
| `src/osax/common.h` | the socket path, version and opcodes both sides agree on |
| `build.rs` | builds both, fat x86_64 + arm64e, into `OUT_DIR` |
| `src/sys/osax.rs` | install, load, uninstall, status, sudoers |
| `src/sys/scripting_addition.rs` | the client: what rift calls at runtime |

One detail worth knowing when reading `sys::osax`: the loader's arm64e
capability byte has to equal the running Dock's or the thread it spawns there is
rejected, and a macOS update moves that byte. It is patched in place on every
load and the binary re-signed when it changed.
