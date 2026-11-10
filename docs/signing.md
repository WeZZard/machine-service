# Signing the macOS relay agent

The macOS relay agent is signed with an Apple Developer ID Application
certificate, so every release carries one stable code identity that the
installer and the daemon trust (DC-224, DC-313). The signing key stays in the owner's
keychain. No key, certificate or password goes to GitHub. The owner signs each
release on their own Mac with `rust/scripts/sign-release.sh`, which then
publishes it.

This page is for two readers:

- the owner, who sets up the certificate once and signs each release;
- the operator of a durable Mac, who checks the team that the Mac trusts.

## Why a Developer ID signature

- The installer and the daemon trust a relay agent release by its code
  identity: the identifier, signed by the team that the Mac trusts
  (DC-227, DC-231, DC-232). An ad hoc signature's identity is the build's
  code hash, so it names one build and no team.
- The signature is not for privacy permissions. The relay agent holds none:
  a relayed command runs as a launchd job of its own, and a task that needs a
  permission arranges it itself (DC-309, DC-310, DC-313).
- A Developer ID signature's designated requirement names the identifier and
  the team:

  ```text
  identifier "com.wezzard.relay-agent" and anchor apple generic
    and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */
    and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */
    and certificate leaf[subject.OU] = <TEAM_ID>
  ```

  Every later build that the same team signs meets the requirement, and no one
  else's build does.

## How a release is signed

- The release workflow (`.github/workflows/release.yml`) builds every platform.
  It signs the `darwin-arm64` executable ad hoc, with the identifier
  `com.wezzard.relay-agent` and the hardened runtime. What happens next
  depends on the tag:

| Tag | What the workflow makes | Then |
| --- | --- | --- |
| A release, `v0.3.0` | a draft release; `SIGNING.json` says `"awaitsSigning": true`, and the notes say that the draft awaits signing | the owner runs `rust/scripts/sign-release.sh v0.3.0`, which signs the executable and publishes the release |
| A pre-release, `v0.3.0-rc.1` | a published pre-release, signed ad hoc, with a warning in `SIGNING.json` and at the top of the notes | nothing; the owner may still sign it locally |

- `sign-release.sh <tag>` runs on the owner's Mac. It:
  1. refuses unless the release is a draft, every archive matches
     `SHA256SUMS`, and `SIGNING.json` names this version, an executable that
     awaits signing, and that executable's SHA-256;
  2. signs the executable as
     `codesign --force --sign "Developer ID Application" --identifier com.wezzard.relay-agent --options runtime --timestamp`,
     or with the identity that `--sign-developer-id=IDENTITY` names. A
     `CODESIGN_IDENTITY` in the environment is ignored (DC-250);
  3. refuses unless the designated requirement is the Developer ID
     requirement above, with the team that the signature itself carries, and
     the signature has the hardened runtime and a secure timestamp. With
     `CODESIGN_TEAM` set, it also refuses any other team;
  4. archives the executable again as the build does: one member, mode 755,
     owner root, a fixed time. It writes `SHA256SUMS` over the three archives
     and a new `SIGNING.json`;
  5. uploads those three files to the draft, downloads the archives again and
     checks them against the new sums, removes the awaiting-signature note and
     publishes the release.

  The team appears only where the signature carries it: the executable and
  `SIGNING.json`. It never appears in the notes or in this repository.
  `--dry-run` stops before anything is uploaded. `--dir DIR` signs assets
  already in `DIR` and uploads nothing.
- `rust/scripts/build-relay-agent.sh` prints the designated requirement and
  writes `SIGNING.json`. A local build signed ad hoc makes a version without a
  pre-release part into `<version>-adhoc`, so a local build is always a
  pre-release. Only with `--sign-developer-id` (or
  `--sign-developer-id=IDENTITY`) does the script sign with Developer ID, and
  it then checks the result as `sign-release.sh` does. A `CODESIGN_IDENTITY`
  in the environment, which a shell profile or a CI runner may set, is
  ignored with a warning, so no build signs with the login keychain's key
  unless its command line asks (DC-250).
- `SIGNING.json` records the version, the identifier, the signature
  (`developer-id` or `adhoc`), whether the release awaits signing, the team,
  the designated requirement, the executable's SHA-256 and whether it is
  notarized. pilot-images copies the signature into its lock. Every check reads
  the signature from the executable itself and refuses a record that disagrees.
- Which signature the installer and the daemon accept:
  - A Developer ID signature, by the team that the Mac trusts (DC-231):
    - `MACHINE_SERVICE_RELAY_AGENT_TEAM_ID`, when set, names that team from
      the start.
    - Otherwise, the team that the instance recorded when it first installed
      a Developer ID relay agent. The record is
      `<state>/relay/relay-agent-team.json` in the instance's state folder,
      mode 600.
    - Before any record, the first Developer ID release is trusted, and its
      team is recorded once the agent is in place.
    - A release signed by another team is refused until the operator changes
      that setting. A release's `SIGNING.json` names its team, but it is
      never the trust anchor by itself, because it travels with the archive
      that it describes.
  - An ad hoc signature with the identifier, for a pre-release only.
- The installer and the daemon run the same check (DC-236). The
  daemon checks the release that it holds at startup, and again before it
  uploads a build into a guest or replaces a guest's agent. This holds on
  every instance, including a Tart-only one with durable exposure off, whose
  installation never checks the signature:
  - A release that fails is never sent. Every new lease is served over SSH,
    and its `link_reason` says why.
  - `/health` shows the last check as `relay_agent_release`, with
    `"trusted": true`, the version and the signature kind, or with
    `"trusted": false` and the reason. A reason names a team only by its
    fingerprint.
  - Before any record, the first Developer ID release that the daemon checks
    is trusted and recorded, as an installation records it
    (DC-237).
- Signing never downgrades (DC-232). machine-service never replaces a relay
  agent signed with Developer ID by a build that is not, on a guest or on a
  Mac, so a Mac that trusts a team never runs an agent that the team did not
  sign:
  - The installer and the daemon read the installed agent's signature
    before they replace it, and refuse with the reason.
  - At a lease's bootstrap, machine-service reads the guest's agent's
    signature with `/usr/bin/codesign` over SSH, never with a `codesign`
    that the guest's `PATH` finds (DC-239). A lease whose only build would
    downgrade the agent is served over SSH, and its `link_reason` says why.
  - On a lease that lives on, the agent reports its own signature in its
    hello (relay protocol 3), and a downgrading replacement is not made. An
    agent of an earlier protocol on macOS is asked with
    `/usr/bin/codesign` through the relay.
  - Ad hoc by ad hoc, and ad hoc by Developer ID, are allowed.
- `/health` shows the trusted team as `relay_agent_team`: a fingerprint,
  `sha256:` and 16 hex digits, and whether the team is `configured` or
  `recorded`. It never shows the team ID itself.

## Notarization

The release is not notarized (DC-228):

- Gatekeeper assesses only files that carry the quarantine attribute. A
  browser, or another app that quarantines its downloads, sets that attribute.
- The relay agent reaches a Mac in two ways:
  - pilot-images downloads the archive with Python's `urllib`.
  - The installer copies a release folder.
- `/usr/bin/tar` then unpacks the archive. Neither path sets the attribute, and
  launchd starts the executable without an assessment.
- Notarization does not change the designated requirement that the trust
  check reads.
- A bare executable in a tar archive cannot be stapled. Its ticket could only
  be looked up online.
- If notarization is ever needed, `sign-release.sh` would submit the signed
  executable with `xcrun notarytool submit --keychain-profile <profile>
  --wait`. The App Store Connect credentials would come from the owner's own
  keychain profile, made once with `xcrun notarytool store-credentials`, and
  never from a secret.
- If you download an archive with a browser, remove the attribute before you
  install it:

  ```zsh
  xattr -d com.apple.quarantine relay-agent-<version>-darwin-arm64.tar.gz
  ```

  Alternatively, fetch the archive with `curl` or `gh release download`.

## The owner: set up the certificate once

You need a paid membership of the Apple Developer Program. Only the Account
Holder of a team can create a Developer ID certificate.

1. **Enroll.** Enroll at <https://developer.apple.com/programs/enroll/>. When
   the membership is active, read your team ID at
   <https://developer.apple.com/account> under Membership details. It is ten
   capital letters and digits.
2. **Create the certificate** in your login keychain. Use either way:
   - Xcode > Settings > Accounts > your team > Manage Certificates > + >
     Developer ID Application.
   - Keychain Access:
     1. Choose Certificate Assistant > Request a Certificate From a Certificate
        Authority, and save the request to disk.
     2. Upload it at
        <https://developer.apple.com/account/resources/certificates/add> as
        Developer ID Application, with the G2 Sub-CA profile.
     3. Open the downloaded `.cer`.

   The private key stays in your keychain. Do not export it.
3. **Check the identity.** This read-only command must list one line:

   ```zsh
   security find-identity -v -p codesigning | grep 'Developer ID Application'
   ```

4. **Have the tools.** You need the Xcode command line tools, for `codesign`,
   and the GitHub CLI, signed in with write access to the repository:

   ```zsh
   gh auth status
   ```

When the certificate is renewed, make the new one the same way. The team does
not change, so the designated requirement stays the same.

## The owner: sign each release

1. Push the release tag, `v<version>`, whose version is the relay agent's. The
   workflow makes a draft whose notes say that it awaits signing.
2. In your checkout, at the tag or later, run:

   ```zsh
   rust/scripts/sign-release.sh --dry-run v<version>   # everything but the upload
   rust/scripts/sign-release.sh v<version>             # sign, upload, check and publish
   ```

   The first run of `codesign` may ask to use the key. Choose Always Allow.
3. Check the published release, without running the executable:

   ```zsh
   gh release download v<version> --pattern 'relay-agent-*-darwin-arm64.tar.gz' --pattern SIGNING.json
   tar -xzf relay-agent-<version>-darwin-arm64.tar.gz relay-agent
   codesign -dr - relay-agent        # names com.wezzard.relay-agent and your team
   codesign -dvv relay-agent 2>&1 | grep -E 'Authority|Timestamp|flags'
   ```

A draft that you no longer want is deleted with `gh release delete v<version>`.
If a draft's archive no longer matches its sums, `sign-release.sh` refuses it.
Delete the draft and run the tag's workflow again with `gh run rerun <run-id>`.

To sign a local build with the same identity:

```zsh
rust/scripts/build-relay-agent.sh --sign-developer-id
```

## The operator of a durable Mac: check the trusted team

1. **Install, and check the team that the Mac trusts.** The first Developer
   ID release that you install sets the team that this Mac trusts, and the
   installer records it. Every later release must be signed by the same team:

   ```zsh
   machine-service-install install --prefix ~/.local/libexec/machine-service --expose-durable on
   ```

   The installer prints `relay agent release signed Developer ID, team
   <TEAM_ID>, trusted as this Mac's first Developer ID team and recorded in
   …/relay/relay-agent-team.json`. Check that `<TEAM_ID>` is the owner's
   team. A pre-release that is signed ad hoc installs too and records no team.

   To name the team before the first installation, set
   `MACHINE_SERVICE_RELAY_AGENT_TEAM_ID=<TEAM_ID>` for the install. A later
   installation keeps the setting, and it wins over the record.

   To trust another team later, for example after the owner moved to a new
   team, do one of these, then install again:
   - Set `MACHINE_SERVICE_RELAY_AGENT_TEAM_ID` to the new team.
   - Remove `<state>/relay/relay-agent-team.json`, so the next Developer ID
     release is trusted as the first.

   Until then, the installer and the daemon refuse a release by the new
   team, and say which team the Mac trusts and where that is recorded.
2. **Check the installed agent:**

   ```zsh
   codesign -dr - ~/.local/libexec/relay-agent/relay-agent
   ```

   The output must name `com.wezzard.relay-agent` and your team.

The operator grants the relay agent nothing. Privacy permissions belong to the
tasks that need them (DC-309).
