#!/bin/sh
# Sign a drafted relay agent release with the owner's Developer ID and publish
# it (DC-224, docs/signing.md). Run on the owner's Mac, whose keychain holds
# the Developer ID Application identity; the key never leaves it.
#
# The release workflow drafts every release with an ad hoc macOS executable
# and a SIGNING.json that says it awaits signing. This script:
#
#  1. downloads the draft's assets, and refuses unless every archive matches
#     SHA256SUMS, and SIGNING.json names this version, an ad hoc executable
#     awaiting its signature, and the darwin-arm64 executable's SHA-256;
#  2. signs that executable with the identifier com.wezzard.relay-agent, the
#     hardened runtime and a secure timestamp, and refuses unless its
#     designated requirement is the Developer ID requirement of the identifier
#     and the team the signature itself carries (or CODESIGN_TEAM when set);
#  3. archives it again as the build does (one member, mode 755, owner root),
#     writes SHA256SUMS over all three archives and a new SIGNING.json;
#  4. uploads the replaced files to the draft, checks the uploaded assets
#     against the new sums, removes the awaiting-signature note and publishes.
#
# The team is read from the signature and written only where the signature
# already carries it: the executable and SIGNING.json, never the notes.
#
# Usage: scripts/sign-release.sh [--dry-run] [--dir DIR] <tag>
#   --dry-run  stop before anything is uploaded or published
#   --dir DIR  sign the assets already in DIR instead of downloading the
#              draft (implies --dry-run); the signed files are left there
#   CODESIGN_IDENTITY  the identity (default "Developer ID Application")
#   CODESIGN_TEAM      refuse a signature by any other team
#   GH_REPO            the repository, when not the checkout's
set -eu

cd "$(dirname "$0")/.."
. scripts/relay-agent-signing.sh

dry_run=false
dir=
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=true; shift ;;
        --dir) dir=$2; dry_run=true; shift 2 ;;
        -*) echo "error: unknown option $1" >&2; exit 2 ;;
        *) break ;;
    esac
done
[ $# -eq 1 ] || { echo "usage: scripts/sign-release.sh [--dry-run] [--dir DIR] <tag>" >&2; exit 2; }
tag=$1
version=${tag#v}
[ "$tag" = "v$version" ] || { echo "error: a release tag is v<version>, not $tag" >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "error: signing needs macOS and its keychain" >&2; exit 1; }
identity="${CODESIGN_IDENTITY:-Developer ID Application}"
marker='<!-- relay-agent: awaits Developer ID signing -->'
darwin="relay-agent-$version-darwin-arm64.tar.gz"
platforms="darwin-arm64 linux-arm64 linux-x86_64"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
if [ -n "$dir" ]; then
    assets=$dir
else
    command -v gh >/dev/null 2>&1 || { echo "error: needs the GitHub CLI, gh" >&2; exit 1; }
    draft=$(gh release view "$tag" --json isDraft --jq .isDraft)
    [ "$draft" = true ] || { echo "error: the release $tag is not a draft; a published release is never re-signed" >&2; exit 1; }
    assets="$work/assets"
    mkdir -p "$assets"
    gh release download "$tag" --dir "$assets" --pattern 'relay-agent-*.tar.gz' --pattern SHA256SUMS --pattern SIGNING.json
fi

# 1. The draft is what the workflow built, and awaits signing.
(
    cd "$assets"
    for platform in $platforms; do
        [ -f "relay-agent-$version-$platform.tar.gz" ] || { echo "error: the draft has no relay-agent-$version-$platform.tar.gz" >&2; exit 1; }
    done
    [ "$(grep -c . SHA256SUMS)" -eq 3 ] || { echo "error: SHA256SUMS must name exactly the three archives" >&2; exit 1; }
    shasum -a 256 -c --strict SHA256SUMS >/dev/null || {
        echo "error: an archive of the draft no longer matches SHA256SUMS; refusing to sign" >&2; exit 1; }
)
record="$assets/SIGNING.json"
field() { plutil -extract "$1" raw -o - "$record" 2>/dev/null || true; }
[ "$(field version)" = "$version" ] || { echo "error: SIGNING.json is not for $version" >&2; exit 1; }
[ "$(field signature)" = adhoc ] && [ "$(field awaitsSigning)" = true ] || {
    echo "error: SIGNING.json says the release does not await signing" >&2; exit 1; }
mkdir -p "$work/member"
tar -xzf "$assets/$darwin" -C "$work/member"
[ "$(ls -A "$work/member")" = relay-agent ] || { echo "error: $darwin must hold the one file relay-agent" >&2; exit 1; }
executable="$work/member/relay-agent"
[ "$(sha256_of "$executable")" = "$(field executableSha256)" ] || {
    echo "error: the darwin-arm64 executable is not the one SIGNING.json names" >&2; exit 1; }

# 2. Sign, and check the signature against the team it carries itself.
sign_developer_id "$identity" "$executable"
verify_developer_id "$executable" "${CODESIGN_TEAM:-}"
echo "signed: Developer ID, identifier $identifier, the signature's own team, hardened runtime, secure timestamp"

# 3. The new archive, sums and record.
out="$work/out"
mkdir -p "$out"
archive_executable "$executable" "$out/$darwin"
for platform in $platforms; do
    [ "$platform" = darwin-arm64 ] || cp "$assets/relay-agent-$version-$platform.tar.gz" "$out/"
done
( cd "$out" && for file in relay-agent-*.tar.gz; do printf '%s  %s\n' "$(sha256_of "$file")" "$file"; done | sort -k2 > SHA256SUMS )
write_signing_record "$out/SIGNING.json" "$executable" "$version" developer-id false
rm -f "$out"/relay-agent-*-linux-*.tar.gz

if [ "$dry_run" = true ]; then
    if [ -n "$dir" ]; then
        cp "$out/$darwin" "$out/SHA256SUMS" "$out/SIGNING.json" "$dir/"
        echo "signed in $dir (nothing uploaded)"
    else
        echo "dry run: signed $darwin; nothing uploaded or published"
    fi
    exit 0
fi

# 4. Replace, check what the release now serves, publish.
gh release upload "$tag" --clobber "$out/$darwin" "$out/SHA256SUMS" "$out/SIGNING.json"
check="$work/check"
mkdir -p "$check"
gh release download "$tag" --dir "$check" --pattern 'relay-agent-*.tar.gz' --pattern SHA256SUMS
cmp -s "$check/SHA256SUMS" "$out/SHA256SUMS" || { echo "error: the uploaded SHA256SUMS is not the one written; the draft is left unpublished" >&2; exit 1; }
( cd "$check" && shasum -a 256 -c --strict SHA256SUMS >/dev/null ) || {
    echo "error: the draft's archives do not match the new SHA256SUMS; the draft is left unpublished" >&2; exit 1; }
gh release view "$tag" --json body --jq .body | awk -v marker="$marker" '
    $0 == marker { skip = 1; next }
    skip && /^<!-- end -->$/ { skip = 0; next }
    !skip' > "$work/notes.md"
gh release edit "$tag" --notes-file "$work/notes.md" --draft=false
echo "published $tag with a Developer ID signed macOS relay agent"
