#!/bin/sh
# Install, update or remove this Mac's runner controller
# (docs/arch/lab-runners.md, LR-1, LR-10): the controller's bundle and the
# LaunchAgent com.wezzard.vm-service.lab that runs `lab-host run` in the
# desktop session. It runs no machine-service of its own: production, the
# Mac's installed machine-service, lends the ephemeral runner VMs, and each
# run's test environment is that run's own build, which the controller
# fetches and removes per run (DC-284). The owner runs this at the Mac, from
# a checkout of the commit to install, and loads the LaunchAgent with the
# command it prints.
#
#   rust/scripts/lab-host.sh install [--owner LOGIN] [--relay-agent-release DIR]
#                                    [--sign-developer-id[=IDENTITY]]
#   rust/scripts/lab-host.sh remove
#
# install builds this checkout's lab-host, second-instance (the launcher it
# stops a test environment with when a run's build left none), machctl (its
# discovery of the durable Macs' test environments) and the guest firewall
# installer, copies them to ~/.local/libexec/vm-service-lab/<commit>/,
# points `current` at it, and writes the LaunchAgent. Run again from a newer
# checkout, it is the update: a running lab-host sees the new `current`,
# exits when it serves no job and runs no test environment, and launchd
# starts the new one.
#
# --owner LOGIN writes the owner's GitHub login to
# ~/.config/vm-service-lab/controller.json. A durable Mac's controller
# serves only runs that login dispatched by hand, and none without it
# (LR-10); a VM host does not read it. The login is local configuration,
# never part of the repository.
# --relay-agent-release DIR puts a published relay agent release (the
# folder of its archives and SHA256SUMS) into the bundle; every test
# environment then uses it in place of its run's ad hoc pre-release, so a
# guest whose image carries a Developer ID agent is never asked to downgrade
# it (DC-232, DC-274).
# --sign-developer-id signs the bundle's lab-host with the keychain's
# Developer ID Application identity under com.wezzard.vm-service.lab, so a
# local network grant survives later updates (DC-271).
#
# remove unloads the LaunchAgent, stops every test environment the
# controller left (which releases their leases and deletes their VMs), and
# removes the bundles and the LaunchAgent. The state folder keeps the runner
# cache and the configuration its file; delete
# ~/.local/state/vm-service-lab and ~/.config/vm-service-lab by hand to drop
# them.
set -eu
cd "$(dirname "$0")/.."
label=com.wezzard.vm-service.lab
bundles="$HOME/.local/libexec/vm-service-lab"
state="$HOME/.local/state/vm-service-lab"
config="$HOME/.config/vm-service-lab/controller.json"
plist="$HOME/Library/LaunchAgents/$label.plist"
target_dir="${CARGO_TARGET_DIR:-target}"

write_owner() {
    case "$1" in
    "" | -* | *[!A-Za-z0-9-]*) echo "--owner $1 is not a GitHub login" >&2; exit 2 ;;
    esac
    mkdir -p "$(dirname "$config")"
    umask 077
    printf '{"owner": "%s"}\n' "$1" >"$config.new"
    mv "$config.new" "$config"
    echo "this Mac's controller serves the dispatched runs of the configured owner"
}

install_bundle() {
    release=""
    identity=""
    owner=""
    while [ $# -gt 0 ]; do
        case "$1" in
        --owner) owner="${2:?--owner needs a login}"; shift 2 ;;
        --relay-agent-release) release="${2:?--relay-agent-release needs a folder}"; shift 2 ;;
        --sign-developer-id) identity="Developer ID Application"; shift ;;
        --sign-developer-id=*) identity="${1#--sign-developer-id=}"; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
        esac
    done
    commit="$(git rev-parse --short=12 HEAD)"
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "the checkout has uncommitted changes; install a commit" >&2
        exit 1
    fi
    out="$target_dir/vm-service-lab/$commit"
    rm -rf "$out"
    mkdir -p "$out"
    cargo build --release -p lab-runners -p second-instance -p machctl -p guest-firewall
    for executable in lab-host second-instance machctl vm-service-guest-firewall; do
        cp "$target_dir/release/$executable" "$out/"
    done
    if [ -n "$release" ]; then
        (cd "$release" && /usr/bin/shasum -a 256 -c SHA256SUMS) >/dev/null
        mkdir -p "$out/relay-agent-release"
        cp "$release/SHA256SUMS" "$out/relay-agent-release/"
        awk '{ sub(/^\*/, "", $2); print $2 }' "$release/SHA256SUMS" | while IFS= read -r file; do
            cp "$release/$file" "$out/relay-agent-release/"
        done
    fi
    if [ -n "$identity" ]; then
        /usr/bin/codesign --force --sign "$identity" --identifier "$label" --options runtime "$out/lab-host"
    fi
    mkdir -p "$bundles"
    rm -rf "$bundles/$commit.new"
    cp -R "$out" "$bundles/$commit.new"
    rm -rf "$bundles/$commit"
    mv "$bundles/$commit.new" "$bundles/$commit"
    ln -sfn "$commit" "$bundles/current"
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    "$bundles/current/lab-host" launch-agent >"$plist.new"
    /usr/bin/plutil -lint "$plist.new" >/dev/null
    mv "$plist.new" "$plist"
    # Older bundles go, except the current one.
    for old in "$bundles"/*; do
        name="$(basename "$old")"
        [ "$name" = current ] || [ "$name" = "$commit" ] || rm -rf "$old"
    done
    [ -z "$owner" ] || write_owner "$owner"
    echo "installed the runner controller $commit"
    if /bin/launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
        echo "lab-host is loaded; it takes the new bundle when it is idle"
    else
        echo "load the LaunchAgent once (the owner approves it):"
        echo "  /bin/launchctl bootstrap gui/$(id -u) $plist"
    fi
    echo "then check this Mac with: $bundles/current/lab-host doctor"
}

remove_bundle() {
    /bin/launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    if [ -d "$state/runs" ]; then
        for run in "$state/runs"/*; do
            [ -d "$run" ] || continue
            if [ -f "$run/env/second-instance.json" ]; then
                launcher="$run/bundle/second-instance"
                [ -x "$launcher" ] || launcher="$bundles/current/second-instance"
                "$launcher" stop --root "$run/env"
            fi
            rm -rf "$run"
        done
    fi
    rm -f "$plist"
    rm -rf "$bundles"
    echo "removed the runner controller; $state keeps the runner cache"
}

case "${1:-}" in
install) shift; install_bundle "$@" ;;
remove) remove_bundle ;;
*) echo "usage: $0 install [--owner LOGIN] [--relay-agent-release DIR] [--sign-developer-id[=IDENTITY]] | remove" >&2; exit 2 ;;
esac
