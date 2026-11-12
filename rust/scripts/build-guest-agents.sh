#!/bin/sh
# Build the guest console agent and the guest session agent for every guest
# OS this host can serve, and install each build under the name the daemon
# looks for. The relay agent is not built here: an installation holds its
# release archives, which scripts/build-relay-agent.sh makes (DC-140).
#
# The Python original shipped `guest-console-agent.py`: one artifact that runs
# on any guest with an interpreter. The native port streams machine code, so a
# single artifact is no longer correct — the bytes must match the *guest's*
# operating system, not the host's. A macOS host leasing a Linux guest would
# otherwise upload a Mach-O image that the guest cannot execute, which shows up
# as a probe that never answers.
#
# `console::guest::agent_path_for` resolves, in order:
#   1. $VM_GUEST_CONSOLE_AGENT                      (explicit override)
#   2. guest-console-agent-<kind> beside the daemon (this script produces these)
#   3. guest-console-agent beside the daemon        (host-native fallback)
#
# `guest_link::agent_path_for` resolves `guest-session-agent` the same way,
# with $VM_GUEST_SESSION_AGENT as the override (docs/arch/guest-session.md).
#
# Agents are release builds: each is uploaded to a guest, so its size is
# transfer time (docs/arch/guest-session.md, GS-3).
#
# Usage: scripts/build-guest-agents.sh [output-dir]
# The output dir must not be cargo's own target dir, or the copies are no-ops.
set -eu

cd "$(dirname "$0")/.."
out="${1:-target/guest-agents}"
target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$out"

agents="guest-console-agent guest-session-agent"

echo "building host-native agents -> $out"
cargo build --release -p guest-console-agent
cargo build --release -p guest-session --bin guest-session-agent
for agent in $agents; do
    cp "$target_dir/release/$agent" "$out/$agent"
done

# Linux guests cannot execute the host build, so cross-compile the same crate.
target=aarch64-unknown-linux-musl
if rustup target list --installed | grep -qx "$target"; then
    echo "building linux agents ($target) -> $out"
    if command -v aarch64-linux-musl-gcc >/dev/null 2>&1; then
        export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
    else
        # Without a musl cross toolchain: the host's clang compiles ring's C
        # for the gnu triple (the same aarch64 ABI; clang's musl triple wants
        # a musl sysroot), the toolchain's llvm-ar archives the ELF objects
        # (Apple's ar drops them), and rust-lld links against Rust's own musl
        # (`rustup component add llvm-tools` provides llvm-ar).
        llvm_bin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
        if [ ! -x "$llvm_bin/llvm-ar" ]; then
            echo "error: no aarch64-linux-musl-gcc and no llvm-ar;" >&2
            echo "       run: rustup component add llvm-tools" >&2
            exit 1
        fi
        export CC_aarch64_unknown_linux_musl=clang
        export CFLAGS_aarch64_unknown_linux_musl="--target=aarch64-unknown-linux-gnu"
        export AR_aarch64_unknown_linux_musl="$llvm_bin/llvm-ar"
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
    fi
    cargo build --release -p guest-console-agent --target "$target"
    cargo build --release -p guest-session --bin guest-session-agent --target "$target"
    for agent in $agents; do
        cp "$target_dir/$target/release/$agent" "$out/$agent-linux"
    done
else
    echo "error: rust target $target is not installed;" >&2
    echo "       run: rustup target add $target" >&2
    echo "       Linux guests cannot be served without a Linux agent binary." >&2
    exit 1
fi

echo "done:"
for agent in $agents; do
    ls -l "$out/$agent" "$out/$agent-linux"
done
