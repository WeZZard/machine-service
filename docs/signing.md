# Signing the macOS relay agent

The macOS relay agent is signed with an Apple Developer ID Application
certificate, so a privacy permission that an operator grants on a durable Mac
holds for every later release (DC-224). The signing key stays in the owner's
keychain. No key, certificate or password goes to GitHub. The owner signs each
release on their own Mac with `rust/scripts/sign-release.sh`, which then
publishes it.

This page is for two readers:

- the owner, who sets up the certificate once and signs each release;
- the operator of a durable Mac, who grants the permissions once.

## Why a Developer ID signature

- A privacy grant (Accessibility, Automation, Reminders and others) stores the
  designated requirement of the code it was given to. An ad hoc signature's
  requirement is the build's code hash, so every new build loses every grant.
  On a Mac with System Integrity Protection on, nothing but the operator can
  grant again.
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
     `codesign --force --sign "Developer ID Application" --identifier com.wezzard.relay-agent --options runtime --timestamp`;
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
  pre-release. With `CODESIGN_IDENTITY` set, the script signs with Developer ID
  and checks the result as `sign-release.sh` does.
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
- Signing never downgrades (DC-232). machine-service never replaces a relay
  agent signed with Developer ID by a build that is not, on a guest or on a
  Mac, because the grants given to the Developer ID agent would no longer
  match:
  - The installer and the daemon read the installed agent's signature
    before they replace it, and refuse with the reason.
  - At a lease's bootstrap, machine-service reads the guest's agent's
    signature with `codesign` over SSH. A lease whose only build would
    downgrade the agent is served over SSH, and its `link_reason` says why.
  - On a lease that lives on, the agent reports its own signature in its
    hello (relay protocol 3), and a downgrading replacement is not made.
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
- A privacy grant matches the designated requirement, and notarization does not
  change that requirement.
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
not change, so the designated requirement and every grant stay valid.

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
CODESIGN_IDENTITY="Developer ID Application" rust/scripts/build-relay-agent.sh
```

## The operator of a durable Mac: grant the permissions once

1. **Install, and check the team that the Mac trusts.** The first Developer
   ID release that you install sets the team that this Mac trusts, and the
   installer records it. Every later release must be signed by the same team:

   ```zsh
   machine-service-install install --prefix ~/.local/libexec/machine-service --expose-durable on
   ```

   The installer prints `relay agent release signed Developer ID, team
   <TEAM_ID>, trusted as this Mac's first Developer ID team and recorded in
   …/relay/relay-agent-team.json`. Check that `<TEAM_ID>` is the owner's
   team. A pre-release that is signed ad hoc installs too, records no team,
   and its grants end with the next build.

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
3. **Grant the permissions that you add by hand.**
   1. Open System Settings > Privacy & Security.
   2. In Accessibility, and in any other list that a lease needs and that has
      a + button, such as Full Disk Access, click +.
   3. Press Command-Shift-G, enter `~/.local/libexec/relay-agent/relay-agent`,
      and switch the entry on.

   Screen Recording stays with the cua-driver daemon (DC-82).
4. **Answer the prompts that come on first use.** macOS asks for Automation
   (one prompt for each app that a script controls) and for Reminders,
   Contacts, Calendar and Photos the first time that a command needs them.
   Each prompt names `relay-agent`, because every command of a lease is the
   relay agent's child (DM-4).
   1. Sit at the Mac's display, or connect with Screen Sharing.
   2. Run each kind of command once through a lease:

      ```zsh
      machctl acquire --purpose grants --machine <machine>
      machctl exec <machine> -- osascript -e 'tell application "System Events" to get name of every process'
      machctl exec <machine> -- osascript -e 'tell application "Reminders" to get name of every list'
      machctl release <machine>
      ```

   3. Click Allow in each prompt.
5. **Done.** Every later Developer ID release by the same team keeps these
   grants, including the build that the installer or the daemon puts in place.
   An ad hoc pre-release is never installed over the Developer ID agent,
   because that would drop these grants (DC-232). To try one, remove the
   relay agent first (`machine-service-install remove-relay-agent`); its
   grants are not the release's, and you may need to grant again after it.
