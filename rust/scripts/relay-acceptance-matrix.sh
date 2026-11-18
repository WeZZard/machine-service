#!/bin/sh
# The relay acceptance matrix across the machines found at run time
# (docs/arch/test-layers.md, TL-7; DC-341 F, DC-343, DC-345, DC-346).
#
# `relay-acceptance plan` finds the instances that answer now and assigns
# each run of the matrix (matrix.json: six cells under four profiles) to one
# that fits, or reports it missing. This script carries that plan out, in
# four steps:
#
#   up    For every Mac the plan uses, put the host folder in ~/acc there
#         and start a second instance with the relay link on its LAN address
#         (ports 6262-6264, never the installed 6240/6241), and make the
#         restricted key that the Ubuntu client VMs' host control uses. A
#         durable Mac's instance is started by `run`, for the durable cell it
#         serves.
#   authorize
#         The owner's one setup command (DC-343): authorize the restricted
#         key on every Mac the plan uses, with its forced command, which runs
#         only the host script's host-control subcommands (DC-307). Without
#         it, an Ubuntu client cell's host-control checks cannot pass.
#   run   One queue per target Mac, the queues side by side and each queue's
#         runs one at a time in the plan's order, as one host's rig carries
#         one profile at a time. A macOS client cell runs here; an Ubuntu
#         client cell runs in a VM leased from its client Mac's second
#         instance (relay-acceptance-ubuntu-client.sh). Every run drives
#         mcp-relay (--client mcp) and each acquisition names its test
#         instance. Each run is capped at RELAY_MATRIX_RUN_HOURS.
#   down  Stop every second instance it started and remove the key's
#         authorization from every Mac.
#   all   up, run, then down; authorize is the owner's, run before `run`.
#
# Then `relay-acceptance matrix-summary` writes summary.json: each run's
# verdicts, the missing runs (an acceptance with a missing run is
# incomplete) and the checklist of permission prompts the runs approved on
# durable Macs (DC-340).
#
# Usage: relay-acceptance-matrix.sh up|authorize|run|down|all <work dir>
#
# The work dir holds plan.json, hosts/, host-key/, evidence/, logs/,
# revisions-<time>.json for each `run` and summary.json. Settings, from the environment:
#   RELAY_MATRIX_KEEP_EVIDENCE set: a passed run keeps its payloads and screenshots (DC-357)
#   RELAY_MATRIX_HOST_BUILD    the host folder's binaries, as
#                              relay-acceptance-host.sh lists them (required)
#   RELAY_MATRIX_MAC_CLIENT    relay-acceptance for this Mac (default: the
#                              host build's)
#   RELAY_MATRIX_LINUX_CLIENT  relay-acceptance for aarch64-unknown-linux-musl
#                              (required when the plan has an Ubuntu client)
#   RELAY_MATRIX_MCP_DIST      mcp-relay's dist folder (required)
#   RELAY_MATRIX_EXCLUDE       instances the plan leaves out, comma-separated
#   RELAY_MATRIX_ONLY          runs to carry out, as cell:profile,... (default:
#                              every assigned run)
#   RELAY_MATRIX_CHECKS        the checks each run runs, as relay-acceptance's
#                              --only takes them (default: every check)
#   RELAY_MATRIX_RUN_HOURS     each run's cap (default 3)
#   PILOT_REPO                 the image source, used on a Mac that has it
set -eu

step="${1:?usage: relay-acceptance-matrix.sh up|run|down|all <work dir>}"
work="${2:?usage: relay-acceptance-matrix.sh up|run|down|all <work dir>}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
scripts="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$scripts/../.." && pwd)"
crate="$repo/rust/crates/relay-acceptance"
host_build="${RELAY_MATRIX_HOST_BUILD:-}"
mac_client="${RELAY_MATRIX_MAC_CLIENT:-$host_build/relay-acceptance}"
cap_hours="${RELAY_MATRIX_RUN_HOURS:-3}"
port=6262
session_port=6263
agent_port=6264
folder=acc
key="$work/host-key/id_ed25519"

py() { python3 -I -c "$@"; }

# Run a command on a host: here when the host is this Mac, over SSH
# otherwise.
on() {
    dest="$1"
    shift
    if [ "$dest" = local ]; then
        /bin/sh -c "$*"
    else
        ssh -o BatchMode=yes -o ConnectTimeout=10 "$dest" "$*"
    fi
}

# One field of a host's record, hosts/<instance>.json.
host() { py 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$work/hosts/$1.json" "$2"; }

need_plan() {
    [ -f "$work/plan.json" ] || { echo "no plan in $work: run up first" >&2; exit 2; }
}

plan() {
    set -- plan
    [ -n "${RELAY_MATRIX_EXCLUDE:-}" ] && set -- "$@" --exclude "$RELAY_MATRIX_EXCLUDE"
    "$mac_client" "$@" > "$work/plan.json"
    py 'import json,sys; d=json.load(open(sys.argv[1]))
for a in d["assignments"]: print(a["cell"], a["profile"], a["target_instance"] or "-", a["client_instance"] or "-", a["missing"] or "")
print("missing:", d["missing"])' "$work/plan.json"
}

# The Macs the plan uses, as `instance address durable` lines.
used_hosts() {
    py 'import json,sys; d=json.load(open(sys.argv[1]))
used = {a["target_instance"] for a in d["assignments"] if a["target_instance"]} | {a["client_instance"] for a in d["assignments"] if a["client_instance"]}
for f in d["found"]:
    if f["instance"] in used: print(f["instance"], f["address"], "durable" if f.get("durable") else "vm", "local" if f.get("local") else "remote")' "$work/plan.json"
}

write_env() {
    # write_env <dest> <hostdir> <bind> <tart home> <durable cell or ->
    dest="$1" hostdir="$2" bind="$3" tart="$4" durable_cell="$5"
    pilot=""
    if [ -n "${PILOT_REPO:-}" ] && on "$dest" "test -d '$PILOT_REPO'"; then
        pilot="PILOT_REPO=$PILOT_REPO"
    fi
    durable=""
    [ "$durable_cell" != - ] && durable="RELAY_HOST_DURABLE=1
RELAY_HOST_DURABLE_CELL=$durable_cell"
    on "$dest" "mkdir -p '$tart' && cat > '$hostdir/host.env'" <<EOF
RELAY_HOST_PORT=$port
RELAY_HOST_SESSION_PORT=$session_port
RELAY_HOST_AGENT_PORT=$agent_port
RELAY_HOST_TART_HOME=$tart
RELAY_HOST_BIND=$bind
$pilot
$durable
EOF
}

start_instance() {
    # start_instance <instance> [<durable cell>]
    dest="$(host "$1" dest)" hostdir="$(host "$1" hostdir)"
    if [ "$(host "$1" kind)" = durable ]; then
        write_env "$dest" "$hostdir" "$(host "$1" bind)" "$hostdir/tart-empty" "${2:?a durable instance serves one durable cell}"
    else
        write_env "$dest" "$hostdir" "$(host "$1" bind)" "$(host "$1" home)/.tart" -
    fi
    # Each start is logged apart, and a failed one is tried once more after a
    # pause, as a previous instance may still be releasing its machine.
    log="$work/logs/start-$1-$(date +%H%M%S).log"
    started=no
    for attempt in 1 2; do
        on "$dest" "RELAY_HOST_DIR='$hostdir' perl -e 'alarm 900; exec @ARGV' /bin/sh '$hostdir/relay-acceptance-host.sh' stop" >> "$log" 2>&1 < /dev/null || true
        if on "$dest" "RELAY_HOST_DIR='$hostdir' perl -e 'alarm 900; exec @ARGV' /bin/sh '$hostdir/relay-acceptance-host.sh' start" >> "$log" 2>&1 < /dev/null; then
            started=yes
            break
        fi
        echo "start attempt $attempt failed" >> "$log"
        sleep 30
    done
    [ "$started" = yes ] || { echo "the second instance did not start on $1; see $log" >&2; return 1; }
    url="$(host "$1" url)"
    id=$(curl -fsS -m 20 "$url/health" | py 'import json,sys; print((json.load(sys.stdin).get("second_instance") or {}).get("id",""))')
    [ -n "$id" ] || { echo "$url does not answer as a second instance" >&2; return 1; }
    echo "second instance $id on $1 at $url${2:+ for $2}"
}

authorize() {
    # The restricted key: the forced command runs only the host script's
    # host-control subcommands, without a shell (DC-307).
    dest="$1" hostdir="$2"
    pub="$(cat "$key.pub")"
    on "$dest" "touch ~/.ssh/authorized_keys && grep -qF '$pub' ~/.ssh/authorized_keys || { cp -p ~/.ssh/authorized_keys ~/.ssh/authorized_keys.relay-acceptance-backup && printf '%s\n' 'restrict,command=\"/usr/bin/python3 -I $hostdir/relay-acceptance-host-control.py\" $pub' >> ~/.ssh/authorized_keys; }"
}

deauthorize() {
    dest="$1"
    comment="$(cut -d' ' -f3 "$key.pub")"
    on "$dest" "[ -f ~/.ssh/authorized_keys ] && grep -v ' $comment\$' ~/.ssh/authorized_keys > ~/.ssh/authorized_keys.relay-acceptance-new; [ -f ~/.ssh/authorized_keys.relay-acceptance-new ] && cat ~/.ssh/authorized_keys.relay-acceptance-new > ~/.ssh/authorized_keys && rm -f ~/.ssh/authorized_keys.relay-acceptance-new; ! grep -q ' $comment\$' ~/.ssh/authorized_keys"
}

up() {
    [ -n "$host_build" ] && [ -x "$mac_client" ] || { echo "set RELAY_MATRIX_HOST_BUILD to the host folder's binaries" >&2; exit 2; }
    mkdir -p "$work/hosts" "$work/logs" "$work/host-key"
    plan
    if [ ! -f "$key" ]; then
        ssh-keygen -q -t ed25519 -N '' -C "relay-acceptance-matrix-$(date -u +%Y%m%dT%H%M%SZ)" -f "$key"
    fi
    used_hosts > "$work/hosts.list"
    # The loops read their list from descriptor 3, so a command in them that
    # reads its input, such as ssh, never takes the list's next lines.
    while read -r instance address kind where <&3; do
        ip=$(printf %s "$address" | sed -E 's#^[a-z]+://##; s#:[0-9]+/?$##')
        if [ "$where" = local ]; then
            dest=local
            ip=$(ipconfig getifaddr "$(route -n get default | awk '/interface:/ {print $2}')")
        else
            dest="$ip"
        fi
        name="$(on "$dest" 'scutil --get LocalHostName')"
        home="$(on "$dest" 'printf %s "$HOME"')"
        user="$(on "$dest" 'id -un')"
        hostdir="$home/$folder"
        if [ "$dest" = local ]; then
            mkdir -p "$hostdir"
            [ "$(cd "$host_build" && pwd)" = "$hostdir" ] || rsync -a --exclude instance --exclude agent-rig --exclude cache --exclude host.env --exclude tart-empty "$host_build/" "$hostdir/"
            cp "$scripts/relay-acceptance-host.sh" "$scripts/relay-acceptance-host-control.py" "$hostdir/"
        else
            on "$dest" "mkdir -p '$hostdir'"
            rsync -a -e 'ssh -o BatchMode=yes' --exclude instance --exclude agent-rig --exclude cache --exclude host.env --exclude tart-empty \
                "$host_build/" "$scripts/relay-acceptance-host.sh" "$scripts/relay-acceptance-host-control.py" "$dest:$hostdir/"
        fi
        py 'import json,sys; k=["instance","dest","ip","bind","name","home","user","hostdir","kind","url"]; json.dump(dict(zip(k,sys.argv[2:])), open(sys.argv[1],"w"), indent=2)' \
            "$work/hosts/$instance.json" "$instance" "$dest" "$ip" "$ip" "$name" "$home" "$user" "$hostdir" "$kind" "http://$name.local:$port"
        echo "host $instance ready: $kind, folder ~/$folder"
        if [ "$kind" = vm ]; then start_instance "$instance"; fi
    done 3< "$work/hosts.list"
    start_client_copies
}

# The Ubuntu client VMs are leased from a second instance of their own on
# their client Mac (~/accc, ports 6272-6274), whose agent chain is started
# once under N0 and never stopped: a client VM's relay agent dials its
# instance's agent chain, and the target instance's chain is started and
# stopped for each run's profile, which took the clients' relay agents with
# it (2026-11-16, three runs held up until their cap).
client_port=6272
start_client_copies() {
    py 'import json,sys; d=json.load(open(sys.argv[1]))
for c in sorted({a["client_instance"] for a in d["assignments"] if a["client_instance"]}): print(c)' "$work/plan.json" > "$work/clients.list"
    while read -r instance <&3; do
        dest="$(host "$instance" dest)" home="$(host "$instance" home)" name="$(host "$instance" name)" bind="$(host "$instance" bind)"
        copy="$home/accc"
        on "$dest" "mkdir -p '$copy' && rsync -a --exclude instance --exclude agent-rig --exclude cache --exclude host.env --exclude tart-empty '$(host "$instance" hostdir)/' '$copy/'" < /dev/null
        pilot=""
        if [ -n "${PILOT_REPO:-}" ] && on "$dest" "test -d '$PILOT_REPO'" < /dev/null; then pilot="PILOT_REPO=$PILOT_REPO"; fi
        on "$dest" "cat > '$copy/host.env'" <<EOF
RELAY_HOST_PORT=$client_port
RELAY_HOST_SESSION_PORT=$((client_port + 1))
RELAY_HOST_AGENT_PORT=$((client_port + 2))
RELAY_HOST_TART_HOME=$home/.tart
RELAY_HOST_BIND=$bind
$pilot
EOF
        on "$dest" "RELAY_HOST_DIR='$copy' /bin/sh '$copy/relay-acceptance-host.sh' stop >/dev/null 2>&1; RELAY_HOST_DIR='$copy' perl -e 'alarm 900; exec @ARGV' /bin/sh '$copy/relay-acceptance-host.sh' start && RELAY_HOST_DIR='$copy' /bin/sh '$copy/relay-acceptance-host.sh' rig-start N0 1" > "$work/logs/client-copy-$instance.log" 2>&1 < /dev/null \
            || { echo "the client copy did not start on $instance; see logs/client-copy-$instance.log" >&2; return 1; }
        url="http://$name.local:$client_port"
        id=$(curl -fsS -m 20 "$url/health" | py 'import json,sys; print((json.load(sys.stdin).get("second_instance") or {}).get("id",""))')
        [ -n "$id" ] || { echo "$url does not answer as a second instance" >&2; return 1; }
        py 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["client_url"]=sys.argv[2]; d["client_dir"]=sys.argv[3]; json.dump(d, open(p,"w"), indent=2)' "$work/hosts/$instance.json" "$url" "$copy"
        echo "client copy $id on $instance at $url, its agent chain under N0"
    done 3< "$work/clients.list"
}

image_of() {
    py 'import json,sys; c=[c for c in json.load(open(sys.argv[1]))["cells"] if c["cell"]==sys.argv[2]][0]; print(c["target"].get("image",""))' "$crate/matrix.json" "$1"
}

# The revisions the runs prove: this repository's, mcp-relay's build, and
# what each second instance reports.
revisions() {
    {
        printf '{"vm_service": "%s", "mcp_relay_build": ' "$(git -C "$repo" rev-parse HEAD)"
        cat "$RELAY_MATRIX_MCP_DIST/build-info.json" 2>/dev/null || printf null
        printf ', "hosts": {'
        sep=""
        for f in "$work"/hosts/*.json; do
            instance=$(basename "$f" .json)
            printf '%s"%s": ' "$sep" "$instance"
            curl -fsS -m 20 "$(host "$instance" url)/health" 2>/dev/null || printf null
            sep=", "
        done
        printf '}}\n'
    } | py 'import json,sys; print(json.dumps(json.loads(sys.stdin.read()), indent=2))' > "$work/revisions-$(date -u +%Y%m%dT%H%M%SZ).json"
}

one_run() {
    # one_run <cell> <profile> <target> <client or ->
    cell="$1" profile="$2" target="$3" client="$4"
    log="$work/logs/$cell.$profile.log"
    image=$(image_of "$cell")
    set -- --cell "$cell" --profile "$profile" --instance "$(host "$target" url)"
    [ -n "$image" ] && set -- "$@" --image "$image"
    [ -n "${RELAY_MATRIX_CHECKS:-}" ] && set -- "$@" --only "$RELAY_MATRIX_CHECKS"
    purpose=$(printf 'acc-%s-%s-%s' "$revision" "$cell" "$profile" | tr 'A-Z.' 'a-z-' | cut -c1-64)
    hostdir="$(host "$target" hostdir)"
    echo "START $cell $profile on $target $(date '+%H:%M')"
    case "$cell" in
    macos-client.*)
        if [ "$(host "$target" dest)" = local ]; then shell="/bin/sh -c"; else shell="ssh -o BatchMode=yes $(host "$target" dest)"; fi
        rc=0
        NETWORK_RIG_CACHE="$work/rig-cache" perl -e "alarm $((cap_hours * 3600)); exec @ARGV" "$mac_client" run "$@" \
            --host-shell "$shell" --host-script "$hostdir/relay-acceptance-host.sh" --agent-rig manage --purpose "$purpose" \
            --client mcp --mcp-server "$RELAY_MATRIX_MCP_DIST/server.mjs" --evidence "$work/evidence" > "$log" 2>&1 || rc=$?
        ;;
    ubuntu-client.*)
        out="$work/ubuntu/$cell.$profile"
        rm -rf "$out"
        rc=0
        if [ -f "$work/host-key/authorized" ]; then
            set -- "$@" --host-shell "ssh -i @HOST_KEY@ -o StrictHostKeyChecking=accept-new -o BatchMode=yes $(host "$target" user)@$(host "$target" ip)" \
                --host-script "$hostdir/relay-acceptance-host.sh" --agent-rig manage
        else
            # Without the key the client VM has no host control: this
            # runner starts the target's agent chain itself, for the run's
            # profile and seed, and the host-control checks report blocked
            # (DC-343).
            seed=$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')
            on "$(host "$target" dest)" "RELAY_HOST_DIR='$hostdir' /bin/sh '$hostdir/relay-acceptance-host.sh' rig-start $profile $((seed + 1))" > "$out.agent-rig.json" 2>&1 < /dev/null
            set -- "$@" --seed "$seed" --agent-rig external
        fi
        # A client in a VM, behind its own NAT, takes about twice as long
        # under a long-haul profile, so its cap is twice the run's.
        RELAY_ACCEPTANCE_HOST_KEY="$key" RELAY_ACCEPTANCE_MCP_DIST="$RELAY_MATRIX_MCP_DIST" \
            perl -e "alarm $((cap_hours * 2 * 3600)); exec @ARGV" /bin/sh "$scripts/relay-acceptance-ubuntu-client.sh" \
            "$(host "$client" client_url)" "${RELAY_MATRIX_LINUX_CLIENT:?set RELAY_MATRIX_LINUX_CLIENT}" "$out" "$@" \
            --purpose "$purpose" --client mcp --mcp-server @MCP_SERVER@ > "$log" 2>&1 || rc=$?
        [ -f "$work/host-key/authorized" ] || on "$(host "$target" dest)" "RELAY_HOST_DIR='$hostdir' /bin/sh '$hostdir/relay-acceptance-host.sh' rig-stop" >/dev/null 2>&1 < /dev/null || true
        [ -f "$out/run.log" ] && cat "$out/run.log" >> "$log"
        if [ -d "$out/$cell/$profile" ]; then
            mkdir -p "$work/evidence/$cell"
            rm -rf "$work/evidence/$cell/$profile"
            mv "$out/$cell/$profile" "$work/evidence/$cell/$profile"
        fi
        ;;
    esac
    verdicts=$(grep -oE ' AC-[0-9]+: (Passed|Failed|Blocked)' "$log" | sort -u | awk '{print $2}' | sort | uniq -c | tr -s ' \n' ' ' || true)
    case "$rc:$verdicts" in
    0:*Passed*) case "$verdicts" in *Failed* | *Blocked*) ;; *) prune_passed "$work/evidence/$cell/$profile" ;; esac ;;
    esac
    echo "RESULT $cell $profile on $target rc=$rc$verdicts $(date '+%H:%M')"
}

prune_passed() {
    # A run that passed every check keeps its report, logs, receipts and
    # records, and drops the bulk that only a failure would need (DC-357):
    # the generated transfer payloads of AC-57 and AC-58, whose sizes and
    # SHA-256 its report keeps, and its screenshots. pruned.tsv lists each
    # file removed with its size; the run's evidence packages no longer
    # verify whole. RELAY_MATRIX_KEEP_EVIDENCE=1 keeps everything.
    dir="$1"
    [ -d "$dir" ] && [ -z "${RELAY_MATRIX_KEEP_EVIDENCE:-}" ] || return 0
    find "$dir" -type f \( -path '*/extractions/ac5[78]-*' -o -path '*/acc58-push/*' -o -name '*.png' \) \
        -exec stat -f '%z	%N' {} + > "$dir/pruned.tsv" 2>/dev/null || true
    cut -f2 "$dir/pruned.tsv" | while IFS= read -r file; do rm -f "$file"; done
    awk -F'\t' '{t+=$1} END{printf "pruned %d files, %.1f GB, of a passed run\n", NR, t/1e9}' "$dir/pruned.tsv"
}

queue() {
    # Every run assigned to one target Mac, one at a time, in the plan's order.
    target="$1"
    current=""
    while read -r cell profile client <&3; do
        if [ "$(host "$target" kind)" = durable ] && [ "$cell" != "$current" ]; then
            start_instance "$target" "$cell" || { echo "RESULT $cell $profile on $target rc=instance-not-started"; continue; }
            current="$cell"
        fi
        one_run "$cell" "$profile" "$target" "$client" < /dev/null
    done 3< "$work/logs/queue-$target.list"
}

run() {
    need_plan
    # The revision each run's purpose names: this checkout's when `run`
    # starts, so a later checkout never renames the runs in flight.
    revision=$(git -C "$repo" rev-parse --short HEAD)
    [ -n "${RELAY_MATRIX_MCP_DIST:-}" ] && [ -f "$RELAY_MATRIX_MCP_DIST/server.mjs" ] || { echo "set RELAY_MATRIX_MCP_DIST to mcp-relay's dist folder" >&2; exit 2; }
    mkdir -p "$work/evidence" "$work/logs" "$work/ubuntu"
    revisions
    targets=$(py 'import json,sys; print(" ".join(sorted({a["target_instance"] for a in json.load(open(sys.argv[1]))["assignments"] if a["target_instance"]})))' "$work/plan.json")
    for target in $targets; do
        # A target with no run here is left alone, so a second `run` beside
        # a first one never touches the first one's queue.
        py 'import json,sys,os; d=json.load(open(sys.argv[1])); only=[o for o in os.environ.get("RELAY_MATRIX_ONLY","").split(",") if o]
for a in d["assignments"]:
    if a["target_instance"]==sys.argv[2] and (not only or a["cell"]+":"+a["profile"] in only):
        print(a["cell"], a["profile"], a["client_instance"] or "-")' "$work/plan.json" "$target" > "$work/logs/queue-$target.next"
        if [ -s "$work/logs/queue-$target.next" ]; then
            mv "$work/logs/queue-$target.next" "$work/logs/queue-$target.list"
            queue "$target" >> "$work/logs/queue-$target.log" 2>&1 &
        else
            rm -f "$work/logs/queue-$target.next"
        fi
    done
    wait
    cat "$work"/logs/queue-*.log | grep -E '^(START|RESULT)' || true
    "$mac_client" matrix-summary --plan "$work/plan.json" "$work/evidence" > "$work/summary.json"
    echo "summary: $work/summary.json"
}

down() {
    need_plan
    for f in "$work"/hosts/*.json; do
        [ -f "$f" ] || continue
        instance=$(basename "$f" .json)
        dest="$(host "$instance" dest)" hostdir="$(host "$instance" hostdir)"
        on "$dest" "RELAY_HOST_DIR='$hostdir' perl -e 'alarm 900; exec @ARGV' /bin/sh '$hostdir/relay-acceptance-host.sh' stop" > "$work/logs/stop-$instance.log" 2>&1 \
            && echo "second instance stopped on $instance" || echo "the second instance on $instance did not stop; see logs/stop-$instance.log"
        copy=$(py 'import json,sys; print(json.load(open(sys.argv[1])).get("client_dir",""))' "$f")
        if [ -n "$copy" ]; then
            on "$dest" "RELAY_HOST_DIR='$copy' /bin/sh '$copy/relay-acceptance-host.sh' rig-stop; RELAY_HOST_DIR='$copy' perl -e 'alarm 900; exec @ARGV' /bin/sh '$copy/relay-acceptance-host.sh' stop" > "$work/logs/stop-client-$instance.log" 2>&1 < /dev/null \
                && echo "client copy stopped on $instance" || echo "the client copy on $instance did not stop; see logs/stop-client-$instance.log"
        fi
        [ -f "$key.pub" ] && deauthorize "$dest" && echo "key removed from $instance"
    done
    rm -f "$work/host-key/authorized"
}

authorize_all() {
    need_plan
    for f in "$work"/hosts/*.json; do
        instance=$(basename "$f" .json)
        authorize "$(host "$instance" dest)" "$(host "$instance" hostdir)" < /dev/null && echo "key authorized on $instance"
    done
    touch "$work/host-key/authorized"
}

case "$step" in
up) up ;;
authorize) authorize_all ;;
run) run ;;
down) down ;;
all) up && run; down ;;
*) echo "usage: relay-acceptance-matrix.sh up|run|down|all <work dir>" >&2; exit 2 ;;
esac
