#!/usr/bin/env bash
#
# Drive the test VM from the host. See docs/vm-harness.md.
#
# Every command here is aimed at the guest. Nothing in this file installs,
# restarts or signs anything on the host's running rift -- that is `just dev`,
# and using it while the VM harness is the point would defeat the isolation
# this script exists to provide.
set -euo pipefail

GUEST_USER="${RIFT_VM_USER:-vm}"
HOST="${RIFT_VM_HOST:-${GUEST_USER}@192.168.64.6}"
# Self-contained on purpose: the harness must not depend on -- or edit -- the
# host's ~/.ssh/config. Point RIFT_VM_HOST at an alias if you'd rather have one.
KEY="${RIFT_VM_KEY:-$HOME/.ssh/id_rift_vm}"
SSH_OPTS=(-i "$KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new
          -o UserKnownHostsFile="$HOME/.ssh/known_hosts_rift_vm"
          -o ConnectTimeout=8)
ssh()  { command ssh  "${SSH_OPTS[@]}" "$@"; }
scp()  { command scp  "${SSH_OPTS[@]}" "$@"; }
rsync_() { command rsync -az -e "ssh ${SSH_OPTS[*]}" "$@"; }
PROFILE="${RIFT_VM_PROFILE:-release-fast}"
IDENTITY="${RIFT_CODESIGN_IDENTITY:-Developer ID Application: Eric Wang (8UR4G77744)}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REMOTE_DIR="/Users/${GUEST_USER}/rift-harness"

die() { echo "vm.sh: $*" >&2; exit 1; }

# rift lives in the guest's GUI login session: it needs a WindowServer, and it
# registers its mach service in that session's bootstrap namespace. An SSH
# session is in a different namespace, so a bare `ssh rift-vm rift-cli ...`
# cannot even find the running rift. `launchctl asuser` crosses over, and it
# needs root -- hence the passwordless sudo rule in the provisioning spec.
gui() { ssh "$HOST" "sudo launchctl asuser \$(id -u ${GUEST_USER}) sudo -u ${GUEST_USER} $*"; }

require_vm() {
    ssh -o ConnectTimeout=5 -o BatchMode=yes "$HOST" true 2>/dev/null \
        || die "cannot reach '$HOST' over ssh. Is the VM booted, and is the alias in ~/.ssh/config? See docs/vm-harness.md."
}

sync_scripts() {
    ssh "$HOST" "mkdir -p '${REMOTE_DIR}/scripts'"
    rsync_ "$REPO/scripts/display-churn.py" "$REPO/scripts/vm-display-probe.sh" \
        "$HOST:${REMOTE_DIR}/scripts/"
}

cmd_deploy() {
    require_vm
    # Build and sign on the host: much faster, and a stable Developer ID
    # signature is what keeps the guest's Accessibility grant alive across
    # rebuilds. An ad-hoc signature changes the cdhash every time and macOS
    # drops the grant, which rift reports as an immediate exit.
    ( cd "$REPO" && cargo build --profile "$PROFILE" --bins )
    for bin in rift rift-cli; do
        codesign --force --sign "$IDENTITY" "$REPO/target/$PROFILE/$bin"
    done

    ssh "$HOST" "mkdir -p '${REMOTE_DIR}/bin'"
    rsync_ "$REPO/target/$PROFILE/rift" "$REPO/target/$PROFILE/rift-cli" \
        "$HOST:${REMOTE_DIR}/bin/"
    sync_scripts

    # Restart through launchd rather than exec'ing rift over ssh, so it comes
    # up inside the GUI session with the Accessibility grant it was given.
    gui "${REMOTE_DIR}/bin/rift service restart" \
        || gui "launchctl kickstart -k gui/\$(id -u ${GUEST_USER})/com.performave.rift-plus" \
        || gui "launchctl kickstart -k gui/\$(id -u ${GUEST_USER})/git.acsandmann.rift"
    echo "vm.sh: deployed to $HOST"
}

cmd_probe() {
    require_vm
    sync_scripts
    gui "bash ${REMOTE_DIR}/scripts/vm-display-probe.sh"
}

cmd_churn() {
    require_vm
    sync_scripts
    gui "RIFT_CLI=${REMOTE_DIR}/bin/rift-cli python3 ${REMOTE_DIR}/scripts/display-churn.py $*"
}

cmd_query()  { require_vm; gui "${REMOTE_DIR}/bin/rift-cli query $*"; }
cmd_logs()   { require_vm; ssh -t "$HOST" "tail -f /tmp/rift_${GUEST_USER}.err.log"; }
cmd_shell()  { require_vm; ssh -t "$HOST"; }
cmd_reset()  {
    require_vm
    gui "${REMOTE_DIR}/bin/rift service stop" || true
    ssh "$HOST" "rm -f /Users/${GUEST_USER}/.rift/layout.ron"
    gui "${REMOTE_DIR}/bin/rift service start"
    echo "vm.sh: cleared the guest's persisted layout"
}

# VZ hands the guest a DHCP lease on the 192.168.64.0/24 vmnet. Reading the
# lease file beats hardcoding an address that changes whenever the guest is
# rebuilt. Prefer an ~/.ssh/config alias; this is for when there isn't one.
cmd_ip() {
    local leases=/var/db/dhcpd_leases
    [[ -r "$leases" ]] || die "cannot read $leases"
    # A name can hold several leases, and the file is ordered newest first --
    # so "the last one" is the stale one. Pick the latest expiry instead.
    # (macOS awk is BWK awk: no strtonum, hence the hex maths in bash.)
    local want="${RIFT_VM_NAME:-vmsVirtlMachine}" best=0 found="" name ip lease value
    while read -r name ip lease; do
        [[ "$name" == "$want" ]] || continue
        value=$(( 16#${lease#0x} ))
        (( value > best )) && { best=$value; found=$ip; }
    done < <(awk '
        /^{/ { name=""; ip=""; lease="" }
        /name=/       { s=$0; sub(/.*name=/, "", s);       name=s }
        /ip_address=/ { s=$0; sub(/.*ip_address=/, "", s); ip=s }
        /lease=/      { s=$0; sub(/.*lease=/, "", s);      lease=s }
        /^}/ { if (name != "" && ip != "" && lease != "") print name, ip, lease }
    ' "$leases")
    [[ -n "$found" ]] || die "no DHCP lease for '$want' in $leases"
    echo "$found"
}

# Seeing the guest without opening its window. Needs Screen Recording granted
# to sshd-keygen-wrapper in the guest, or the capture comes out black.
cmd_shot() {
    require_vm
    local out="${1:-/tmp/rift-vm-$(date +%H%M%S).png}"
    gui "/usr/sbin/screencapture -x /tmp/vm-shot.png"
    scp -q "$HOST:/tmp/vm-shot.png" "$out"
    ssh "$HOST" "rm -f /tmp/vm-shot.png"
    echo "$out"
}

usage() {
    cat <<'EOF'
usage: scripts/vm.sh <command> [args]

  probe              can this guest hotplug a display at all? run this first
  deploy             build + sign on the host, rsync into the guest, restart
  churn [args]       display churn, in the guest (args pass to display-churn.py)
  query <args>       rift-cli query, in the guest
  reset              stop rift, drop the persisted layout, start it again
  ip                 the guest's address, from the vmnet DHCP leases
  shot [path]        screenshot the guest and copy it back to the host
  logs               tail the guest's rift log
  shell              interactive ssh session

Env: RIFT_VM_HOST, RIFT_VM_USER (default vm), RIFT_VM_KEY, RIFT_VM_NAME
See docs/vm-harness.md.
EOF
}

case "${1:-}" in
    probe)  shift; cmd_probe "$@" ;;
    deploy) shift; cmd_deploy "$@" ;;
    churn)  shift; cmd_churn "$@" ;;
    query)  shift; cmd_query "$@" ;;
    reset)  shift; cmd_reset "$@" ;;
    ip)     shift; cmd_ip "$@" ;;
    shot)   shift; cmd_shot "$@" ;;
    logs)   shift; cmd_logs "$@" ;;
    shell)  shift; cmd_shell "$@" ;;
    ""|-h|--help|help) usage ;;
    *) usage; die "unknown command '${1}'" ;;
esac
