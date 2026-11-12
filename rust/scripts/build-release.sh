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
# folder holds its release, relay-agent-release/, made by the same
# scripts/build-relay-agent.sh as a published release, with an archive for
# each platform the installation serves (DC-140). The installer copies that
# folder verbatim, and the daemon sends a guest the executable that the
# archive holds, never a build of its own.
#
# Usage: scripts/build-release.sh [output-dir]   (default target/machine-service)
set -eu

cd "$(dirname "$0")/.."
out="${1:-target/machine-service}"
target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$out"

# The guest agents: the host-native build and the Linux build of each.
scripts/build-guest-agents.sh "$out"

# The relay agent's release, for macOS (guests and this Mac) and Linux
# guests on this host's architecture.
case "$(uname -m)" in
    arm64 | aarch64) arch=arm64 ;;
    *) arch=$(uname -m) ;;
esac
scripts/build-relay-agent.sh "$out/relay-agent-release" "darwin-$arch" "linux-$arch"
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
