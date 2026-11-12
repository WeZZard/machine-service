#!/bin/sh
# One relay acceptance cell in an ephemeral runner VM
# (docs/arch/lab-runners.md, LR-9). The runner VM reaches GitHub and its
# run's test environments only; its plan, LAB_PLAN, names them, and the
# client executables come from the workflow's build job, so nothing here
# builds or fetches.
#
#   lab-cell.sh mask                       print every name and address of the plan
#   lab-cell.sh run <cell> <profiles> <client dir> <evidence dir>
#   lab-cell.sh clean                      release this run's leases on its test environments
#   lab-cell.sh redact                     filter: replace the plan's names and addresses
#   lab-cell.sh scrub <from> <to>          copy reports and logs, redacted
#
# A VM cell's target is the run's test environment on the runner's own
# host; a durable cell's is the run's test environment on a durable Mac.
# Every lease the cell makes has a purpose that starts with acc-r<run>-, so
# `clean` finds them; stopping a test environment releases the rest.
set -eu
plan="${LAB_PLAN:?LAB_PLAN names no plan; this runs only in a lab runner VM}"
run="${GITHUB_RUN_ID:?}"

json() {
    python3 -I -c "import json, sys; plan = json.load(open(sys.argv[1])); $1" "$plan"
}

prefix() {
    printf 'acc-r%s-' "$run"
}

mask() {
    json '
seen = set()
for item in plan.get("instances", []):
    for value in (item["name"], item["name"] + ".local", item["name"].lower(), item["name"].lower() + ".local", item["address"]):
        if value not in seen:
            seen.add(value)
            print(value)'
}

target_url() {
    case "$1" in
    *.durable-mac) json 'print((plan.get("durable_targets") or [{}])[0].get("url", ""))' ;;
    *) json 'print((plan.get("vm_target") or {}).get("url", ""))' ;;
    esac
}

image_of() {
    case "$1" in
    *.macos-vm) echo macos26 ;;
    *.ubuntu-vm) echo ubuntu2404 ;;
    *) echo "" ;;
    esac
}

run_cell() {
    cell="${1:?cell}"
    profiles="${2:?profiles}"
    client="${3:?client dir}"
    evidence="${4:?evidence dir}"
    url="$(target_url "$cell")"
    if [ -z "$url" ]; then
        echo "the plan names no test environment for $cell" >&2
        exit 1
    fi
    image="$(image_of "$cell")"
    export NETWORK_RIG_CACHE="$client/rig-cache"
    chmod +x "$client/relay-acceptance"
    mkdir -p "$evidence"
    status=0
    for profile in $(printf '%s' "$profiles" | tr ',' ' '); do
        purpose="$(prefix)$(printf '%s-%s' "$cell" "$profile" | tr 'A-Z.' 'a-z-')"
        set -- run --cell "$cell" --profile "$profile" --instance "$url" \
            --agent-rig external --purpose "$purpose" --evidence "$evidence"
        if [ -n "$image" ]; then
            set -- "$@" --image "$image"
        fi
        echo "cell $cell under $profile"
        "$client/relay-acceptance" "$@" || status=1
    done
    "$client/relay-acceptance" summary "$evidence" || true
    return "$status"
}

clean() {
    want="$(prefix)"
    json 'print("\n".join(item["url"] for item in plan.get("instances", [])))' | while IFS= read -r url; do
        [ -n "$url" ] || continue
        curl -sS -m 30 "$url/machines" 2>/dev/null | python3 -I -c '
import json, sys
want = sys.argv[1]
try:
    machines = json.load(sys.stdin).get("machines") or {}
except ValueError:
    machines = {}
records = machines.values() if isinstance(machines, dict) else machines
for record in records:
    purpose = record.get("purpose") or ""
    if purpose.startswith(want):
        print("%s %s" % (record.get("vm") or record.get("name"), record.get("lease_id") or ""))' "$want" |
            while read -r vm lease; do
                body="{\"reason\": \"lab cleanup\""
                [ -n "$lease" ] && body="$body, \"lease_id\": \"$lease\""
                curl -sS -m 900 -X POST -H 'Content-Type: application/json' -d "$body}" "$url/machines/$vm/release" >/dev/null &&
                    echo "released $vm" || echo "could not release $vm" >&2
            done
    done
}

redact() {
    values="$(mask)"
    python3 -I -c '
import re, sys
values = sorted((v for v in sys.argv[1].split("\n") if v), key=len, reverse=True)
private = re.compile(r"\b(?:10|127)\.\d+\.\d+\.\d+\b|\b192\.168\.\d+\.\d+\b|\b172\.(?:1[6-9]|2\d|3[01])\.\d+\.\d+\b")
for line in sys.stdin:
    for value in values:
        line = line.replace(value, "<lab>")
    sys.stdout.write(private.sub("<private>", line))
    sys.stdout.flush()' "$values"
}

scrub() {
    from="${1:?from}"
    to="${2:?to}"
    mkdir -p "$to"
    [ -d "$from" ] || return 0
    (cd "$from" && find . -type f \( -name '*.json' -o -name '*.log' -o -name '*.txt' -o -name '*.jsonl' \)) |
        while IFS= read -r file; do
            mkdir -p "$to/$(dirname "$file")"
            redact <"$from/$file" >"$to/$file"
        done
}

case "${1:-}" in
mask) mask ;;
run) shift; run_cell "$@" ;;
clean) clean ;;
redact) redact ;;
scrub) shift; scrub "$@" ;;
*) echo "usage: $0 mask | run <cell> <profiles> <client dir> <evidence dir> | clean | redact | scrub <from> <to>" >&2; exit 2 ;;
esac
