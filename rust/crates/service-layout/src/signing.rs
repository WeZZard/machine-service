//! The macOS relay agent's code signature (DC-135, DC-224, DC-1xx-SIGN2-2
//! to DC-1xx-SIGN2-4).
//!
//! A release's macOS executable carries the identifier [`IDENTIFIER`], and
//! one of two signatures:
//!
//! - Developer ID: the designated requirement names the identifier and the
//!   owner's team, so a privacy grant given to one release holds for every
//!   later one that the same team signs. A release is signed this way.
//! - Ad hoc: the designated requirement is the build's code hash. Only a
//!   pre-release, a version with a `-` part, may be ad hoc; a local build
//!   names itself `<version>-adhoc` (`scripts/build-relay-agent.sh`).
//!
//! The team a Developer ID signature must name is never a value of this
//! repository. It is the operator's configuration, [`TEAM_VARIABLE`], or,
//! without one, the team the release's record names; a signature by any
//! other team with the same identifier is refused.
//!
//! The release records which signature it carries in [`METADATA`], which
//! `build-relay-agent.sh` and `sign-release.sh` write. Every check here reads
//! the executable itself and refuses a record that disagrees with it.

use std::path::Path;
use std::process::{Command, Stdio};

/// The code-signing identifier of every macOS build: the LaunchAgent's label.
pub const IDENTIFIER: &str = "com.wezzard.relay-agent";
/// The configuration that names the Developer ID team a release must be
/// signed by.
pub const TEAM_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT_TEAM_ID";
/// The release's record of its macOS executable's signature.
pub const METADATA: &str = "SIGNING.json";
/// The system `codesign`.
const CODESIGN: &str = "/usr/bin/codesign";

/// What follows the identifier in the designated requirement that
/// `codesign` makes for a Developer ID Application certificate: an Apple
/// anchor, the Developer ID intermediate, a Developer ID Application leaf,
/// and the leaf's team.
const DEVELOPER_ID_TAIL: &str = " and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] /* exists */ and certificate leaf[field.1.2.840.113635.100.6.1.13] /* exists */ and certificate leaf[subject.OU] = ";

/// A designated requirement as `codesign -dr -` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Designated {
    /// An ad hoc signature: the requirement is the build's code hashes.
    AdHoc,
    /// A Developer ID signature of `identifier` by `team`.
    DeveloperId {
        /// The signing identifier.
        identifier: String,
        /// The team ID, the leaf certificate's organizational unit.
        team: String,
    },
}

/// The signature a release carries, once accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signing {
    /// Developer ID, by `team`.
    DeveloperId {
        /// The configured team the signature names.
        team: String,
    },
    /// Ad hoc, with [`IDENTIFIER`]; a pre-release only.
    AdHoc,
}

impl Signing {
    /// Its name in the release metadata and the image's lock.
    pub fn kind(&self) -> &'static str {
        match self {
            Signing::DeveloperId { .. } => "developer-id",
            Signing::AdHoc => "adhoc",
        }
    }

    /// The code requirement a privacy grant to this build is pinned to:
    /// the full designated requirement of a Developer ID build, the
    /// identifier alone for an ad hoc one, whose designated requirement
    /// changes with every build.
    pub fn requirement(&self) -> String {
        match self {
            Signing::DeveloperId { team } => developer_id_requirement(IDENTIFIER, team),
            Signing::AdHoc => format!("identifier \"{IDENTIFIER}\""),
        }
    }
}

impl std::fmt::Display for Signing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signing::DeveloperId { team } => write!(f, "Developer ID, team {team}"),
            Signing::AdHoc => write!(f, "ad hoc"),
        }
    }
}

/// The designated requirement `codesign` makes for `identifier` signed with
/// a Developer ID Application certificate of `team`.
pub fn developer_id_requirement(identifier: &str, team: &str) -> String {
    format!("identifier \"{identifier}\"{DEVELOPER_ID_TAIL}\"{team}\"")
}

/// Whether `team` has the form of an Apple team ID: ten capital letters
/// and digits.
pub fn valid_team(team: &str) -> bool {
    team.len() == 10
        && team
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Whether `version` is a pre-release: it has a `-` part, as
/// `0.3.0-rc.1` or a local build's `0.3.0-adhoc`.
pub fn is_prerelease(version: &str) -> bool {
    version.contains('-')
}

/// A string of a requirement: quoted, or a bare token, as `codesign`
/// prints a team that begins with a letter. Answers it and the rest.
fn string(text: &str) -> Option<(&str, &str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let end = quoted.find('"')?;
        return Some((&quoted[..end], &quoted[end + 1..]));
    }
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
        .unwrap_or(text.len());
    (end > 0).then(|| (&text[..end], &text[end..]))
}

/// Read the designated requirement from `codesign -dr -` output: an ad hoc
/// one, or the requirement `codesign` makes for a Developer ID Application
/// certificate. Any other form is refused with its text, since no release
/// is signed that way.
pub fn parse_designated(output: &str) -> Result<Designated, String> {
    let line = output
        .lines()
        .map(|line| line.trim().trim_start_matches("# ").trim())
        .find_map(|line| line.strip_prefix("designated => "))
        .ok_or_else(|| format!("no designated requirement in {output:?}"))?;
    if line
        .split(" or ")
        .all(|part| part.starts_with("cdhash H\"") && part.ends_with('"'))
    {
        return Ok(Designated::AdHoc);
    }
    let unknown = || {
        format!(
            "the designated requirement {line:?} is neither ad hoc nor Developer ID; a release is signed with the identifier {IDENTIFIER}"
        )
    };
    let rest = line.strip_prefix("identifier ").ok_or_else(unknown)?;
    let (identifier, rest) = string(rest).ok_or_else(unknown)?;
    let rest = rest.strip_prefix(DEVELOPER_ID_TAIL).ok_or_else(unknown)?;
    let (team, rest) = string(rest).ok_or_else(unknown)?;
    if !rest.is_empty() || !valid_team(team) {
        return Err(unknown());
    }
    Ok(Designated::DeveloperId {
        identifier: identifier.to_string(),
        team: team.to_string(),
    })
}

/// Accept the designated requirement of a release of `version`, with the
/// operator's configured `team`, or refuse it with the fix: a Developer ID
/// signature must name [`IDENTIFIER`] and the configured team; an ad hoc one
/// only a pre-release may carry.
pub fn accept(
    designated: &Designated,
    version: &str,
    team: Option<&str>,
) -> Result<Signing, String> {
    match designated {
        Designated::DeveloperId { identifier, .. } if identifier != IDENTIFIER => Err(format!(
            "the relay agent {version} is signed with the identifier {identifier}, not {IDENTIFIER} (DC-135)"
        )),
        Designated::DeveloperId { team: signer, .. } => match team {
            None => Err(format!(
                "the relay agent {version} is signed with Developer ID by the team {signer}, and neither {TEAM_VARIABLE} nor the release's {METADATA} names a team; set {TEAM_VARIABLE} to the team you trust to sign it (DC-224)"
            )),
            Some(team) if team != signer => Err(format!(
                "the relay agent {version} is signed with Developer ID by the team {signer}, not {team}, which {TEAM_VARIABLE} names (DC-224)"
            )),
            Some(team) => Ok(Signing::DeveloperId {
                team: team.to_string(),
            }),
        },
        Designated::AdHoc if is_prerelease(version) => Ok(Signing::AdHoc),
        Designated::AdHoc => Err(format!(
            "the relay agent {version} is signed ad hoc; a release must be signed with Developer ID (DC-224), and only a pre-release, such as a local build's {version}-adhoc, may be ad hoc"
        )),
    }
}

/// The configured team: `value` unless empty, which must be a team ID.
pub fn configured_team(value: Option<&str>) -> Result<Option<String>, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(team) if valid_team(team) => Ok(Some(team.to_string())),
        Some(team) => Err(format!(
            "{TEAM_VARIABLE} must be a team ID of ten capital letters and digits; got {team:?}"
        )),
    }
}

/// A release's [`METADATA`]: which signature its macOS executable carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `developer-id` or `adhoc`.
    pub signature: String,
    /// The Developer ID team, if any.
    pub team: Option<String>,
    /// The release still awaits the owner's Developer ID signature.
    pub awaits_signing: bool,
}

impl Record {
    /// The record in the release folder `dir`; `None` when it has none.
    pub fn read(dir: &Path) -> Result<Option<Record>, String> {
        let path = dir.join(METADATA);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Record::parse(&text)
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The record in `text`.
    pub fn parse(text: &str) -> Result<Record, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let signature = value["signature"]
            .as_str()
            .filter(|kind| ["developer-id", "adhoc"].contains(kind))
            .ok_or("its signature is neither developer-id nor adhoc")?
            .to_string();
        let team = match &value["team"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(team) if valid_team(team) => Some(team.clone()),
            _ => return Err("its team is not a team ID".into()),
        };
        if (signature == "developer-id") != team.is_some() {
            return Err("a Developer ID record names its team, an ad hoc one none".into());
        }
        Ok(Record {
            signature,
            team,
            awaits_signing: value["awaitsSigning"].as_bool().unwrap_or(false),
        })
    }

    /// Refuse a record that disagrees with the executable's `signing`.
    pub fn agrees(&self, signing: &Signing) -> Result<(), String> {
        let team = match signing {
            Signing::DeveloperId { team } => Some(team),
            Signing::AdHoc => None,
        };
        if self.signature != signing.kind() || self.team.as_ref() != team {
            return Err(format!(
                "the release's {METADATA} says {}, and its executable is signed {signing}",
                self.signature
            ));
        }
        Ok(())
    }
}

/// The team a release in `dir` must be signed by: the configured `team`,
/// else the one its record names.
pub fn expected_team(dir: &Path, team: Option<&str>) -> Result<Option<String>, String> {
    Ok(match team {
        Some(team) => Some(team.to_string()),
        None => Record::read(dir)?.and_then(|record| record.team),
    })
}

fn codesign(arguments: &[&str], path: &Path) -> Result<std::process::Output, String> {
    Command::new(CODESIGN)
        .args(arguments)
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{CODESIGN}: {error}"))
}

/// The signature of the macOS executable at `path`, of a release of
/// `version`, accepted against the configured `team` and then verified by
/// `codesign` against the requirement its grants are pinned to.
pub fn verify(path: &Path, version: &str, team: Option<&str>) -> Result<Signing, String> {
    let shown = codesign(&["-d", "-r-"], path)?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&shown.stderr),
        String::from_utf8_lossy(&shown.stdout)
    );
    if !shown.status.success() {
        return Err(format!(
            "the release's relay agent {version} is not signed: {}",
            text.trim()
        ));
    }
    let signing = accept(&parse_designated(&text)?, version, team)?;
    let requirement = signing.requirement();
    let verified = codesign(
        &["--verify", "--strict", &format!("-R={requirement}")],
        path,
    )?;
    if !verified.status.success() {
        return Err(format!(
            "the release's relay agent {version} does not satisfy {requirement} ({signing}): {}",
            String::from_utf8_lossy(&verified.stderr).trim()
        ));
    }
    Ok(signing)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `codesign -dr -` output recorded on macOS, with the path and every
    /// team reduced to placeholders (a team that begins with a digit is
    /// printed quoted, any other bare).
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codesign")
            .join(name);
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn recorded_requirements_parse() {
        assert_eq!(
            parse_designated(&fixture("adhoc.txt")).unwrap(),
            Designated::AdHoc
        );
        assert_eq!(
            parse_designated(&fixture("developer-id.txt")).unwrap(),
            Designated::DeveloperId {
                identifier: IDENTIFIER.into(),
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(
            parse_designated(&fixture("developer-id-quoted-team.txt")).unwrap(),
            Designated::DeveloperId {
                identifier: IDENTIFIER.into(),
                team: "9EXAMPLE12".into()
            }
        );
        assert_eq!(
            parse_designated(&fixture("developer-id-other-identifier.txt")).unwrap(),
            Designated::DeveloperId {
                identifier: "com.example.other".into(),
                team: "EXAMPLE123".into()
            }
        );
        let other = parse_designated(&fixture("other-form.txt")).unwrap_err();
        assert!(other.contains("neither ad hoc nor Developer ID"), "{other}");
        assert!(parse_designated("Executable=/tmp/x\n").is_err());
        assert!(parse_designated("designated => identifier \"x\" and anchor apple").is_err());
    }

    #[test]
    fn the_assembled_requirement_is_the_one_codesign_prints() {
        // codesign prints the team bare when it begins with a letter; the
        // assembled requirement quotes it, which compiles to the same blob.
        let printed = fixture("developer-id.txt");
        let assembled = developer_id_requirement(IDENTIFIER, "EXAMPLE123");
        let line = printed
            .lines()
            .nth(1)
            .unwrap()
            .trim_start_matches("designated => ");
        assert_eq!(assembled.replace("\"EXAMPLE123\"", "EXAMPLE123"), line);
        assert_eq!(
            parse_designated(&format!("designated => {assembled}")).unwrap(),
            Designated::DeveloperId {
                identifier: IDENTIFIER.into(),
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(
            Signing::DeveloperId {
                team: "EXAMPLE123".into()
            }
            .requirement(),
            assembled
        );
        assert_eq!(
            Signing::AdHoc.requirement(),
            "identifier \"com.wezzard.relay-agent\""
        );
    }

    #[test]
    fn a_release_is_developer_id_signed_by_the_configured_team() {
        let signed = |team: &str| Designated::DeveloperId {
            identifier: IDENTIFIER.into(),
            team: team.into(),
        };
        assert_eq!(
            accept(&signed("EXAMPLE123"), "1.0.0", Some("EXAMPLE123")).unwrap(),
            Signing::DeveloperId {
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(
            accept(&signed("EXAMPLE123"), "1.0.0-rc.1", Some("EXAMPLE123"))
                .unwrap()
                .kind(),
            "developer-id"
        );
        let unconfigured = accept(&signed("EXAMPLE123"), "1.0.0", None).unwrap_err();
        assert!(unconfigured.contains(TEAM_VARIABLE), "{unconfigured}");
        let other = accept(&signed("EXAMPLE123"), "1.0.0", Some("OTHER45678")).unwrap_err();
        assert!(other.contains("not OTHER45678"), "{other}");
        let identifier = Designated::DeveloperId {
            identifier: "com.example.other".into(),
            team: "EXAMPLE123".into(),
        };
        assert!(accept(&identifier, "1.0.0", Some("EXAMPLE123"))
            .unwrap_err()
            .contains("com.example.other"));
    }

    #[test]
    fn only_a_pre_release_may_be_ad_hoc() {
        assert_eq!(
            accept(&Designated::AdHoc, "1.0.0-rc.1", None).unwrap(),
            Signing::AdHoc
        );
        assert_eq!(
            accept(&Designated::AdHoc, "1.0.0-adhoc", Some("EXAMPLE123")).unwrap(),
            Signing::AdHoc
        );
        let release = accept(&Designated::AdHoc, "1.0.0", Some("EXAMPLE123")).unwrap_err();
        assert!(
            release.contains("must be signed with Developer ID"),
            "{release}"
        );
    }

    #[test]
    fn the_release_record_names_its_signature() {
        let signed = Record::parse(
            r#"{"signature": "developer-id", "team": "EXAMPLE123", "awaitsSigning": false}"#,
        )
        .unwrap();
        assert_eq!(signed.team.as_deref(), Some("EXAMPLE123"));
        signed
            .agrees(&Signing::DeveloperId {
                team: "EXAMPLE123".into(),
            })
            .unwrap();
        assert!(signed.agrees(&Signing::AdHoc).is_err());
        assert!(signed
            .agrees(&Signing::DeveloperId {
                team: "OTHER45678".into()
            })
            .is_err());
        let draft = Record::parse(r#"{"signature": "adhoc", "team": null, "awaitsSigning": true}"#)
            .unwrap();
        assert!(draft.awaits_signing);
        draft.agrees(&Signing::AdHoc).unwrap();
        for bad in [
            r#"{"signature": "developer-id", "team": null}"#,
            r#"{"signature": "adhoc", "team": "EXAMPLE123"}"#,
            r#"{"signature": "other", "team": null}"#,
            r#"{"signature": "developer-id", "team": "lower12345"}"#,
            "not json",
        ] {
            assert!(Record::parse(bad).is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Record::read(dir.path()).unwrap(), None);
        assert_eq!(expected_team(dir.path(), None).unwrap(), None);
        std::fs::write(
            dir.path().join(METADATA),
            r#"{"signature": "developer-id", "team": "EXAMPLE123"}"#,
        )
        .unwrap();
        assert_eq!(
            expected_team(dir.path(), None).unwrap().as_deref(),
            Some("EXAMPLE123")
        );
        assert_eq!(
            expected_team(dir.path(), Some("OTHER45678"))
                .unwrap()
                .as_deref(),
            Some("OTHER45678"),
            "the configuration wins"
        );
    }

    #[test]
    fn the_configured_team_is_a_team_id() {
        assert_eq!(configured_team(None).unwrap(), None);
        assert_eq!(configured_team(Some(" ")).unwrap(), None);
        assert_eq!(
            configured_team(Some("EXAMPLE123")).unwrap(),
            Some("EXAMPLE123".into())
        );
        assert!(configured_team(Some("example123")).is_err());
        assert!(configured_team(Some("SHORT")).is_err());
    }

    /// On a Mac whose keychain holds a Developer ID Application identity:
    /// `RELAY_AGENT_TEST_IDENTITY="Developer ID Application" cargo test -p
    /// service-layout -- --ignored`. A throwaway copy of a system executable
    /// is signed as `sign-release.sh` signs, and is accepted by the team its
    /// own signature carries and refused by any other team or none.
    #[test]
    #[ignore = "needs a Developer ID identity in the keychain"]
    fn codesign_reads_a_developer_id_build() {
        let identity =
            std::env::var("RELAY_AGENT_TEST_IDENTITY").expect("RELAY_AGENT_TEST_IDENTITY");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay-agent");
        std::fs::copy("/usr/bin/true", &path).unwrap();
        let signed = Command::new(CODESIGN)
            .args(["--force", "--sign", &identity, "--identifier", IDENTIFIER])
            .args(["--options", "runtime", "--timestamp"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );
        let shown = codesign(&["-d", "-r-"], &path).unwrap();
        let Designated::DeveloperId { identifier, team } =
            parse_designated(&String::from_utf8_lossy(&shown.stdout)).unwrap()
        else {
            panic!("not a Developer ID requirement")
        };
        assert_eq!(identifier, IDENTIFIER);
        assert_eq!(
            verify(&path, "1.0.0", Some(&team)).unwrap(),
            Signing::DeveloperId { team: team.clone() }
        );
        assert!(verify(&path, "1.0.0", None).is_err());
        let other = if team == "EXAMPLE123" {
            "OTHER45678"
        } else {
            "EXAMPLE123"
        };
        assert!(verify(&path, "1.0.0", Some(other)).is_err());
    }

    /// The real `codesign` on an executable signed ad hoc as the build
    /// script signs it: a pre-release passes with the identifier, a release
    /// is refused, and another identifier is refused either way.
    #[test]
    fn codesign_reads_an_ad_hoc_build() {
        let directory = tempfile::tempdir().unwrap();
        let sign = |identifier: &str| {
            let path = directory.path().join(identifier);
            std::fs::copy("/usr/bin/true", &path).unwrap();
            let status = Command::new(CODESIGN)
                .args([
                    "--force",
                    "--sign",
                    "-",
                    "--options",
                    "runtime",
                    "--identifier",
                    identifier,
                ])
                .arg(&path)
                .output()
                .unwrap()
                .status;
            assert!(status.success());
            path
        };
        let ours = sign(IDENTIFIER);
        assert_eq!(verify(&ours, "1.0.0-adhoc", None).unwrap(), Signing::AdHoc);
        assert!(verify(&ours, "1.0.0", None)
            .unwrap_err()
            .contains("must be signed with Developer ID"));
        let theirs = sign("com.example.other");
        assert!(verify(&theirs, "1.0.0-adhoc", None)
            .unwrap_err()
            .contains("does not satisfy"));
        let unsigned = directory.path().join("unsigned");
        std::fs::write(&unsigned, b"not code").unwrap();
        assert!(verify(&unsigned, "1.0.0-adhoc", None)
            .unwrap_err()
            .contains("not signed"));
    }
}
