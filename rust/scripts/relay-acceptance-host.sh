#!/bin/sh
# The host side of the relay acceptance suite (docs/arch/test-layers.md,
# TL-7): a second machine-service instance with the relay link, beside the
# installed one and without touching it, and the relay agents' network rig
# chain in front of its broker.
#
# Everything lives in one directory, RELAY_HOST_DIR (default: this script's
# directory), which holds the binaries (machine-service, its guest agents and
# network-rig, as scripts/build-guest-agents.sh and `cargo build -p
# network-rig` make them), the instance's state, configuration and logs, and
# the rig's tool cache, so `clean` leaves nothing behind.
#
# The instance runs detached in its own session, as scripts/live-instance.sh
# starts one, so stopping the shell that started it cannot signal the `tart
# run` processes of its VMs. Its guests' relay agents dial the agent chain,
# never the broker itself: MACHINE_SERVICE_RELAY_GUEST_PORT gives them the
# chain's port on the host's address on their network, as machine-service
# derives it (BK-5), and the chain's edge serves those addresses.
#
# Usage: relay-acceptance-host.sh start | stop | restart | status | clean
#        relay-acceptance-host.sh rig-start <profile> <seed> | rig-stop | rig-ctl <json> | rig-status
#
# Settings, from the environment or from $RELAY_HOST_DIR/host.env:
#   RELAY_HOST_PORT          the instance's HTTP port (default 6262)
#   RELAY_HOST_SESSION_PORT  its broker's port (default 6263)
#   RELAY_HOST_AGENT_PORT    the agent chain's port (default 6264)
#   RELAY_HOST_AGENT_NAME    the name the agent chain's edge serves beside the
#                            host's addresses (default: <LocalHostName>.local)
#   RELAY_HOST_RELAY_URL     the relay_url to advertise instead of the default
#   RELAY_HOST_DURABLE       1 to turn durable exposure on (TL-6)
#   PILOT_REPO               the image source, as the installed instance has it
set -eu

dir="$(cd "${RELAY_HOST_DIR:-$(dirname "$0")}" && pwd)"
# shellcheck disable=SC1091
[ -f "$dir/host.env" ] && . "$dir/host.env"
port="${RELAY_HOST_PORT:-6262}"
session_port="${RELAY_HOST_SESSION_PORT:-6263}"
agent_port="${RELAY_HOST_AGENT_PORT:-6264}"
agent_name="${RELAY_HOST_AGENT_NAME:-$(scutil --get LocalHostName 2>/dev/null || hostname -s).local}"
state="$dir/state"
pidfile="$dir/daemon.pid"
rigs="$dir/agent-rig"
export NETWORK_RIG_CACHE="$dir/cache"

running() {
    [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

detach() {
    nohup perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!"' "$@" </dev/null &
}

start_instance() {
    if running; then
        echo "already running: pid $(cat "$pidfile")"
        return 0
    fi
    mkdir -p "$state" "$dir/config"
    durable="${RELAY_HOST_DURABLE:-}"
    env PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        MACHINE_SERVICE_PORT="$port" MACHINE_SERVICE_HOST=0.0.0.0 \
        MACHINE_SERVICE_STATE="$state" MACHINE_SERVICE_CONFIG_DIR="$dir/config" \
        MACHINE_SERVICE_GUEST_LINK=relay MACHINE_SERVICE_SESSION_PORT="$session_port" \
        MACHINE_SERVICE_RELAY_GUEST_PORT="$agent_port" \
        ${RELAY_HOST_RELAY_URL:+MACHINE_SERVICE_RELAY_URL="$RELAY_HOST_RELAY_URL"} \
        ${durable:+MACHINE_SERVICE_EXPOSE_DURABLE=on} \
        MACHINE_SERVICE_DISCOVERY_SCOPE=local-only \
        ${PILOT_REPO:+PILOT_REPO="$PILOT_REPO"} \
        sh -c 'exec "$@" >>"'"$dir"'/daemon.out" 2>>"'"$dir"'/daemon.err"' sh \
        perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!"' "$dir/machine-service" \
        </dev/null &
    echo $! >"$pidfile"
    tries=0
    until curl -fsS -m 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; do
        tries=$((tries + 1))
        if [ "$tries" -ge 80 ] || ! running; then
            echo "the instance did not answer on port $port; see $dir/daemon.err" >&2
            exit 1
        fi
        sleep 0.25
    done
    echo "started: pid $(cat "$pidfile"), port $port, broker $session_port, guests dial the agent chain on port $agent_port"
}

stop_instance() {
    if running; then
        pid="$(cat "$pidfile")"
        kill "$pid"
        while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
        echo "stopped: pid $pid"
    fi
    rm -f "$pidfile"
}

current_rig() {
    [ -f "$rigs/current" ] && cat "$rigs/current"
}

rig_running() {
    run="$(current_rig)" || return 1
    [ -f "$run/serve.json" ] || return 1
    pid="$(sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$run/serve.json" | head -1)"
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

rig_stop() {
    if rig_running; then
        run="$(current_rig)"
        pid="$(sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$run/serve.json" | head -1)"
        kill "$pid"
        i=0
        while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 100 ]; do sleep 0.1; i=$((i + 1)); done
        echo "agent chain stopped: pid $pid"
    fi
    rm -f "$rigs/current"
}

rig_start() {
    profile="${1:?usage: rig-start <profile> <seed>}"
    seed="${2:?usage: rig-start <profile> <seed>}"
    rig_stop >/dev/null
    tls="$state/relay/tls"
    if [ ! -f "$tls/ca.pem" ]; then
        echo "no broker authority at $tls; start the instance first" >&2
        exit 1
    fi
    run="$rigs/$(date +%Y%m%d-%H%M%S)-$profile-$seed"
    mkdir -p "$run"
    detach "$dir/network-rig" serve --profile "$profile" --seed "$seed" \
        --upstream "127.0.0.1:$session_port" --upstream-ca "$tls/ca.pem" \
        --edge-ca-key "$tls/ca.key" --edge-ca-cert "$tls/ca.pem" --edge-ca-name "$tls/ca.name" \
        --edge-host-addresses \
        --broker-name "$agent_name" --listen "0.0.0.0:$agent_port" \
        --run-dir "$run" >"$run/serve.out" 2>&1
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
stop) stop_instance ;;
restart)
    stop_instance
    start_instance
    ;;
status)
    if running; then echo "instance: running, pid $(cat "$pidfile")"; else echo "instance: stopped"; fi
    if rig_running; then echo "agent chain: running, $(current_rig)"; else echo "agent chain: stopped"; fi
    ;;
rig-start) rig_start "${2:-}" "${3:-}" ;;
rig-stop) rig_stop ;;
rig-status)
    if rig_running; then cat "$(current_rig)/serve.json"; else echo "agent chain: stopped" >&2; exit 1; fi
    ;;
rig-ctl)
    run="$(current_rig)" || { echo "agent chain: stopped" >&2; exit 1; }
    exec "$dir/network-rig" ctl --run-dir "$run" "${2:?usage: rig-ctl <json>}"
    ;;
clean)
    rig_stop
    stop_instance
    rm -rf "$state" "$dir/config" "$rigs" "$dir/cache" "$dir/daemon.out" "$dir/daemon.err"
    echo "cleaned $dir"
    ;;
*)
    echo "usage: relay-acceptance-host.sh start | stop | restart | status | clean | rig-start <profile> <seed> | rig-stop | rig-ctl <json> | rig-status" >&2
    exit 2
    ;;
esac
