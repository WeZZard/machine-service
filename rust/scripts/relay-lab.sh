#!/bin/sh
# The relay acceptance (TL-7) on the lab runners: what .github/workflows/lab.yml
# runs on a durable Mac whose lease its job holds (docs/arch/hygiene.md HY-5,
# docs/arch/test-layers.md TL-7).
#
# The job's own Mac is the macOS client. Every target is found at run time
# from `machctl list`, never written down:
#   - the durable cells target the OTHER durable Mac, because this Mac's lease
#     is the job's own and a job never waits on it;
#   - the virtual machine cells target the VM host with the fewest running
#     macOS VMs, and the Ubuntu client VMs are leased from its installed
#     instance.
# On each target host the lab's own second instance runs from the folder the
# lab key's gate keeps (scripts/relay-lab-gate.py), on the lab's ports, beside
# the installed instance and any other agent's second instance.
#
# Usage: relay-lab.sh plan <dir>                write <dir>/plan.env, or fail fast
#        relay-lab.sh toolchain                 install or update the job's own Rust
#        relay-lab.sh build <dir>               build the host bundle and the clients
#        relay-lab.sh deploy <dir>              start the lab instances on the targets
#        relay-lab.sh run <dir> <cells> <profiles>
#                                               run the cells, each target's in turn,
#                                               the two targets at once
#        relay-lab.sh clean <dir>               stop and remove the lab instances
#        relay-lab.sh redact <names>            filter standard input for a public log
#        relay-lab.sh scrub <names> <from> <to> copy the evidence's text, redacted
#
# <cells> is `all` or a comma-separated list of cell IDs; <profiles> a
# comma-separated list of N0..N3. The SSH key is ~/.ssh/relay-lab_ed25519.
#
# Every command runs in the job's own environment, never the one it was
# started in: this script starts itself again under `env -i` with a HOME,
# TMPDIR, RUSTUP_HOME and CARGO_HOME of its own under the lab's folder,
# <runner work folder>/_lab (outside a runner, ~/.cache/relay-lab), and a
# PATH of the system's tools and that Rust only. So no VM_ENVIRONMENT_*,
# MACHINE_SERVICE_*, CODESIGN_IDENTITY or ZDOTDIR reaches a child, nothing
# reads the operator's configuration, and no global tool is changed; the
# Rust toolchain the build needs is the job's own, kept between jobs.
set -eu

if [ -z "${RELAY_LAB_OWN_ENV:-}" ]; then
    root="${RUNNER_TEMP:+$RUNNER_TEMP/../_lab}"
    root="${root:-$HOME/.cache/relay-lab}"
    mkdir -p "$root/home" "$root/tmp"
    root="$(cd "$root" && pwd)"
    exec env -i RELAY_LAB_OWN_ENV=1 RELAY_LAB_ROOT="$root" \
        RELAY_LAB_KEY="$HOME/.ssh/relay-lab_ed25519" RELAY_LAB_HOMES="$(dirname "$HOME")" \
        RELAY_LAB_MACHCTL="$(command -v machctl || echo machctl)" \
        HOME="$root/home" TMPDIR="$root/tmp/" USER="$(id -un)" LOGNAME="$(id -un)" LANG=en_US.UTF-8 \
        RUSTUP_HOME="$root/rustup" CARGO_HOME="$root/cargo" \
        PATH="$root/cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
        /bin/sh "$0" "$@"
fi

here="$(cd "$(dirname "$0")" && pwd)"
rust="$(cd "$here/.." && pwd)"
key="$RELAY_LAB_KEY"
machctl() { "$RELAY_LAB_MACHCTL" "$@"; }
# The lab instance's HTTP, broker and agent-chain ports on every target.
lab_ports="RELAY_HOST_PORT=6272 RELAY_HOST_SESSION_PORT=6273 RELAY_HOST_AGENT_PORT=6274"
all_cells="macos-client.macos-vm macos-client.ubuntu-vm ubuntu-client.macos-vm ubuntu-client.ubuntu-vm macos-client.durable-mac ubuntu-client.durable-mac"

die() {
    echo "relay-lab: $*" >&2
    exit 1
}

host_shell() {
    echo "ssh -i $key -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=20 -o ServerAliveInterval=30 $1.local"
}

# plan <dir>: this Mac, the other durable Mac and the VM host, from every
# instance's listing; refuse when a target is leased or has no room.
plan() {
    dir="$1"
    mkdir -p "$dir"
    # An instance busy with a release can miss the listing's 5 s, so the
    # listing is read again before the plan gives up.
    tries=0
    until machctl list --json >"$dir/list.json" && plan_from "$dir/list.json" >"$dir/plan.new" 2>"$dir/plan.err"; do
        tries=$((tries + 1))
        if [ "$tries" -ge 5 ]; then
            cat "$dir/plan.err" >&2
            rm -f "$dir/plan.new" "$dir/plan.err"
            exit 1
        fi
        sleep 15
    done
    mv "$dir/plan.new" "$dir/plan.env"
    rm -f "$dir/plan.err"
    echo "plan: this Mac is the macOS client; the durable cells target the other durable Mac; the VM cells one VM host"
}

plan_from() {
    python3 -I - "$1" <<'EOF'
import json, shlex, sys

instances = json.load(open(sys.argv[1]))
def fail(message):
    print(f"relay-lab: {message}", file=sys.stderr)
    sys.exit(1)

durables = [(i, m) for i in instances for m in i.get("machines") or [] if m.get("kind") == "durable"]
own = [(i, m) for i, m in durables if i.get("local")]
if len(own) != 1:
    fail("this Mac's instance exposes no durable machine; a lab job runs on a durable Mac")
own_name = own[0][1]["name"]
if own[0][1].get("purpose", "") and not str(own[0][1].get("purpose")).startswith("ci-"):
    fail("this Mac is leased to another client")
others = [(i, m) for i, m in durables if m["name"] != own_name]
if not others:
    silent = [i.get("error") for i in instances if not i.get("reachable", True)]
    fail(f"no other durable Mac answered; the durable cells need one (instances without an answer: {silent})")
free = [(i, m) for i, m in others if m.get("state") == "available" and not m.get("lease_id")]
if not free:
    i, m = others[0]
    fail(f"the other durable Mac is leased (purpose {m.get('purpose')}, until {m.get('ttl_expires_at')}); "
         "the lab backs off and runs when the lease ends")
durable = free[0][1]["name"]

hosts = []
for i in instances:
    offers = {m["name"]: m for m in i.get("machines") or [] if m.get("kind") == "virtual"}
    if not {"macos26", "ubuntu2404"} <= set(offers):
        continue
    macos = [m for m in offers.values() if (m.get("concurrency") or {}).get("limit")]
    running = sum((m.get("concurrency") or {}).get("running") or 0 for m in macos)
    acquirable = all((offers[n].get("concurrency") or {}).get("acquirable") for n in ("macos26", "ubuntu2404"))
    if acquirable and i.get("reachable", True):
        port = i["address"].rsplit(":", 1)[-1].strip("/")
        hosts.append((running, i["instance"], port))
if not hosts:
    fail("no VM host has room for a macOS VM and an Ubuntu VM; the lab backs off")
hosts.sort(key=lambda h: (h[0], h[1]))
_, vm_host, vm_port = hosts[0]
for key, value in (("OWN", own_name), ("DURABLE", durable), ("VM_HOST", vm_host),
                   ("CLIENT_INSTANCE", f"http://{vm_host}.local:{vm_port}")):
    print(f"{key}={shlex.quote(value)}")
EOF
}

load_plan() {
    [ -f "$1/plan.env" ] || die "no plan in $1; run plan first"
    # shellcheck disable=SC1091
    . "$1/plan.env"
}

musl_env() {
    llvm_bin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
    export CC_aarch64_unknown_linux_musl=clang
    export CFLAGS_aarch64_unknown_linux_musl="--target=aarch64-unknown-linux-gnu"
    export AR_aarch64_unknown_linux_musl="$llvm_bin/llvm-ar"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
}

# toolchain: the job's own stable Rust, with the Linux target and llvm-tools
# the musl builds need, installed by rustup-init checked against its
# published SHA-256 the first time and updated after.
toolchain() {
    if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
        url=https://static.rust-lang.org/rustup/dist/aarch64-apple-darwin/rustup-init
        curl -fsSL -o "$TMPDIR/rustup-init" "$url"
        want="$(curl -fsSL "$url.sha256" | cut -d' ' -f1)"
        got="$(shasum -a 256 "$TMPDIR/rustup-init" | cut -d' ' -f1)"
        [ -n "$want" ] && [ "$want" = "$got" ] || die "rustup-init: SHA-256 $got, not the published $want"
        chmod +x "$TMPDIR/rustup-init"
        "$TMPDIR/rustup-init" -y --no-modify-path --profile minimal --default-toolchain none
        rm -f "$TMPDIR/rustup-init"
    fi
    rustup toolchain install stable --profile minimal \
        --component rustfmt,clippy,llvm-tools --target aarch64-unknown-linux-musl
    rustup default stable
    echo "toolchain: $(rustc --version), the job's own"
}

# build <dir>: <dir>/host, the bundle every target runs (the installation's
# executables, the relay agent's release, network-rig and the host script),
# and <dir>/bin, the suite for this Mac and for an Ubuntu client.
build() {
    dir="$1"
    cd "$rust"
    target_dir="${CARGO_TARGET_DIR:-target}"
    rm -rf "$dir/host" "$dir/bin"
    mkdir -p "$dir/host" "$dir/bin"
    scripts/build-release.sh "$dir/host"
    cargo build --release -p network-rig -p relay-acceptance
    cp "$target_dir/release/network-rig" "$dir/host/network-rig"
    cp scripts/relay-acceptance-host.sh "$dir/host/relay-acceptance-host.sh"
    cp "$target_dir/release/relay-acceptance" "$dir/bin/relay-acceptance"
    (musl_env && cargo build --release -p relay-acceptance --target aarch64-unknown-linux-musl)
    cp "$target_dir/aarch64-unknown-linux-musl/release/relay-acceptance" "$dir/bin/relay-acceptance-linux"
    echo "built: $(du -sh "$dir/host" | cut -f1) for each target"
}

# deploy <dir>: the bundle to both targets through their gates, and the lab
# instance started on each, the durable Mac's with exposure on.
deploy() {
    dir="$1"
    load_plan "$dir"
    for target in "$VM_HOST" "$DURABLE"; do
        stage="$dir/stage-$(printf %s "$target" | shasum | cut -c1-8)"
        rm -rf "$stage"
        cp -R "$dir/host" "$stage"
        {
            for port in $lab_ports; do echo "$port"; done
            [ "$target" = "$DURABLE" ] && echo "RELAY_HOST_DURABLE=1"
            true
        } >"$stage/host.env"
        tar -czf - -C "$stage" . | $(host_shell "$target") deploy
        rm -rf "$stage"
        $(host_shell "$target") relay-acceptance-host.sh start
    done
}

clean() {
    dir="$1"
    [ -f "$dir/plan.env" ] || { echo "relay-lab: no plan, so nothing was deployed"; return 0; }
    load_plan "$dir"
    for target in "$VM_HOST" "$DURABLE"; do
        release_all "http://$target.local:6272"
        $(host_shell "$target") clean || echo "relay-lab: clean of a target failed" >&2
    done
    # The client side's network rigs, which a cancelled cell leaves running.
    pkill -TERM -f "^$HOME/Library/Caches/machine-service/network-rig/" 2>/dev/null || true
}

# Release every lease the lab instance still holds, so that a cancelled run
# leaves no VM running and no durable lease behind its removed state.
release_all() {
    curl -fsS -m 20 "$1/machines" 2>/dev/null | python3 -I -c '
import json, sys
for record in json.load(sys.stdin).get("machines") or []:
    if record.get("lease_id") and record.get("state") in ("pending", "provisioning", "running", "leased"):
        print(record["name"], record["lease_id"])' 2>/dev/null |
        while read -r name lease; do
            curl -fsS -m 600 -X POST "$1/machines/$name/release" -H 'content-type: application/json' \
                -d "{\"lease_id\":\"$lease\",\"reason\":\"relay lab cleanup\"}" >/dev/null &&
                echo "relay-lab: released a lease the lab instance still held"
        done
    return 0
}

# One cell under one profile; its report lands in <evidence>/<cell>/<profile>/.
cell() {
    dir="$1" cell="$2" profile="$3"
    evidence="$dir/evidence"
    logs="$evidence/logs"
    mkdir -p "$logs"
    case "$cell" in
    *.durable-mac) target="$DURABLE" image="" ;;
    *.macos-vm) target="$VM_HOST" image=macos26 ;;
    *.ubuntu-vm) target="$VM_HOST" image=ubuntu2404 ;;
    esac
    set -- --cell "$cell" --profile "$profile" --instance "http://$target.local:6272" \
        --purpose "lab-$(printf %s "$cell-$profile" | tr 'A-Z.' 'a-z-')"
    [ -n "$image" ] && set -- "$@" --image "$image"
    case "$cell" in
    macos-client.*)
        set -- "$@" --host-shell "$(host_shell "$target")" --host-script relay-acceptance-host.sh
        ;;
    ubuntu-client.*)
        # The client VM has no way to the target's host: this Mac runs the
        # agents' chain for it, and the scenarios that cut or restart on the
        # host have no host control there.
        set -- "$@" --agent-rig external
        ;;
    esac
    log="$logs/$cell-$profile.log"
    echo "[$cell $profile] started"
    status=0
    case "$cell" in
    macos-client.*)
        "$dir/bin/relay-acceptance" run "$@" --evidence "$evidence" >"$log" 2>&1 || status=$?
        ;;
    ubuntu-client.*)
        stage="$dir/uc-$cell-$profile"
        rm -rf "$stage"
        if [ -n "$image" ]; then
            seed="$(od -An -N8 -tu8 /dev/urandom | tr -d ' ')"
            $(host_shell "$target") relay-acceptance-host.sh rig-start "$profile" "$seed" >"$logs/$cell-$profile.agent-rig.json" ||
                status=1
        fi
        sh "$here/relay-acceptance-ubuntu-client.sh" "$CLIENT_INSTANCE" "$dir/bin/relay-acceptance-linux" \
            "$stage" "$@" >"$log" 2>&1 || status=$?
        if [ -d "$stage/$cell/$profile" ]; then
            mkdir -p "$evidence/$cell"
            rm -rf "${evidence:?}/$cell/$profile"
            mv "$stage/$cell/$profile" "$evidence/$cell/$profile"
        fi
        [ -f "$stage/run.log" ] && mv "$stage/run.log" "$logs/$cell-$profile.client.log"
        rm -rf "$stage"
        [ -z "$image" ] || $(host_shell "$target") relay-acceptance-host.sh rig-stop >/dev/null || true
        [ -f "$evidence/$cell/$profile/report.json" ] || status=1
        ;;
    esac
    grep -E "^$cell AC-[0-9]+: " "$log" "$logs/$cell-$profile.client.log" 2>/dev/null | sed 's/^[^:]*://' | sort -u || true
    echo "[$cell $profile] ended with status $status"
    return "$status"
}

# A lane runs its cells in turn, profile by profile.
lane() {
    dir="$1" cells="$2" profiles="$3"
    failed=0
    for profile in $profiles; do
        for c in $cells; do
            cell "$dir" "$c" "$profile" || failed=1
        done
    done
    return "$failed"
}

run() {
    dir="$1"
    load_plan "$dir"
    wanted="$(printf %s "$2" | tr ',' ' ')"
    [ "$wanted" = all ] && wanted="$all_cells"
    profiles="$(printf %s "$3" | tr ',' ' ')"
    for c in $wanted; do
        case " $all_cells " in *" $c "*) ;; *) die "unknown cell $c" ;; esac
    done
    for p in $profiles; do
        case "$p" in N0 | N1 | N2 | N3) ;; *) die "unknown profile $p" ;; esac
    done
    vm_cells="" durable_cells=""
    for c in $all_cells; do
        case " $wanted " in *" $c "*) ;; *) continue ;; esac
        case "$c" in
        *.durable-mac) durable_cells="$durable_cells $c" ;;
        *) vm_cells="$vm_cells $c" ;;
        esac
    done
    status=0
    # Each lane is a process group of its own. A cancelled step signals the
    # step's shell, or only ends it, so this script stops both lanes and all
    # they started when it is signalled or orphaned, and nothing goes on
    # leasing after the job.
    set -m
    lane "$dir" "$vm_cells" "$profiles" &
    vm_lane=$!
    lane "$dir" "$durable_cells" "$profiles" &
    durable_lane=$!
    set +m
    trap 'trap - INT TERM HUP PIPE; kill -TERM -"$vm_lane" -"$durable_lane" 2>/dev/null; wait; exit 143' INT TERM HUP PIPE
    while kill -0 "$vm_lane" 2>/dev/null || kill -0 "$durable_lane" 2>/dev/null; do
        if [ "$(ps -o ppid= -p $$ | tr -d ' ')" = 1 ]; then
            kill -TERM -"$vm_lane" -"$durable_lane" 2>/dev/null || true
            wait
            exit 143
        fi
        sleep 5
    done
    wait "$vm_lane" || status=1
    wait "$durable_lane" || status=1
    "$dir/bin/relay-acceptance" summary "$dir/evidence" | tee "$dir/evidence/summary.txt"
    return "$status"
}

# The redaction every public output passes through: the names and addresses
# the runner discovered (one per line in <names>: names, addresses and DNS
# search domains), the lab's private range, home folders and the login.
redactor() {
    cat <<'EOF'
import os, re, sys

def redactor(names_file):
    values = sorted({line.strip() for line in open(names_file) if line.strip()}, key=len, reverse=True)
    login = os.environ.get("USER") or os.path.basename(os.path.expanduser("~"))
    rules = [(re.compile(re.escape(v), re.IGNORECASE), f"<lab-{i}>") for i, v in enumerate(values)]
    rules += [
        (re.compile(re.escape(os.environ.get("RELAY_LAB_HOMES") or os.path.dirname(os.path.expanduser("~"))) + r"/[^/\s\"']+"), "~"),
        (re.compile(r"\b192\.168\.\d{1,3}\.\d{1,3}\b"), "<lan-address>"),
        (re.compile(re.escape(login) + "@"), "<user>@"),
    ]
    def apply(text):
        for pattern, replacement in rules:
            text = pattern.sub(replacement, text)
        return text
    return apply
EOF
}

redact() {
    python3 -I -c "$(redactor)
import sys
apply = redactor(sys.argv[1])
for line in sys.stdin:
    sys.stdout.write(apply(line))
    sys.stdout.flush()" "$1"
}

# scrub <names> <from> <to>: the evidence's text files, redacted; no
# certificate, key, image or copied tool goes public.
scrub() {
    python3 -I -c "$(redactor)
import os, shutil, sys
apply = redactor(sys.argv[1])
src, dst = sys.argv[2], sys.argv[3]
kept = skipped = 0
for root, dirs, files in os.walk(src):
    dirs[:] = [d for d in dirs if d not in ('mitmproxy', 'cache')]
    for name in files:
        path = os.path.join(root, name)
        if not name.endswith(('.json', '.jsonl', '.log', '.txt', '.out', '.err')):
            skipped += 1
            continue
        out = os.path.join(dst, apply(os.path.relpath(path, src)))
        os.makedirs(os.path.dirname(out), exist_ok=True)
        with open(path, errors='replace') as reader, open(out, 'w') as writer:
            for line in reader:
                writer.write(apply(line))
        kept += 1
print(f'scrubbed {kept} text files; left out {skipped} others (images, certificates, keys, copied tools)')
" "$1" "$2" "$3"
}

case "${1:-}" in
plan) plan "${2:?dir}" ;;
toolchain) toolchain ;;
build) build "${2:?dir}" ;;
deploy) deploy "${2:?dir}" ;;
run) run "${2:?dir}" "${3:-all}" "${4:-N0}" ;;
clean) clean "${2:?dir}" ;;
redact) redact "${2:?names}" ;;
scrub) scrub "${2:?names}" "${3:?from}" "${4:?to}" ;;
*)
    sed -n '/^# Usage:/,/^# <cells>/p' "$0" >&2
    exit 2
    ;;
esac
