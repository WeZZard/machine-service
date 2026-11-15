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
# revisions.json and summary.json. Settings, from the environment:
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
    on "$dest" "RELAY_HOST_DIR='$hostdir' perl -e 'alarm 900; exec @ARGV' /bin/sh '$hostdir/relay-acceptance-host.sh' stop >/dev/null 2>&1 || true"
    on "$dest" "RELAY_HOST_DIR='$hostdir' perl -e 'alarm 900; exec @ARGV' /bin/sh '$hostdir/relay-acceptance-host.sh' start" > "$work/logs/start-$1.log" 2>&1 \
        || { echo "the second instance did not start on $1; see logs/start-$1.log" >&2; return 1; }
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
    purpose=$(printf 'acc-%s-%s-%s' "$(git -C "$repo" rev-parse --short HEAD)" "$cell" "$profile" | tr 'A-Z.' 'a-z-' | cut -c1-64)
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
        RELAY_ACCEPTANCE_HOST_KEY="$key" RELAY_ACCEPTANCE_MCP_DIST="$RELAY_MATRIX_MCP_DIST" \
            perl -e "alarm $((cap_hours * 3600)); exec @ARGV" /bin/sh "$scripts/relay-acceptance-ubuntu-client.sh" \
            "$(host "$client" url)" "${RELAY_MATRIX_LINUX_CLIENT:?set RELAY_MATRIX_LINUX_CLIENT}" "$out" "$@" \
            --host-shell "ssh -i @HOST_KEY@ -o StrictHostKeyChecking=accept-new -o BatchMode=yes $(host "$target" user)@$(host "$target" ip)" \
            --host-script "$hostdir/relay-acceptance-host.sh" --agent-rig manage --purpose "$purpose" \
            --client mcp --mcp-server @MCP_SERVER@ > "$log" 2>&1 || rc=$?
        [ -f "$out/run.log" ] && cat "$out/run.log" >> "$log"
        if [ -d "$out/$cell/$profile" ]; then
            mkdir -p "$work/evidence/$cell"
            rm -rf "$work/evidence/$cell/$profile"
            mv "$out/$cell/$profile" "$work/evidence/$cell/$profile"
        fi
        ;;
    esac
    verdicts=$(grep -oE ' AC-[0-9]+: (Passed|Failed|Blocked)' "$log" | sort -u | awk '{print $2}' | sort | uniq -c | tr -s ' \n' ' ' || true)
    echo "RESULT $cell $profile on $target rc=$rc$verdicts $(date '+%H:%M')"
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
        [ -f "$key.pub" ] && deauthorize "$dest" && echo "key removed from $instance"
    done
}

authorize_all() {
    need_plan
    for f in "$work"/hosts/*.json; do
        instance=$(basename "$f" .json)
        authorize "$(host "$instance" dest)" "$(host "$instance" hostdir)" < /dev/null && echo "key authorized on $instance"
    done
}

case "$step" in
up) up ;;
authorize) authorize_all ;;
run) run ;;
down) down ;;
all) up && run; down ;;
*) echo "usage: relay-acceptance-matrix.sh up|run|down|all <work dir>" >&2; exit 2 ;;
esac
