#!/bin/sh
# Download the published relay agent release of a tag into a folder, for a
# deploy's build (build-release.sh --relay-agent-release, DC-356), and
# refuse anything that is not the signed release:
#
# - the release is published, not a draft that awaits its signature;
# - every archive matches SHA256SUMS and is of the tag's version;
# - SIGNING.json records a Developer ID signature of that version that
#   awaits nothing, and the SHA-256 of the macOS executable;
# - the macOS executable is that one, and is signed with Developer ID, with
#   the hardened runtime and a secure timestamp (relay-agent-signing.sh).
#
# The installer and the daemon then check the signature's team against the
# instance's trust (DC-231, DC-236). The folder holds the archives,
# SHA256SUMS and SIGNING.json, as the release workflow made them.
#
# Usage: scripts/fetch-relay-agent-release.sh <tag> <folder>
#   GH_REPO   the repository, when not the checkout's
set -eu

[ $# -eq 2 ] || { echo "usage: $0 <tag> <folder>" >&2; exit 2; }
tag=$1
dest=$2
version=${tag#v}
cd "$(dirname "$0")/.."
. scripts/relay-agent-signing.sh

fail() { echo "error: $*" >&2; exit 1; }

draft=$(gh release view "$tag" --json isDraft --jq .isDraft) ||
    fail "no release $tag"
[ "$draft" = false ] || fail "$tag is a draft that awaits its signature (sign-release.sh)"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/release" "$work/x"
gh release download "$tag" --dir "$work/release" \
    --pattern 'relay-agent-*.tar.gz' --pattern SHA256SUMS --pattern SIGNING.json
(cd "$work/release" && shasum -a 256 -c SHA256SUMS >/dev/null) ||
    fail "the archives of $tag do not match its SHA256SUMS"
for file in $(awk '{ sub(/^\*/, "", $2); print $2 }' "$work/release/SHA256SUMS"); do
    case "$file" in
    "relay-agent-$version-"*.tar.gz) ;;
    *) fail "$file in $tag's SHA256SUMS is not an archive of version $version" ;;
    esac
done

# SIGNING.json: one line per field, as write_signing_record writes it.
field() { sed -n "s/^ *\"$1\": *\"*\([^\",]*\)\"*,*\$/\1/p" "$work/release/SIGNING.json" | head -1; }
[ "$(field version)" = "$version" ] || fail "SIGNING.json of $tag names version $(field version), not $version"
[ "$(field signature)" = developer-id ] || fail "SIGNING.json of $tag records the signature $(field signature), not developer-id"
[ "$(field awaitsSigning)" = false ] || fail "SIGNING.json of $tag awaits its signature"

darwin=$(ls "$work/release"/relay-agent-"$version"-darwin-*.tar.gz 2>/dev/null | head -1)
[ -n "$darwin" ] || fail "$tag has no macOS archive"
tar -xzf "$darwin" -C "$work/x" relay-agent
[ "$(sha256_of "$work/x/relay-agent")" = "$(field executableSha256)" ] ||
    fail "the macOS executable of $tag is not the one SIGNING.json records"
verify_developer_id "$work/x/relay-agent" || fail "the macOS executable of $tag is not signed with Developer ID"

mkdir -p "$dest"
cp "$work/release"/* "$dest/"
echo "relay agent release $version: SHA256SUMS, SIGNING.json and its Developer ID signature checked -> $dest"
