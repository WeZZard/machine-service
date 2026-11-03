#!/bin/sh
# The lab runner's job hooks (docs/arch/hygiene.md, HY-5), by which a job on
# a lab runner holds its durable Mac's lease for as long as it runs, as any
# other client of machine-service does, so it never shares the desktop with a
# lease.
#
#   job-started.sh          a link, ACTIONS_RUNNER_HOOK_JOB_STARTED: release a
#                           lease an earlier job of this runner left, then
#                           acquire this Mac's lease for purpose ci-<run id>,
#                           or fail the job at once when the Mac is leased
#   job-completed.sh        a link, ACTIONS_RUNNER_HOOK_JOB_COMPLETED: release
#                           the lease (sweep, inventory, purge) and keep its
#                           hygiene record
#
# The runner is a plain per-user LaunchAgent in the desktop session. macOS's
# local network privacy asks the operator once to let its programs reach the
# lab's other machines (DC-1xx-LAB-14).
#
# The lease is renewed every 10 minutes for as long as the job's
# Runner.Worker runs, by a detached loop, so a job of any length keeps it and
# a runner that dies leaves a lease that ends at its short TTL.
#
# The Mac's name comes from the local instance at run time. Nothing this
# script prints names the Mac, its address or the operator's home, because a
# hook's output is part of the job's public log; for the same reason the
# runner calls the hooks through links outside the home folder.
#
# State, in the runner's folder: _lab/lease.json (the current lease),
# _lab/heartbeat.pid, and _lab/hygiene/<lease id>.json (each released
# lease's hygiene record).
set -eu

# The runner calls the hooks through links: resolve them to find the runner.
runner="$(cd "$(dirname "$(realpath "$0")")" && pwd)"
lab="$runner/_lab"
ttl_hours=2
beat_seconds=600
umask 077
mkdir -p "$lab/hygiene"

json() { python3 -I -c 'import json,sys; d=json.load(sys.stdin); v=eval(sys.argv[1]); print("" if v is None else v)' "$1"; }

# This Mac's durable machine and its instance's address, from the local
# instance.
local_machine() {
    machctl list --local --kind durable --json | python3 -I -c '
import json, sys
for instance in json.load(sys.stdin):
    for machine in instance.get("machines") or []:
        if instance.get("local") and machine.get("kind") == "durable":
            print(machine["name"], instance["address"])
            sys.exit(0)
sys.exit(1)'
}

# Print a line with this Mac's name replaced, for the public log.
say() {
    printf '%s\n' "$*" | sed "s/${name:-this-mac-name}/this Mac/g"
}

release_lease() {
    # $1: the lease file. The hygiene record is kept beside the runner and
    # summarized for the log.
    lease_name="$(json 'd["name"]' <"$1")"
    lease_id="$(json 'd["lease_id"]' <"$1")"
    address="$(json 'd["address"]' <"$1")"
    if [ -f "$lab/heartbeat.pid" ]; then
        kill "$(cat "$lab/heartbeat.pid")" 2>/dev/null || true
        rm -f "$lab/heartbeat.pid"
    fi
    if ! out="$(machctl release "$lease_name" --lease-id "$lease_id" 2>&1)"; then
        name="$lease_name" say "lab lease $lease_id: release failed: $out" >&2
        return 1
    fi
    record="$lab/hygiene/$lease_id.json"
    curl -fsS -m 60 "$address/machines/$lease_name/leases/$lease_id/hygiene" >"$record" || true
    rm -f "$1"
    python3 -I - "$record" "$lease_id" <<'EOF'
import json, sys
path, lease = sys.argv[1], sys.argv[2]
try:
    record = json.load(open(path))
except Exception as error:
    print(f"lab lease {lease} released; its hygiene record was not read: {error}")
    sys.exit(0)
def count(key):
    value = record.get(key)
    return len(value) if isinstance(value, (list, dict)) else 0
relay = record.get("relay_records", "not reported")
relay = relay.get("status", relay) if isinstance(relay, dict) else relay
failures = [f.get("step") for f in record.get("failures") or [] if isinstance(f, dict)]
print(f"lab lease {lease} released: stopped {count('stopped')}, swept {count('swept')}, "
      f"spared {count('spared')} processes; relay records {relay}; "
      f"CI busy intervals {count('ci_busy_intervals')}; failed steps {failures or 'none'}")
EOF
}

started() {
    machine="$(local_machine)" || {
        echo "lab runner: this Mac's instance exposes no durable machine; a lab job runs only under its Mac's lease" >&2
        exit 1
    }
    name="${machine% *}"
    address="${machine#* }"
    if [ -f "$lab/lease.json" ]; then
        say "lab runner: releasing the lease an earlier job left"
        release_lease "$lab/lease.json" || true
        rm -f "$lab/lease.json"
    fi
    purpose="ci-${GITHUB_RUN_ID:?}"
    answer="$lab/acquire.json"
    if ! machctl acquire --machine "$name" --purpose "$purpose" --env none --ttl-hours "$ttl_hours" \
        >"$answer" 2>"$lab/acquire.err"; then
        say "lab runner: this Mac is leased, so this job does not run: $(cat "$lab/acquire.err")" >&2
        say "lab runner: a lab job never shares the desktop with a lease; run it again when the lease ends" >&2
        rm -f "$answer" "$lab/acquire.err"
        exit 1
    fi
    lease_id="$(json 'd["lease_id"]' <"$answer")"
    # The answer carries the lease's relay credential: keep only what
    # release needs.
    printf '{"name": "%s", "lease_id": "%s", "address": "%s", "purpose": "%s"}\n' \
        "$name" "$lease_id" "$address" "$purpose" >"$lab/lease.json"
    rm -f "$answer" "$lab/acquire.err"
    worker="$(worker_pid)"
    env -u RUNNER_TRACKING_ID nohup perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!"' \
        /bin/sh "$runner/lab-runner.sh" heartbeat "$worker" "$name" "$lease_id" \
        </dev/null >/dev/null 2>&1 &
    echo $! >"$lab/heartbeat.pid"
    echo "lab lease $lease_id acquired for $purpose on this Mac, renewed while the job runs"
}

# The job's Runner.Worker: the nearest ancestor of this hook that runs it.
worker_pid() {
    pid=$$
    while [ "$pid" -gt 1 ]; do
        if ps -o comm= -p "$pid" | grep -q 'Runner.Worker$'; then
            echo "$pid"
            return 0
        fi
        pid="$(ps -o ppid= -p "$pid" | tr -d ' ')"
    done
    echo $PPID
}

heartbeat() {
    worker="$1" name="$2" lease_id="$3"
    while sleep "$beat_seconds"; do
        kill -0 "$worker" 2>/dev/null || exit 0
        [ -f "$lab/lease.json" ] || exit 0
        machctl heartbeat "$name" --lease-id "$lease_id" --ttl-hours "$ttl_hours" >/dev/null 2>&1 || true
    done
}

completed() {
    if [ ! -f "$lab/lease.json" ]; then
        echo "lab runner: no lab lease to release"
        return 0
    fi
    name="$(json 'd["name"]' <"$lab/lease.json")"
    release_lease "$lab/lease.json"
}

case "$(basename "$0")" in
job-started*) started ;;
job-completed*) completed ;;
*)
    case "${1:-}" in
    heartbeat) shift; heartbeat "$@" ;;
    *) echo "usage: link as job-started.sh or job-completed.sh" >&2; exit 2 ;;
    esac
    ;;
esac
