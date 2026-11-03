#!/bin/sh
# Install, update or remove this Mac's lab host (docs/arch/lab-runners.md,
# LR-6): the lab bundle, which lab-host runs its lab instance from, and the
# LaunchAgent com.wezzard.vm-service.lab that runs `lab-host run` in the
# desktop session. The owner runs it at the Mac, from a checkout of the
# commit to install, and loads the LaunchAgent with the command it prints.
#
#   rust/scripts/lab-host.sh install [--relay-agent-release DIR] [--sign-developer-id[=IDENTITY]]
#   rust/scripts/lab-host.sh remove
#
# install builds this checkout (scripts/build-release.sh, second-instance
# and lab-host), copies the bundle to
# ~/.local/libexec/vm-service-lab/<commit>/, points `current` at it, and
# writes the LaunchAgent. Run again from a newer checkout, it is the update:
# a running lab-host sees the new `current`, exits when it serves no job,
# and launchd starts the new one, which restarts the lab instance when idle.
#
# --relay-agent-release DIR puts a published relay agent release (the
# folder of its archives and SHA256SUMS) into the bundle instead of the
# ad hoc pre-release this checkout builds, so a guest whose image carries a
# Developer ID agent is never asked to downgrade it (DC-232).
# --sign-developer-id signs the bundle's lab-host with the keychain's
# Developer ID Application identity under com.wezzard.vm-service.lab, so a
# local network grant survives later updates (DC-271).
#
# remove unloads the LaunchAgent, stops the lab instance (which releases its
# leases and deletes its VMs), and removes the bundles and the LaunchAgent.
# The lab's state folder keeps its runner cache; delete
# ~/.local/state/vm-service-lab by hand to drop it.
set -eu
cd "$(dirname "$0")/.."
label=com.wezzard.vm-service.lab
bundles="$HOME/.local/libexec/vm-service-lab"
state="$HOME/.local/state/vm-service-lab"
plist="$HOME/Library/LaunchAgents/$label.plist"
target_dir="${CARGO_TARGET_DIR:-target}"

install_bundle() {
    release=""
    identity=""
    while [ $# -gt 0 ]; do
        case "$1" in
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
    scripts/build-release.sh "$out"
    cargo build --release -p second-instance -p lab-runners
    cp "$target_dir/release/second-instance" "$target_dir/release/lab-host" "$out/"
    if [ -n "$release" ]; then
        (cd "$release" && /usr/bin/shasum -a 256 -c SHA256SUMS) >/dev/null
        rm -rf "$out/relay-agent-release"
        cp -R "$release" "$out/relay-agent-release"
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
    echo "installed the lab bundle $commit"
    if /bin/launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
        echo "lab-host is loaded; it takes the new bundle when it serves no job"
    else
        echo "load the LaunchAgent once (the owner approves it):"
        echo "  /bin/launchctl bootstrap gui/$(id -u) $plist"
    fi
    echo "then check this Mac with: $bundles/current/lab-host doctor"
}

remove_bundle() {
    /bin/launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    if [ -x "$bundles/current/second-instance" ] && [ -d "$state/instance" ]; then
        "$bundles/current/second-instance" stop --root "$state/instance"
    fi
    rm -f "$plist"
    rm -rf "$bundles"
    echo "removed the lab host; $state keeps the runner cache"
}

case "${1:-}" in
install) shift; install_bundle "$@" ;;
remove) remove_bundle ;;
*) echo "usage: $0 install [--relay-agent-release DIR] [--sign-developer-id[=IDENTITY]] | remove" >&2; exit 2 ;;
esac
