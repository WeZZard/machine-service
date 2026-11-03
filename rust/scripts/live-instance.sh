#!/bin/sh
# Run a second machine-service instance for live tests (docs/arch/test-layers.md,
# TL-4), beside the installed daemon and without touching it, through the
# shared launcher, `second-instance` (DC-1xx-LIVE-1).
#
# The launcher gives the instance its own root, LIVE_BIN/<port>, which holds
# its state, configuration, credential packs, logs and the manifest of what
# it owns; a minimal environment; loopback listeners only; and its own
# session, so stopping the shell that started it cannot signal the `tart
# run` processes of its VMs. Its clones carry the prefix si-<id>-, the only
# VMs it ever deletes. `restart` restarts the daemon alone, whose leases live
# on; `stop` releases every lease, deletes every VM it made and removes the
# root, and finishes what a crashed run left when run again.
#
# Usage: scripts/live-instance.sh start <guest-link> | restart | stop | status
# Environment, each required by start; none defaults to the installed
# instance's:
#   LIVE_PORT         the instance's HTTP port (never 6240)
#   LIVE_BROKER_PORT  its broker's port (never 6241)
#   LIVE_TART_HOME    the Tart store it clones its bases from
# Optional:
#   LIVE_BIN          the folder holding machine-service, second-instance and
#                     the agents from build-guest-agents.sh (default target/live)
#   PILOT_REPO        the instance's image checkout
set -eu

cd "$(dirname "$0")/.."
port="${LIVE_PORT:?set LIVE_PORT, the HTTP port of the second instance}"
bin="$(cd "${LIVE_BIN:-target/live}" && pwd)"
root="$bin/$port"
launcher="$bin/second-instance"

case "${1:-}" in
start)
    link="${2:?usage: live-instance.sh start <guest-link>}"
    set -- start --root "$root" --port "$port" \
        --session-port "${LIVE_BROKER_PORT:?set LIVE_BROKER_PORT, the port of its broker}" \
        --tart-home "${LIVE_TART_HOME:?set LIVE_TART_HOME, the Tart store it clones in}" \
        --executable "$bin/machine-service" --guest-link "$link"
    if [ -n "${PILOT_REPO:-}" ]; then
        set -- "$@" --env "PILOT_REPO=$PILOT_REPO"
    fi
    exec "$launcher" "$@"
    ;;
restart | stop | status)
    exec "$launcher" "$1" --root "$root"
    ;;
*)
    echo "usage: live-instance.sh start <guest-link> | restart | stop | status" >&2
    exit 2
    ;;
esac
