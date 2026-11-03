#!/bin/sh
# Build the relay agent's release archives: one per guest platform, each a
# gzipped tar holding the one executable `relay-agent`, named
# relay-agent-<version>-<platform>.tar.gz, and SHA256SUMS over them, so an
# image pins a release by version and SHA-256 (docs/arch/relay-agent.md RA-6).
#
# Platforms: darwin-arm64, linux-arm64, linux-x86_64. The Linux builds are
# static musl executables linked by rust-lld. A release builds each platform
# natively on its own runner (darwin-arm64 on macOS, each Linux platform on a
# Linux runner of that architecture with musl-gcc from musl-tools), so ring's
# C code compiles against real musl headers (DC-178). On a Mac, linux-arm64
# can also be cross-built for development through the clang fallback below;
# linux-x86_64 cannot, because clang has no C library headers for it there.
#
# The macOS executable is signed with the identifier com.wezzard.relay-agent,
# the LaunchAgent's label, instead of the linker's per-build identifier, and
# with the hardened runtime (DC-135, DC-224, docs/signing.md):
#
# - --sign-developer-id signs with a Developer ID Application identity of
#   the keychain, "Developer ID Application" or the one named after `=`: the
#   signature carries a secure timestamp, and its designated requirement
#   names the identifier and the team, so a privacy grant holds for every
#   later build the team signs. The script checks that requirement against
#   the team the signature carries, or CODESIGN_TEAM when set. Only the flag
#   signs with Developer ID: a CODESIGN_IDENTITY in the environment, which a
#   shell profile or a runner may set, is ignored, so no build signs with the
#   login keychain's key unless its command line asks (DC-250).
# - Without it the signature is ad hoc, and its designated requirement is the
#   build's code hash. Only a pre-release may be ad hoc, so an ad hoc build of
#   a version without a pre-release part names itself <version>-adhoc, unless
#   RELAY_AGENT_AWAITS_SIGNING=1: the release workflow's build of a release,
#   which the owner signs with rust/scripts/sign-release.sh before it is
#   published.
#
# Either way the script prints the designated requirement and writes
# SIGNING.json beside the archives, the release's record of the signature;
# every consumer checks the executable itself.
#
# Usage: scripts/build-relay-agent.sh [--sign-developer-id[=IDENTITY]] [output-dir [platform ...]]
#        (default target/relay-agent, every platform this host can build)
#        CODESIGN_TEAM, RELAY_AGENT_AWAITS_SIGNING: above
set -eu

cd "$(dirname "$0")/.."
. scripts/relay-agent-signing.sh
identity=-
case "${1:-}" in
    --sign-developer-id) identity="Developer ID Application"; shift ;;
    --sign-developer-id=?*) identity="${1#--sign-developer-id=}"; shift ;;
    -*) echo "error: unknown option $1" >&2; exit 2 ;;
esac
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    echo "warning: CODESIGN_IDENTITY is ignored; only --sign-developer-id signs with Developer ID" >&2
fi
out="${1:-target/relay-agent}"
[ $# -gt 0 ] && shift
target_dir="${CARGO_TARGET_DIR:-target}"
version=$(cargo pkgid -p relay-agent | sed 's/.*[#@]//')
case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "error: cannot read the relay-agent version (got '$version')" >&2; exit 1 ;;
esac

if [ $# -eq 0 ]; then
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) set -- darwin-arm64 linux-arm64 ;;
        Linux-aarch64) set -- linux-arm64 ;;
        Linux-x86_64) set -- linux-x86_64 ;;
        *) echo "error: no default platform for $(uname -s)-$(uname -m); name one" >&2; exit 1 ;;
    esac
fi

wanted_team="${CODESIGN_TEAM:-}"
awaits=false
[ "${RELAY_AGENT_AWAITS_SIGNING:-}" != 1 ] || awaits=true
[ -z "$wanted_team" ] || [ "$identity" != - ] || {
    echo "error: CODESIGN_TEAM needs --sign-developer-id" >&2; exit 1; }
[ "$awaits" = false ] || [ "$identity" = - ] || {
    echo "error: RELAY_AGENT_AWAITS_SIGNING is for an ad hoc build; --sign-developer-id signs it now" >&2; exit 1; }
for platform in "$@"; do
    if [ "$platform" = darwin-arm64 ] && [ "$identity" = - ] && [ "$awaits" = false ]; then
        case "$version" in
            *-*) ;;
            *) version="$version-adhoc"
               echo "warning: no --sign-developer-id: the macOS build is ad hoc, so this is the pre-release $version (DC-224)" >&2 ;;
        esac
    fi
done

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
        *-linux-musl)
            # ring compiles C. Without a musl cross compiler, the host's clang
            # compiles it for the gnu triple of the same ABI and the
            # toolchain's llvm-ar archives the ELF objects (DC-154).
            arch_prefix=${target%%-*}
            target_variable=$(echo "$target" | tr 'a-z-' 'a-z_')
            if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = "$arch_prefix" ] && command -v musl-gcc >/dev/null 2>&1; then
                # A native Linux build: musl-gcc compiles ring against musl.
                export "CC_$target_variable=musl-gcc"
            elif ! command -v "$arch_prefix-linux-musl-gcc" >/dev/null 2>&1; then
                # No musl cross compiler: the clang fallback is the only way,
                # so its tools must be present, or the build stops here with
                # the fix instead of failing later inside ring's build.
                command -v clang >/dev/null 2>&1 || {
                    echo "error: building $target needs $arch_prefix-linux-musl-gcc or clang" >&2; exit 1; }
                llvm_bin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
                [ -x "$llvm_bin/llvm-ar" ] || {
                    echo "error: building $target needs llvm-ar; run: rustup component add llvm-tools" >&2; exit 1; }
                export "CC_$target_variable=clang"
                export "CFLAGS_$target_variable=--target=$arch_prefix-unknown-linux-gnu"
                export "AR_$target_variable=$llvm_bin/llvm-ar"
            fi
            env "$linker_variable=rust-lld" cargo build --release -p relay-agent --target "$target" ;;
        *) cargo build --release -p relay-agent --target "$target" ;;
    esac
    mkdir -p "$stage/$platform"
    cp "$target_dir/$target/release/relay-agent" "$stage/$platform/relay-agent"
    chmod 755 "$stage/$platform/relay-agent"
    if [ "$platform" = darwin-arm64 ]; then
        executable="$stage/$platform/relay-agent"
        if [ "$identity" = - ]; then
            sign_ad_hoc "$executable"
            signature=adhoc
        else
            sign_developer_id "$identity" "$executable"
            verify_developer_id "$executable" "$wanted_team"
            signature=developer-id
        fi
        codesign --verify --strict -R="identifier \"$identifier\"" "$executable"
        echo "designated requirement: $(designated_requirement "$executable")"
        [ "$signature" = adhoc ] || echo "verified: Developer ID, identifier $identifier, the signature's team, hardened runtime, secure timestamp"
        write_signing_record "$out/SIGNING.json" "$executable" "$version" "$signature" "$awaits"
    fi
    # The archive names no builder: owner root, as an installer expects.
    archive_executable "$stage/$platform/relay-agent" "$out/relay-agent-$version-$platform.tar.gz"
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
