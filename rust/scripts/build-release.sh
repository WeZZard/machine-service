#!/bin/sh
# Build every executable of a machine-service installation in the release
# profile and put each under its installed name in one folder
# (docs/arch/installation.md IN-2).
#
# The folder is what `machine-service-install install --prefix DIR` installs
# from: the installer copies the executables beside itself that the daemon
# resolves at run time (`service_layout::required`), and refuses, before it
# changes anything, when one is missing. A name added to the layout but not
# built here therefore fails the installation loudly, never silently.
#
# The relay agent is the one exception to building from source here: the
# folder holds its release, relay-agent-release/, with an archive for each
# platform the installation serves (DC-140). The installer copies that
# folder verbatim, and the daemon sends a guest the executable that the
# archive holds, never a build of its own. Where it comes from is named on
# the command line, and nothing is built without it (DC-356):
#
# --relay-agent-release <tag>
#       A deploy: the published release of <tag>, downloaded with `gh`. It
#       must match its SHA256SUMS, its SIGNING.json must record a Developer
#       ID signature that awaits nothing and the tag's version, and its
#       macOS executable must pass `codesign --verify --strict`. The
#       installer and the daemon check its team (DC-231, DC-236).
# --relay-agent-from-source
#       Development: build the relay agent here with build-relay-agent.sh,
#       an ad hoc `<version>-adhoc` pre-release, which an installer takes
#       only with --allow-prerelease-relay-agent.
#
# Usage: scripts/build-release.sh (--relay-agent-release <tag> | --relay-agent-from-source) [output-dir]
#        (output-dir defaults to target/machine-service)
set -eu

usage() {
    echo "usage: $0 (--relay-agent-release <tag> | --relay-agent-from-source) [output-dir]" >&2
    exit 2
}
tag=""
from_source=""
while [ $# -gt 0 ]; do
    case "$1" in
    --relay-agent-release) tag="${2:-}"; [ -n "$tag" ] || usage; shift 2 ;;
    --relay-agent-from-source) from_source=1; shift ;;
    -*) usage ;;
    *) break ;;
    esac
done
if [ -n "$tag" ] && [ -n "$from_source" ]; then usage; fi
if [ -z "$tag" ] && [ -z "$from_source" ]; then
    echo "name where the relay agent comes from: --relay-agent-release <tag> for a deploy, or --relay-agent-from-source for an ad hoc pre-release (DC-356)" >&2
    usage
fi
[ $# -le 1 ] || usage

cd "$(dirname "$0")/.."
out="${1:-target/machine-service}"
target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$out"

# The guest agents: the host-native build and the Linux build of each.
scripts/build-guest-agents.sh "$out"

# The relay agent's release, for macOS (guests and this Mac) and Linux
# guests on this host's architecture.
release="$out/relay-agent-release"
rm -rf "$release"
if [ -n "$tag" ]; then
    scripts/fetch-relay-agent-release.sh "$tag" "$release"
else
    case "$(uname -m)" in
        arm64 | aarch64) arch=arm64 ;;
        *) arch=$(uname -m) ;;
    esac
    scripts/build-relay-agent.sh "$release" "darwin-$arch" "linux-$arch"
fi
# Builds of the relay agent from an earlier layout are not part of this one.
rm -f "$out/relay-agent" "$out/relay-agent-linux"

echo "building host executables -> $out"
cargo build --release \
    -p machine-service -p machctl -p machine-service-install \
    -p console-worker -p guest-dot-forwarder -p guest-firewall
for executable in machine-service machctl machine-service-install \
    console-worker vm-service-dot-forwarder vm-service-guest-firewall; do
    cp "$target_dir/release/$executable" "$out/$executable"
done

echo "done: $out"
ls -l "$out"
