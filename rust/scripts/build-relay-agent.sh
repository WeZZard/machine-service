#!/bin/sh
# Build the relay agent's release archives: one per guest platform, each a
# gzipped tar holding the one executable `relay-agent`, named
# relay-agent-<version>-<platform>.tar.gz, and SHA256SUMS over them, so an
# image pins a release by version and SHA-256 (docs/arch/relay-agent.md RA-6).
#
# Platforms: darwin-arm64, linux-arm64, linux-x86_64. The Linux builds are
# static musl executables linked by rust-lld, which rustup ships, so any host
# with the musl targets installed builds them without a C cross toolchain.
# darwin-arm64 builds only on a macOS arm64 host.
#
# The macOS executable is signed with the identifier com.wezzard.relay-agent,
# the LaunchAgent's label, instead of the linker's per-build identifier. A
# guest grants privacy permissions to the code requirement
# `identifier "com.wezzard.relay-agent"`, which every build signed here
# satisfies, so a new build keeps the grants (decision DC-1xx-REL-1). The
# signature is ad hoc unless CODESIGN_IDENTITY names a signing identity.
#
# Usage: scripts/build-relay-agent.sh [output-dir [platform ...]]
#        (default target/relay-agent, every platform this host can build)
set -eu

cd "$(dirname "$0")/.."
out="${1:-target/relay-agent}"
[ $# -gt 0 ] && shift
target_dir="${CARGO_TARGET_DIR:-target}"
identifier=com.wezzard.relay-agent

version=$(cargo pkgid -p relay-agent | sed 's/.*[#@]//')
case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "error: cannot read the relay-agent version (got '$version')" >&2; exit 1 ;;
esac

if [ $# -eq 0 ]; then
    if [ "$(uname -s)-$(uname -m)" = Darwin-arm64 ]; then
        set -- darwin-arm64 linux-arm64 linux-x86_64
    else
        set -- linux-arm64 linux-x86_64
    fi
fi

rm -rf "$out"
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

for platform in "$@"; do
    case "$platform" in
        darwin-arm64) target=aarch64-apple-darwin ;;
        linux-arm64) target=aarch64-unknown-linux-musl ;;
        linux-x86_64) target=x86_64-unknown-linux-musl ;;
        *) echo "error: unknown platform $platform" >&2; exit 1 ;;
    esac
    if ! rustup target list --installed | grep -qx "$target"; then
        echo "error: rust target $target is not installed; run: rustup target add $target" >&2
        exit 1
    fi
    echo "building relay-agent $version for $platform ($target)"
    linker_variable="CARGO_TARGET_$(echo "$target" | tr 'a-z-' 'A-Z_')_LINKER"
    case "$target" in
        *-linux-musl) env "$linker_variable=rust-lld" cargo build --release -p relay-agent --target "$target" ;;
        *) cargo build --release -p relay-agent --target "$target" ;;
    esac
    mkdir -p "$stage/$platform"
    cp "$target_dir/$target/release/relay-agent" "$stage/$platform/relay-agent"
    chmod 755 "$stage/$platform/relay-agent"
    if [ "$platform" = darwin-arm64 ]; then
        codesign --force --sign "${CODESIGN_IDENTITY:--}" --identifier "$identifier" "$stage/$platform/relay-agent"
        codesign --verify --strict -R="identifier \"$identifier\"" "$stage/$platform/relay-agent"
    fi
    # The archive names no builder: owner root, as an installer expects.
    if tar --version 2>/dev/null | grep -q GNU; then
        tar --owner=0 --group=0 --numeric-owner -C "$stage/$platform" -czf "$out/relay-agent-$version-$platform.tar.gz" relay-agent
    else
        tar --uid 0 --gid 0 --uname root --gname root -C "$stage/$platform" -czf "$out/relay-agent-$version-$platform.tar.gz" relay-agent
    fi
done

(
    cd "$out"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum relay-agent-*.tar.gz > SHA256SUMS
    else
        shasum -a 256 relay-agent-*.tar.gz > SHA256SUMS
    fi
)
echo "done: $out"
cat "$out/SHA256SUMS"
