#!/bin/sh
# The host side of the relay acceptance suite (docs/arch/test-layers.md,
# TL-7): a second machine-service instance with the relay link, beside the
# installed one and without touching it, and the relay agents' network rig
# chain in front of its broker.
#
# Everything lives in one directory, RELAY_HOST_DIR (default: this script's
# directory), which holds the binaries (machine-service, second-instance,
# its guest agents and network-rig, as scripts/build-guest-agents.sh and
# `cargo build -p second-instance -p network-rig` make them), the instance's
# root and the rig's runs and tool cache.
#
# The instance is started by the shared launcher, `second-instance`
# (DC-243), under $RELAY_HOST_DIR/instance: its own ports, state,
# configuration and credential packs, a minimal environment, its own
# session, clones named si-<id>- in the Tart store it is given, and both of
# its listeners on one address, loopback unless RELAY_HOST_BIND names this
# host's LAN address for clients on other machines; never every interface.
# Its guests' relay agents dial the agent chain, never the broker itself:
# MACHINE_SERVICE_RELAY_GUEST_PORT gives them the chain's port on the
# host's address on their network, as machine-service derives it (BK-5).
# That address is a vmnet bridge, which appears only with the first VM, so
# the chain listens on every interface and admits only loopback and the
# bridges' guests (`--admit guests`, DC-248).
#
# With durable exposure on, the instance brings up its own relay agent
# (IN-3), whose label and home derive from the instance's state folder, so
# it never touches the installed instance's agent, and whose LaunchAgent
# lies under the state folder, so launchd never loads it at a later login.
# Durable exposure off loopback is for a durable acceptance cell only, named
# by RELAY_HOST_DURABLE_CELL; `exposure` prints what the instance exposes,
# which the suite records in the run's evidence.
#
# `restart` restarts the daemon alone; its leases and its agent live on, as
# the installed agent outlives a restart of its instance (AC-64). `stop`
# releases every lease, deletes every VM the instance made, unloads and
# removes its relay agent and removes its root; run again, it finishes what a
# crashed run left. `clean` is `stop` and the rig's runs and cache too.
#
# Usage: relay-acceptance-host.sh start | stop | restart | status | exposure | clean
#        relay-acceptance-host.sh rig-start <profile> <seed> | rig-stop | rig-ctl <json> | rig-status
#
# Settings, from the environment or from $RELAY_HOST_DIR/host.env; the first
# four are required and none defaults to the installed instance's:
#   RELAY_HOST_PORT          the instance's HTTP port (never 6240)
#   RELAY_HOST_SESSION_PORT  its broker's port (never 6241)
#   RELAY_HOST_AGENT_PORT    the agent chain's port
#   RELAY_HOST_TART_HOME     the Tart store it clones its bases from
#   RELAY_HOST_BIND          the address both listeners bind (default 127.0.0.1)
#   RELAY_HOST_AGENT_NAME    the name the agent chain's edge serves beside the
#                            host's addresses (default: <LocalHostName>.local)
#   RELAY_HOST_RELAY_URL     the relay_url to advertise instead of the default
#   RELAY_HOST_DURABLE       1 to turn durable exposure on (TL-6)
#   RELAY_HOST_DURABLE_CELL  the durable acceptance cell, required with
#                            RELAY_HOST_DURABLE off loopback
#   RELAY_HOST_GRACE_HOURS   the instance's lease grace, MACHINE_SERVICE_GRACE_HOURS
#                            (default 0: a lease and its relay credentials end
#                            at the end of its TTL, which AC-66 waits for)
#   PILOT_REPO               the image source, as the installed instance has it
set -eu

dir="$(cd "${RELAY_HOST_DIR:-$(dirname "$0")}" && pwd)"
# shellcheck disable=SC1091
[ -f "$dir/host.env" ] && . "$dir/host.env"
bind="${RELAY_HOST_BIND:-127.0.0.1}"
agent_name="${RELAY_HOST_AGENT_NAME:-$(scutil --get LocalHostName 2>/dev/null || hostname -s).local}"
grace="${RELAY_HOST_GRACE_HOURS:-0}"
root="$dir/instance"
rigs="$dir/agent-rig"
launcher="$dir/second-instance"
export NETWORK_RIG_CACHE="$dir/cache"

need() {
    eval "value=\${$1:-}"
    # shellcheck disable=SC2154
    if [ -z "$value" ]; then
        echo "set $1 (in the environment or $dir/host.env): $2" >&2
        exit 2
    fi
    printf '%s\n' "$value"
}

start_instance() {
    port="$(need RELAY_HOST_PORT "the instance's own HTTP port")"
    session_port="$(need RELAY_HOST_SESSION_PORT "its broker's own port")"
    agent_port="$(need RELAY_HOST_AGENT_PORT "the agent chain's port")"
    tart_home="$(need RELAY_HOST_TART_HOME "the Tart store it clones its bases from")"
    set -- start --root "$root" --port "$port" --session-port "$session_port" \
        --bind "$bind" --tart-home "$tart_home" --executable "$dir/machine-service" \
        --guest-link relay \
        --env "MACHINE_SERVICE_RELAY_GUEST_PORT=$agent_port" \
        --env "MACHINE_SERVICE_GRACE_HOURS=$grace"
    if [ -n "${RELAY_HOST_RELAY_URL:-}" ]; then
        set -- "$@" --env "MACHINE_SERVICE_RELAY_URL=$RELAY_HOST_RELAY_URL"
    fi
    if [ -n "${PILOT_REPO:-}" ]; then
        set -- "$@" --env "PILOT_REPO=$PILOT_REPO"
    fi
    if [ -n "${RELAY_HOST_DURABLE:-}" ]; then
        set -- "$@" --durable
        if [ -n "${RELAY_HOST_DURABLE_CELL:-}" ]; then
            set -- "$@" --durable-cell "$RELAY_HOST_DURABLE_CELL"
        fi
    fi
    "$launcher" "$@"
}

current_rig() {
    [ -f "$rigs/current" ] && cat "$rigs/current"
}

rig_pid() {
    run="$(current_rig)" || return 1
    [ -f "$run/serve.json" ] || return 1
    pid="$(sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$run/serve.json" | head -1)"
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && printf '%s\n' "$pid"
}

rig_stop() {
    if pid="$(rig_pid)"; then
        kill "$pid"
        i=0
        while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 100 ]; do
            sleep 0.1
            i=$((i + 1))
        done
        echo "agent chain stopped: pid $pid"
    fi
    rm -f "$rigs/current"
}

# The broker's address: the instance's one bind address.
broker_address() {
    case "$bind" in
    *:*) printf '[%s]:%s\n' "$bind" "$1" ;;
    *) printf '%s:%s\n' "$bind" "$1" ;;
    esac
}

rig_start() {
    profile="${1:?usage: rig-start <profile> <seed>}"
    seed="${2:?usage: rig-start <profile> <seed>}"
    session_port="$(need RELAY_HOST_SESSION_PORT "its broker's own port")"
    agent_port="$(need RELAY_HOST_AGENT_PORT "the agent chain's port")"
    rig_stop >/dev/null
    tls="$root/state/relay/tls"
    if [ ! -f "$tls/ca.pem" ]; then
        echo "no broker authority at $tls; start the instance first" >&2
        exit 1
    fi
    run="$rigs/$(date +%Y%m%d-%H%M%S)-$profile-$seed"
    mkdir -p "$run"
    nohup perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!"' \
        "$dir/network-rig" serve --profile "$profile" --seed "$seed" \
        --upstream "$(broker_address "$session_port")" --upstream-ca "$tls/ca.pem" \
        --edge-ca-key "$tls/ca.key" --edge-ca-cert "$tls/ca.pem" --edge-ca-name "$tls/ca.name" \
        --edge-host-addresses \
        --broker-name "$agent_name" --listen "0.0.0.0:$agent_port" --admit guests \
        --run-dir "$run" >"$run/serve.out" 2>&1 </dev/null &
    echo "$run" >"$rigs/current"
    i=0
    until [ -f "$run/serve.json" ]; do
        i=$((i + 1))
        if [ "$i" -ge 1200 ]; then
            echo "the agent chain did not start; see $run/serve.out" >&2
            exit 1
        fi
        sleep 0.25
    done
    cat "$run/serve.json"
}

case "${1:-}" in
start) start_instance ;;
stop)
    rig_stop
    "$launcher" stop --root "$root"
    ;;
restart) "$launcher" restart --root "$root" ;;
status)
    "$launcher" status --root "$root"
    if rig_pid >/dev/null; then echo "agent chain: running, $(current_rig)"; else echo "agent chain: stopped"; fi
    ;;
exposure) "$launcher" exposure --root "$root" ;;
rig-start) rig_start "${2:-}" "${3:-}" ;;
rig-stop) rig_stop ;;
rig-status)
    if rig_pid >/dev/null; then cat "$(current_rig)/serve.json"; else
        echo "agent chain: stopped" >&2
        exit 1
    fi
    ;;
rig-ctl)
    run="$(current_rig)" || {
        echo "agent chain: stopped" >&2
        exit 1
    }
    exec "$dir/network-rig" ctl --run-dir "$run" "${2:?usage: rig-ctl <json>}"
    ;;
clean)
    rig_stop
    "$launcher" stop --root "$root"
    rm -rf "$rigs" "$dir/cache"
    echo "cleaned $dir"
    ;;
*)
    echo "usage: relay-acceptance-host.sh start | stop | restart | status | exposure | clean | rig-start <profile> <seed> | rig-stop | rig-ctl <json> | rig-status" >&2
    exit 2
    ;;
esac
