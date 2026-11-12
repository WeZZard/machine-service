# Shared by build-relay-agent.sh and sign-release.sh, which source it: how the
# macOS relay agent is signed, checked, archived and recorded (DC-224,
# docs/signing.md). POSIX sh; every name it sets but `identifier` and `signed_team` starts
# with _rs_, so it never changes a caller's variable.

identifier=com.wezzard.relay-agent

# The designated requirement codesign makes for a Developer ID Application
# certificate of the team $1 (service_layout::signing).
developer_id_requirement() {
    printf 'identifier "%s" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = "%s"' "$identifier" "$1"
}

# The designated requirement of the executable $1, as codesign prints it.
designated_requirement() {
    codesign -d -r- "$1" 2>/dev/null | sed -n 's/^#* *designated => //p'
}

# The team of the executable $1's signature, read from the signature itself.
signature_team() {
    codesign -d --verbose=2 "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p' | grep -v '^not set$' || true
}

# Sign $2 with the Developer ID identity $1, with the identifier, the hardened
# runtime and a secure timestamp.
sign_developer_id() {
    codesign --force --sign "$1" --identifier "$identifier" --options runtime --timestamp "$2"
}

# Sign $1 ad hoc with the identifier and the hardened runtime.
sign_ad_hoc() {
    codesign --force --sign - --identifier "$identifier" --options runtime "$1"
}

# Refuse, naming why, unless $1 is signed with Developer ID: its designated
# requirement must be the Developer ID requirement of the identifier and of
# the team its own signature carries, or of $2 when given, and the signature
# must carry the hardened runtime and a secure timestamp. Sets the
# signature's team in `signed_team`. Never prints the team.
verify_developer_id() {
    signed_team=$(signature_team "$1")
    case "$signed_team" in
        [A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9]) ;;
        *) echo "error: $1 carries no team: it is not signed with Developer ID" >&2; return 1 ;;
    esac
    if [ -n "${2:-}" ] && [ "$2" != "$signed_team" ]; then
        echo "error: $1 is signed by another team than the one named" >&2; return 1
    fi
    _rs_want=$(developer_id_requirement "$signed_team")
    _rs_have=$(designated_requirement "$1")
    # codesign prints a team that begins with a letter bare.
    _rs_bare=$(printf '%s' "$_rs_want" | sed "s/\"$signed_team\"\$/$signed_team/")
    if [ "$_rs_have" != "$_rs_want" ] && [ "$_rs_have" != "$_rs_bare" ]; then
        echo "error: the designated requirement of $1 is not the Developer ID requirement of $identifier and its team" >&2; return 1
    fi
    codesign --verify --strict -R="$_rs_want" "$1" || {
        echo "error: $1 does not satisfy the Developer ID requirement" >&2; return 1; }
    _rs_details=$(codesign -d --verbose=2 "$1" 2>&1)
    case "$_rs_details" in *"flags=0x"*"runtime"*) ;; *) echo "error: $1 has no hardened runtime" >&2; return 1 ;; esac
    case "$_rs_details" in *"Timestamp="*) ;; *) echo "error: $1 has no secure timestamp" >&2; return 1 ;; esac
}

# The SHA-256 of $1.
sha256_of() {
    { sha256sum "$1" 2>/dev/null || shasum -a 256 "$1"; } | cut -d' ' -f1
}

# Archive the executable $1 as $2: a gzipped tar of the one member
# relay-agent, mode 755, owner root, a fixed modification time and no gzip
# name or time, so the same executable makes the same archive.
archive_executable() {
    _rs_dir=$(dirname "$1")
    [ "$(basename "$1")" = relay-agent ] || { echo "error: the member must be named relay-agent" >&2; return 1; }
    chmod 755 "$1"
    touch -t 200001010000.00 "$1"
    if tar --version 2>/dev/null | grep -q GNU; then
        tar --owner=0 --group=0 --numeric-owner -C "$_rs_dir" -cf - relay-agent | gzip -n -9 > "$2"
    else
        tar --uid 0 --gid 0 --uname root --gname root -C "$_rs_dir" -cf - relay-agent | gzip -n -9 > "$2"
    fi
}

# Write the release's signature record $1 for the macOS executable $2 of
# version $3. $4 is developer-id or adhoc; $5 is true for an ad hoc build that
# awaits the owner's Developer ID signature (sign-release.sh).
write_signing_record() {
    _rs_record=$1 _rs_executable=$2 _rs_record_version=$3 _rs_signature=$4 _rs_awaits=${5:-false}
    _rs_designated=$(designated_requirement "$_rs_executable")
    _rs_escaped=$(printf '%s' "$_rs_designated" | sed 's/\\/\\\\/g; s/"/\\"/g')
    _rs_timestamped=false
    case "$(codesign -d --verbose=2 "$_rs_executable" 2>&1)" in *"Timestamp="*) _rs_timestamped=true ;; esac
    if [ "$_rs_signature" = developer-id ]; then
        _rs_team_json="\"$(signature_team "$_rs_executable")\""
        _rs_warning=null
    else
        _rs_team_json=null
        if [ "$_rs_awaits" = true ]; then
            _rs_warning='"This release awaits the Developer ID signature of its macOS relay agent (rust/scripts/sign-release.sh); until then it is a draft."'
        else
            _rs_warning='"The macOS relay agent of this pre-release is signed ad hoc, not with Developer ID: a privacy grant given to it ends with the next build (DC-224)."'
        fi
    fi
    cat > "$_rs_record" <<JSON
{
 "schemaVersion": 1,
 "version": "$_rs_record_version",
 "platform": "darwin-arm64",
 "identifier": "$identifier",
 "signature": "$_rs_signature",
 "awaitsSigning": $_rs_awaits,
 "team": $_rs_team_json,
 "designatedRequirement": "$_rs_escaped",
 "hardenedRuntime": true,
 "timestamp": $_rs_timestamped,
 "notarized": false,
 "executableSha256": "$(sha256_of "$_rs_executable")",
 "warning": $_rs_warning
}
JSON
}
