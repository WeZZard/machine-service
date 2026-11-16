#!/bin/sh
# Run one relay acceptance cell with its client on Ubuntu (TL-7): lease an
# ubuntu2404 VM as the client machine, put the static Linux build of
# `relay-acceptance` in it, run the cell there, bring its evidence home and
# release the VM.
#
# Usage: relay-acceptance-ubuntu-client.sh <client instance> <binary> <evidence dir> <run args...>
#   <client instance>  http://<name>:<port> of the instance that leases the client VM
#   <binary>           relay-acceptance built for aarch64-unknown-linux-musl
#   <run args>         the arguments of `relay-acceptance run`, without --evidence
#
# RELAY_ACCEPTANCE_HOST_KEY, when set, names a private key that the target
# Macs authorize only with a forced command for the host script (DC-307).
# The key is copied into the client VM's home, mode 600, and every
# @HOST_KEY@ in the run arguments becomes its path there, so a
# --host-shell such as "ssh -i @HOST_KEY@ ..." gives the client VM the host
# control that AC-61 and AC-64 need. The VM is deleted with the key at
# release.
#
# RELAY_ACCEPTANCE_MCP_DIST, when set, names mcp-relay's dist folder, the
# bundled MCP server that a --client mcp run drives (DC-345). It goes into the
# client VM as ~/mcp-relay/dist, whose Node.js the ubuntu2404 image carries,
# and every @MCP_SERVER@ in the run arguments becomes the path of its
# server.mjs there.
#
# The client instance leases a VM, so it must be a second instance, never
# the installed one: its port is neither 6240 nor 6241 and its /health names
# it a second instance, or nothing is leased.
#
# The client VM resolves host names with its own resolver. A NAT guest has no
# multicast DNS beyond its host, so the names the run uses (the target
# instance's and its relay_url's) are looked up here, at run time, and written
# into the VM's /etc/hosts; nothing is written down by hand.
set -eu

client_instance="${1:?usage: relay-acceptance-ubuntu-client.sh <client instance> <binary> <evidence dir> <run args...>}"
binary="${2:?binary}"
evidence="${3:?evidence dir}"
shift 3
mkdir -p "$evidence"

json() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }

case "$(printf %s "$client_instance" | sed -E 's#^[a-z]+://##; s#/.*$##; s#^.*:##')" in
6240|6241) echo "$client_instance is the installed instance's port; the client VM is leased only from a second instance" >&2; exit 2 ;;
esac
second=$(curl -fsS -m 20 "$client_instance/health" | json '(d.get("second_instance") or {}).get("id","")')
[ -n "$second" ] || { echo "$client_instance does not report itself a second instance in /health; nothing is leased" >&2; exit 2; }

exec_script() {
    body=$(python3 -c 'import json,sys; print(json.dumps({"script": sys.argv[1], "timeout": int(sys.argv[2])}))' "$2" "${3:-300}")
    curl -fsS -m "$(( ${3:-300} + 120 ))" -X POST "$client_instance/machines/$1/exec" \
        -H 'content-type: application/json' -d "$body"
}

cell=""
profile=""
prev=""
for arg in "$@"; do
    [ "$prev" = "--cell" ] && cell="$arg"
    [ "$prev" = "--profile" ] && profile="$arg"
    prev="$arg"
done

# A purpose is leased once per image, so each client VM names its cell, in
# the characters a purpose takes. The client VM gets no credential pack: it
# needs none, and a pack is for the environment it was made for (DC-283).
purpose=$(printf 'acc-client-%s-%s' "$cell" "$profile" | tr 'A-Z.' 'a-z-' | cut -c1-64)
echo "leasing the Ubuntu client VM from $client_instance"
answer=$(curl -fsS -m 1800 -X POST "$client_instance/acquire" -H 'content-type: application/json' \
    -d "{\"purpose\":\"$purpose\",\"image\":\"ubuntu2404\",\"env\":\"none\",\"ttl_hours\":8}")
vm=$(printf %s "$answer" | json 'd["vm"]')
lease=$(printf %s "$answer" | json 'd.get("lease_id","")')
echo "client VM: $vm"
release() {
    curl -fsS -m 600 -X POST "$client_instance/machines/$vm/release" -H 'content-type: application/json' \
        -d '{"reason":"relay acceptance client done"}' >/dev/null && echo "released $vm"
}
trap release EXIT
# A run stopped by its cap (SIGALRM) or a signal still releases its VMs.
trap 'exit 142' ALRM
trap 'exit 143' TERM INT

sha=$(shasum -a 256 "$binary" | cut -d' ' -f1)
curl -fsS -m 600 -X PUT "$client_instance/machines/$vm/files?lease_id=$lease&path=relay-acceptance" \
    -H 'content-type: application/octet-stream' -H "X-Content-SHA256: $sha" --data-binary "@$binary" >/dev/null

server_path=""
if [ -n "${RELAY_ACCEPTANCE_MCP_DIST:-}" ]; then
    dist_tar=$(mktemp)
    tar -cf "$dist_tar" -C "$RELAY_ACCEPTANCE_MCP_DIST" .
    dist_sha=$(shasum -a 256 "$dist_tar" | cut -d' ' -f1)
    curl -fsS -m 600 -X PUT "$client_instance/machines/$vm/files?lease_id=$lease&path=mcp-relay-dist.tar" \
        -H 'content-type: application/octet-stream' -H "X-Content-SHA256: $dist_sha" --data-binary "@$dist_tar" >/dev/null
    rm -f "$dist_tar"
    server_path=$(exec_script "$vm" 'mkdir -p ~/mcp-relay/dist && tar -xf ~/mcp-relay-dist.tar -C ~/mcp-relay/dist && node --version >/dev/null && echo "$HOME/mcp-relay/dist/server.mjs"' 120 | json 'd.get("output","")' | grep '/server\.mjs$' | tail -1)
    case "$server_path" in */server.mjs) ;; *) echo "could not place mcp-relay in $vm: $server_path" >&2; exit 1 ;; esac
fi

key_path=""
if [ -n "${RELAY_ACCEPTANCE_HOST_KEY:-}" ]; then
    key_sha=$(shasum -a 256 "$RELAY_ACCEPTANCE_HOST_KEY" | cut -d' ' -f1)
    curl -fsS -m 60 -X PUT "$client_instance/machines/$vm/files?lease_id=$lease&path=acc-host-key" \
        -H 'content-type: application/octet-stream' -H "X-Content-SHA256: $key_sha" \
        --data-binary "@$RELAY_ACCEPTANCE_HOST_KEY" >/dev/null
    key_path=$(exec_script "$vm" 'chmod 600 ~/acc-host-key && printf %s "$HOME/acc-host-key"' 60 | json 'd.get("output","")')
    [ -n "$key_path" ] || { echo "could not place the host-control key in $vm" >&2; exit 1; }
fi

# The names the run reaches: the target instance's host and, from its
# discovery, the broker's.
instance=""
prev=""
for arg in "$@"; do
    [ "$prev" = "--instance" ] && instance="$arg"
    prev="$arg"
done
names=$(printf %s "$instance" | sed -E 's#^[a-z]+://##; s#[:/].*$##')
relay=$(curl -fsS -m 20 "$instance/health" | json 'd["relay_url"]' | sed -E 's#^wss?://##; s#[:/].*$##')
names="$names $relay"
bridges=$(ifconfig 2>/dev/null | awk '/^bridge/ {b=1; next} /^[a-z]/ {b=0} b && /inet / {printf "%s ", $2}')
hosts=""
for name in $(printf '%s\n' $names | sort -u); do
    # A name of this host resolves to loopback and to its VM bridges too, in
    # an order that changes; in the VM the first is the VM itself and the
    # others are no listener's. The first address that is neither is the one.
    address=$(dscacheutil -q host -a name "$name" | awk -v skip="$bridges" '/^ip_address:/ && $2 !~ /^127\./ && index(" " skip " ", " " $2 " ") == 0 {print $2; exit}')
    [ -n "$address" ] || { echo "cannot resolve $name here" >&2; exit 1; }
    hosts="$hosts$address $name\n"
done

quoted=""
for arg in "$@"; do
    if [ -n "$key_path" ]; then
        arg=$(printf %s "$arg" | sed "s#@HOST_KEY@#$key_path#g")
    fi
    if [ -n "$server_path" ]; then
        arg=$(printf %s "$arg" | sed "s#@MCP_SERVER@#$server_path#g")
    fi
    quoted="$quoted '$(printf %s "$arg" | sed "s/'/'\\\\''/g")'"
done

# The run's command line goes into a file of its own, so an argument with
# spaces or quotes, such as a --host-shell, reaches the client whole: quoting
# it once more inside another quoted command would split it.
run_script=$(mktemp)
printf '#!/bin/sh\nexec "$HOME/relay-acceptance" run%s --evidence "$HOME/evidence" > "$HOME/evidence/run.log" 2>&1\n' "$quoted" > "$run_script"
run_sha=$(shasum -a 256 "$run_script" | cut -d' ' -f1)
curl -fsS -m 60 -X PUT "$client_instance/machines/$vm/files?lease_id=$lease&path=acc-run.sh" \
    -H 'content-type: application/octet-stream' -H "X-Content-SHA256: $run_sha" --data-binary "@$run_script" >/dev/null
rm -f "$run_script"
started=$(exec_script "$vm" "chmod +x ~/relay-acceptance; printf '$hosts' | sudo -n tee -a /etc/hosts >/dev/null; \
mkdir -p ~/evidence ~/rig-cache; systemd-run --user --unit relay-acceptance-run --collect \
--setenv=NETWORK_RIG_CACHE=\$HOME/rig-cache \
--setenv=RELAY_ACCEPTANCE_LONG_MINUTES=\${RELAY_ACCEPTANCE_LONG_MINUTES:-30} \
/bin/sh \$HOME/acc-run.sh && echo started" | json 'd.get("output","")')
case "$started" in *started*) ;; *) echo "the run did not start in $vm: $started" >&2; exit 1 ;; esac
echo "running $cell under $profile in $vm"

# Only an answer from the VM ends the wait: a poll that does not reach it,
# as when the client instance is slow to answer, says nothing of the run.
while :; do
    sleep 60
    state=$(exec_script "$vm" "systemctl --user is-active relay-acceptance-run 2>/dev/null || true; tail -1 ~/evidence/run.log" 120 \
        | json 'd.get("output","")' || true)
    if [ -z "$state" ]; then
        echo "the client VM did not answer; waiting"
        continue
    fi
    echo "$state" | tail -1
    case "$state" in active*|activating*) ;; *) break ;; esac
done

# The run's evidence holds the bulk test data of AC-57 and AC-58, gigabytes
# of random bytes whose hashes the checks already recorded; each file over
# 4 MiB is listed with its size and SHA-256 in pruned-files.txt and removed
# before the evidence comes home, so the copy carries the record, not the
# bytes.
exec_script "$vm" 'cd ~/evidence && find . -type f -size +4M | while read -r f; do printf "%s %s %s\n" "$(stat -c %s "$f")" "$(sha256sum "$f" | cut -d" " -f1)" "$f"; rm -f "$f"; done > pruned-files.txt; wc -l < pruned-files.txt' 1800 >/dev/null || echo "could not prune the bulk test data in $vm; copying it whole"

# Bring the evidence home as a tar stream.
curl -fsS -m 1800 --retry 5 --retry-delay 30 --retry-all-errors "$client_instance/machines/$vm/files?lease_id=$lease&path=evidence" -o "$evidence/ubuntu-client-evidence.tar"
tar -xf "$evidence/ubuntu-client-evidence.tar" -C "$evidence" && rm -f "$evidence/ubuntu-client-evidence.tar"
echo "evidence in $evidence"
