#!/usr/bin/env bash
#
# Can this machine hotplug a second display at all?
#
# On the host the answer is yes and `just churn` depends on it. In a
# Virtualization.framework guest it is an open question: VZ caps its own
# displays at one, but BetterDisplay's device comes from a separate driver, so
# the cap may not reach it. This script settles it, and is the first thing to
# run on a fresh guest. See docs/vm-harness.md.
#
# Standalone on purpose: it does not need rift deployed. If rift-cli happens to
# be there it also answers the follow-up question -- whether a virtual display
# that attaches gets a desktop of its own, which is what makes it useful for
# churn testing rather than merely present.
set -uo pipefail

PROBE="rift-vm-probe"   # a fixed name: each new virtual identity leaks a
                        # permanent root-owned ICC profile into
                        # /Library/ColorSync/Profiles/Displays.
RIFT_CLI="${RIFT_CLI:-$(command -v rift-cli || true)}"

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }

# CGGetOnlineDisplayList is the authority; system_profiler lags behind it.
count_displays() {
    if command -v swift >/dev/null 2>&1; then
        swift -e '
import CoreGraphics
var n: UInt32 = 0
CGGetOnlineDisplayList(0, nil, &n)
var ids = [CGDirectDisplayID](repeating: 0, count: Int(n))
CGGetOnlineDisplayList(n, &ids, &n)
print(n)' 2>/dev/null && return
    fi
    system_profiler SPDisplaysDataType 2>/dev/null | grep -c "Resolution:"
}

# macOS goes on listing a display for about a second after it is gone, and an
# attach settles in 0.3-0.7s. Poll rather than sleep; a fixed wait either wastes
# the run or reads the set mid-transition.
wait_for_count() {
    local want="$1" deadline=$((SECONDS + 8)) got
    while (( SECONDS < deadline )); do
        got="$(count_displays)"
        [[ "$got" == "$want" ]] && { echo "$got"; return 0; }
        sleep 0.2
    done
    echo "$got"; return 1
}

cleanup() {
    # Never a bare `discard`: with no identifier it drops every discardable
    # device on the machine.
    betterdisplaycli discard "-name=${PROBE}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "Environment"
info "$(sw_vers -productName) $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
if [[ "$(sysctl -n kern.hv_vmm_present 2>/dev/null || echo 0)" == "1" ]]; then
    info "running inside a VM (kern.hv_vmm_present=1)"
else
    info "NOT in a VM -- this is the host. Nothing here is destructive, but the"
    info "point of the probe is the guest."
fi

command -v betterdisplaycli >/dev/null 2>&1 || {
    say "RESULT: inconclusive"
    info "betterdisplaycli is not installed. See docs/vm-harness.md step 6."
    exit 2
}

baseline="$(count_displays)"
say "Baseline"
info "online displays: ${baseline}"

say "Creating the probe device"
# `-name=` is a lookup identifier; `-virtualScreenName=` is what names a new
# device. Creating does not connect it.
betterdisplaycli create "-virtualScreenName=${PROBE}" -vendor=rift -model=probe >/dev/null 2>&1
if ! betterdisplaycli get --identifiers 2>/dev/null | grep -q "${PROBE}"; then
    say "RESULT: NO -- the guest cannot create a virtual display"
    info "BetterDisplay's driver did not produce a device. Most likely its"
    info "DriverKit extension will not load in the guest."
    info ""
    info "Fallback: display churn stays a host-only activity, and the VM is used"
    info "for everything else. See the 'Read this first' section of the doc."
    exit 1
fi
info "device created"

say "Connecting it"
betterdisplaycli set "-name=${PROBE}" -connected=on >/dev/null 2>&1
attached="$(wait_for_count $((baseline + 1)))"
if [[ "$attached" != "$((baseline + 1))" ]]; then
    say "RESULT: NO -- created, but it will not attach"
    info "online displays stayed at ${attached}, expected $((baseline + 1))."
    info "The VZ one-display cap appears to reach BetterDisplay's device too."
    exit 1
fi
info "online displays: ${attached}  <- a second display attached"

# Attaching is necessary but not sufficient. A display that does not get a
# desktop of its own cannot exercise the space renumbering that the bugs live
# in, which is the only reason we want it.
say "Does it get a desktop of its own?"
if [[ -n "$RIFT_CLI" ]] && "$RIFT_CLI" query displays >/dev/null 2>&1; then
    "$RIFT_CLI" query displays | python3 -c '
import json, sys
for d in json.load(sys.stdin):
    spaces = d.get("active_space_ids") or []
    print(f"  {d.get(\"name\") or \"?\":<28} desktop={d.get(\"space\")} active={spaces}")
'
    info ""
    info "A probe row with a desktop id of its own is the answer we want."
else
    info "rift-cli not available -- deploy rift and re-run to confirm the"
    info "desktop allocation. Display attachment alone is already the hard part."
fi

say "Detaching"
betterdisplaycli set "-name=${PROBE}" -connected=off >/dev/null 2>&1
detached="$(wait_for_count "$baseline")"
info "online displays: ${detached}"

say "RESULT: YES -- this guest can hotplug a virtual display"
info "Run the real thing:  scripts/vm.sh churn --cycles 20"
