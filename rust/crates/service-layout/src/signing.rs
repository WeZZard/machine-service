//! The macOS relay agent's code signature (DC-135, DC-224, DC-227
//! to DC-232).
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
//! repository. A Mac trusts the first Developer ID team it installs a relay
//! agent from and records it in its instance's [`TRUST_RECORD`]; the
//! operator's [`TEAM_VARIABLE`], when set, is that setting from the start
//! ([`Anchor`], DC-231). A signature by any other team with the same
//! identifier is refused until the operator changes the setting. The
//! release's own record is never the anchor, because it travels with the
//! archive it describes.
//!
//! Signing never downgrades (DC-232): a relay agent in place signed with
//! Developer ID is never replaced by a build that is not ([`downgrade`]),
//! because the privacy grants given to the Developer ID requirement would
//! no longer match.
//!
//! The release records which signature it carries in [`METADATA`], which
//! `build-relay-agent.sh` and `sign-release.sh` write. Every check here reads
//! the executable itself and refuses a record that disagrees with it.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

/// The code-signing identifier of every macOS build: the LaunchAgent's label.
pub const IDENTIFIER: &str = "com.wezzard.relay-agent";
/// The configuration that names the Developer ID team a release must be
/// signed by.
pub const TEAM_VARIABLE: &str = "MACHINE_SERVICE_RELAY_AGENT_TEAM_ID";
/// The release's record of its macOS executable's signature.
pub const METADATA: &str = "SIGNING.json";
/// The instance's record of the Developer ID team it trusts, under its
/// state folder, mode 600 (DC-231).
pub const TRUST_RECORD: &str = "relay/relay-agent-team.json";
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

/// Accept the designated requirement of a release of `version` against the
/// instance's trust `anchor`, or refuse it with the fix: a Developer ID
/// signature must name [`IDENTIFIER`] and the trusted team, or, when the
/// instance trusts none yet, is trusted on first use; an ad hoc one only a
/// pre-release may carry (DC-224, DC-231).
pub fn accept(designated: &Designated, version: &str, anchor: &Anchor) -> Result<Signing, String> {
    match designated {
        Designated::DeveloperId { identifier, .. } if identifier != IDENTIFIER => Err(format!(
            "the relay agent {version} is signed with the identifier {identifier}, not {IDENTIFIER} (DC-135)"
        )),
        Designated::DeveloperId { team: signer, .. } => match (&anchor.configured, &anchor.recorded) {
            (Some(team), _) if team != signer => Err(format!(
                "the relay agent {version} is signed with Developer ID by the team {signer}, not {team}, which {TEAM_VARIABLE} names; set {TEAM_VARIABLE} to the team you trust to sign it (DC-231)"
            )),
            (None, Some(team)) if team != signer => Err(format!(
                "the relay agent {version} is signed with Developer ID by the team {signer}, and this Mac trusts the team {team}, which it recorded in {} when it first installed a relay agent; a release by another team is refused until you change that setting: set {TEAM_VARIABLE} to the team you now trust, or remove that file (DC-231)",
                anchor.record.display()
            )),
            _ => Ok(Signing::DeveloperId {
                team: signer.clone(),
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

/// The Developer ID team an instance trusts to sign its relay agent
/// (DC-231): the operator's [`TEAM_VARIABLE`], else the team the
/// instance's [`TRUST_RECORD`] names, else none yet, when the first
/// Developer ID release it installs is trusted and recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// The team [`TEAM_VARIABLE`] names.
    pub configured: Option<String>,
    /// The team the record names.
    pub recorded: Option<String>,
    /// The record's path.
    pub record: PathBuf,
}

/// Where the instance's trust is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// [`TEAM_VARIABLE`].
    Configured,
    /// The instance's [`TRUST_RECORD`].
    Recorded,
}

impl Origin {
    /// Its name in the record and in `/health`.
    pub fn name(self) -> &'static str {
        match self {
            Origin::Configured => "configured",
            Origin::Recorded => "recorded",
        }
    }
}

impl Anchor {
    /// The anchor of the instance whose state folder is `state_dir`, with
    /// the operator's `configured` team. A record that cannot be read is
    /// refused, never taken as no trust yet.
    pub fn resolve(state_dir: &Path, configured: Option<String>) -> Result<Anchor, String> {
        let record = state_dir.join(TRUST_RECORD);
        let recorded = read_trust_record(&record)?;
        Ok(Anchor {
            configured,
            recorded,
            record,
        })
    }

    /// The trusted team and where it is set; `None` before the first
    /// Developer ID installation.
    pub fn team(&self) -> Option<(&str, Origin)> {
        match (&self.configured, &self.recorded) {
            (Some(team), _) => Some((team, Origin::Configured)),
            (None, Some(team)) => Some((team, Origin::Recorded)),
            (None, None) => None,
        }
    }

    /// The team to record once `signing` is installed: a Developer ID team
    /// that the record does not name yet. The configured team is recorded
    /// too, so the Mac keeps trusting it when the setting is removed.
    pub fn to_record<'a>(&self, signing: &'a Signing) -> Option<&'a str> {
        match signing {
            Signing::DeveloperId { team } if self.recorded.as_deref() != Some(team) => Some(team),
            _ => None,
        }
    }

    /// Record `team` as the trusted one, atomically, mode 600.
    pub fn record(&self, team: &str) -> Result<(), String> {
        let origin = if self.configured.is_some() {
            "configured"
        } else {
            "first-install"
        };
        let text = serde_json::to_string_pretty(&serde_json::json!({
            "team": team,
            "origin": origin,
            "recordedAt": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }))
        .map_err(|e| e.to_string())?;
        write_private(&self.record, format!("{text}\n").as_bytes())
    }

    /// What `/health` shows of the trust: the team's [`fingerprint`] and
    /// where it is set, never the team itself; `null` before any.
    pub fn health(&self) -> serde_json::Value {
        match self.team() {
            Some((team, origin)) => serde_json::json!({
                "fingerprint": fingerprint(team),
                "origin": origin.name(),
            }),
            None => serde_json::Value::Null,
        }
    }
}

/// A team's fingerprint: `sha256:` and the first 16 hex digits of the
/// SHA-256 of its ID, so `/health` can show which team is trusted, and
/// that it changed, without naming it.
pub fn fingerprint(team: &str) -> String {
    let digest = Sha256::digest(team.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// `text` with each team ID of `teams` shown as its [`fingerprint`]
/// instead, for a message that leaves the operator's terminal, such as a
/// lease's `link_reason` or `/health` (DC-238).
pub fn redact<'a>(text: &str, teams: impl IntoIterator<Item = &'a str>) -> String {
    teams
        .into_iter()
        .filter(|team| valid_team(team))
        .fold(text.to_string(), |text, team| {
            text.replace(team, &fingerprint(team))
        })
}

fn read_trust_record(path: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{}: {error}; fix or remove it", path.display()))?;
    match value["team"].as_str() {
        Some(team) if valid_team(team) => Ok(Some(team.to_string())),
        _ => Err(format!(
            "{} names no team ID; fix or remove it",
            path.display()
        )),
    }
}

/// Write `bytes` to `path` through a temporary sibling and a rename, mode
/// 600, in a folder made mode 700 when it is new.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", path.display());
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    if !parent.is_dir() {
        std::fs::create_dir_all(parent).map_err(at)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(at)?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let temporary = parent.join(format!(".{name}.{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(at)
}

/// The signature of a relay agent in place, as `codesign -dr -` shows it,
/// whatever its form: what [`downgrade`] compares, and what the agent's
/// hello reports of itself (DC-232).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// Developer ID with [`IDENTIFIER`], by `team`.
    DeveloperId {
        /// The team ID.
        team: String,
    },
    /// Ad hoc.
    AdHoc,
    /// Not signed at all, as a Linux build.
    Unsigned,
    /// Any other signature.
    Other,
}

impl Signature {
    /// Read `codesign -d -r-` output, standard error and output together.
    pub fn from_codesign(output: &str) -> Signature {
        match parse_designated(output) {
            Ok(Designated::AdHoc) => Signature::AdHoc,
            Ok(Designated::DeveloperId { identifier, team }) if identifier == IDENTIFIER => {
                Signature::DeveloperId { team }
            }
            Ok(Designated::DeveloperId { .. }) => Signature::Other,
            Err(_) if output.contains("not signed at all") => Signature::Unsigned,
            Err(_) => Signature::Other,
        }
    }

    /// The signature a hello names with `kind` and `team`; an unknown kind
    /// or a Developer ID one without a valid team is [`Signature::Other`].
    pub fn from_parts(kind: &str, team: Option<&str>) -> Signature {
        match (kind, team) {
            ("developer-id", Some(team)) if valid_team(team) => Signature::DeveloperId {
                team: team.to_string(),
            },
            ("adhoc", _) => Signature::AdHoc,
            ("unsigned", _) => Signature::Unsigned,
            _ => Signature::Other,
        }
    }

    /// `developer-id`, `adhoc`, `unsigned` or `other`.
    pub fn kind(&self) -> &'static str {
        match self {
            Signature::DeveloperId { .. } => "developer-id",
            Signature::AdHoc => "adhoc",
            Signature::Unsigned => "unsigned",
            Signature::Other => "other",
        }
    }

    /// The Developer ID team, if any.
    pub fn team(&self) -> Option<&str> {
        match self {
            Signature::DeveloperId { team } => Some(team),
            _ => None,
        }
    }
}

impl From<&Signing> for Signature {
    fn from(signing: &Signing) -> Signature {
        match signing {
            Signing::DeveloperId { team } => Signature::DeveloperId { team: team.clone() },
            Signing::AdHoc => Signature::AdHoc,
        }
    }
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Signature::DeveloperId { team } => {
                write!(f, "Developer ID (team {})", fingerprint(team))
            }
            Signature::AdHoc => write!(f, "ad hoc"),
            Signature::Unsigned => write!(f, "unsigned"),
            Signature::Other => write!(f, "with another signature"),
        }
    }
}

/// Why replacing a relay agent signed `present` with a build signed
/// `incoming` would downgrade its signing, if it would: a Developer ID
/// agent is replaced only by a Developer ID build (DC-232). Any other
/// replacement, such as ad hoc by ad hoc or ad hoc by Developer ID, is
/// allowed.
pub fn downgrade(present: &Signature, incoming: &Signature) -> Option<String> {
    match (present, incoming) {
        (Signature::DeveloperId { .. }, Signature::DeveloperId { .. }) => None,
        (Signature::DeveloperId { .. }, _) => Some(format!(
            "the relay agent in place is signed {present} and the build to replace it is signed {incoming}; signing never downgrades, because the privacy grants given to the Developer ID agent would no longer match (DC-232)"
        )),
        _ => None,
    }
}

/// The signature of the executable at `path`, whatever its form.
pub fn inspect(path: &Path) -> Result<Signature, String> {
    let shown = codesign(&["-d", "-r-"], path)?;
    Ok(Signature::from_codesign(&format!(
        "{}{}",
        String::from_utf8_lossy(&shown.stderr),
        String::from_utf8_lossy(&shown.stdout)
    )))
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
/// `version`, accepted against the instance's trust `anchor` and then
/// verified by `codesign` against the requirement its grants are pinned to.
pub fn verify(path: &Path, version: &str, anchor: &Anchor) -> Result<Signing, String> {
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
    let signing = accept(&parse_designated(&text)?, version, anchor)?;
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

    fn anchor(configured: Option<&str>, recorded: Option<&str>) -> Anchor {
        Anchor {
            configured: configured.map(str::to_string),
            recorded: recorded.map(str::to_string),
            record: PathBuf::from("state").join(TRUST_RECORD),
        }
    }

    fn signed(team: &str) -> Designated {
        Designated::DeveloperId {
            identifier: IDENTIFIER.into(),
            team: team.into(),
        }
    }

    #[test]
    fn the_first_developer_id_team_is_trusted_and_then_recorded() {
        let first = anchor(None, None);
        let signing = accept(&signed("EXAMPLE123"), "1.0.0", &first).unwrap();
        assert_eq!(
            signing,
            Signing::DeveloperId {
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(first.to_record(&signing), Some("EXAMPLE123"));
        assert_eq!(first.team(), None);
        // Once recorded, the same team is accepted and not recorded again.
        let recorded = anchor(None, Some("EXAMPLE123"));
        let again = accept(&signed("EXAMPLE123"), "1.0.1", &recorded).unwrap();
        assert_eq!(recorded.to_record(&again), None);
        assert_eq!(recorded.team(), Some(("EXAMPLE123", Origin::Recorded)));
        // An ad hoc pre-release records nothing.
        assert_eq!(first.to_record(&Signing::AdHoc), None);
    }

    #[test]
    fn a_release_by_another_team_is_refused_until_the_setting_changes() {
        let recorded = anchor(None, Some("EXAMPLE123"));
        let other = accept(&signed("OTHER45678"), "1.1.0", &recorded).unwrap_err();
        assert!(other.contains("OTHER45678"), "{other}");
        assert!(other.contains("trusts the team EXAMPLE123"), "{other}");
        assert!(other.contains(TEAM_VARIABLE), "{other}");
        assert!(other.contains(TRUST_RECORD), "{other}");
        assert!(other.contains("DC-231"), "{other}");
        // The operator's setting is the fix.
        let changed = anchor(Some("OTHER45678"), Some("EXAMPLE123"));
        let signing = accept(&signed("OTHER45678"), "1.1.0", &changed).unwrap();
        assert_eq!(changed.to_record(&signing), Some("OTHER45678"));
        let identifier = Designated::DeveloperId {
            identifier: "com.example.other".into(),
            team: "EXAMPLE123".into(),
        };
        assert!(accept(&identifier, "1.0.0", &recorded)
            .unwrap_err()
            .contains("com.example.other"));
    }

    #[test]
    fn the_configured_team_wins_from_the_start() {
        let configured = anchor(Some("EXAMPLE123"), None);
        assert_eq!(configured.team(), Some(("EXAMPLE123", Origin::Configured)));
        let refused = accept(&signed("OTHER45678"), "1.0.0", &configured).unwrap_err();
        assert!(refused.contains("not EXAMPLE123"), "{refused}");
        assert!(refused.contains(TEAM_VARIABLE), "{refused}");
        let signing = accept(&signed("EXAMPLE123"), "1.0.0", &configured).unwrap();
        assert_eq!(configured.to_record(&signing), Some("EXAMPLE123"));
        // It wins over a record of another team.
        let over = anchor(Some("EXAMPLE123"), Some("OTHER45678"));
        assert_eq!(over.team(), Some(("EXAMPLE123", Origin::Configured)));
        assert!(accept(&signed("OTHER45678"), "1.0.0", &over).is_err());
        assert!(accept(&signed("EXAMPLE123"), "1.0.0", &over).is_ok());
    }

    #[test]
    fn only_a_pre_release_may_be_ad_hoc() {
        assert_eq!(
            accept(&Designated::AdHoc, "1.0.0-rc.1", &anchor(None, None)).unwrap(),
            Signing::AdHoc
        );
        assert_eq!(
            accept(
                &Designated::AdHoc,
                "1.0.0-adhoc",
                &anchor(Some("EXAMPLE123"), None)
            )
            .unwrap(),
            Signing::AdHoc
        );
        let release = accept(&Designated::AdHoc, "1.0.0", &anchor(None, None)).unwrap_err();
        assert!(
            release.contains("must be signed with Developer ID"),
            "{release}"
        );
    }

    #[test]
    fn the_trust_record_is_private_atomic_and_never_silently_lost() {
        let state = tempfile::tempdir().unwrap();
        let first = Anchor::resolve(state.path(), None).unwrap();
        assert_eq!(first.recorded, None);
        assert_eq!(first.health(), serde_json::Value::Null);
        first.record("EXAMPLE123").unwrap();
        let path = state.path().join(TRUST_RECORD);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let folder = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(folder.permissions().mode() & 0o777, 0o700);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("first-install"), "{text}");
        // No temporary file is left beside it.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let again = Anchor::resolve(state.path(), None).unwrap();
        assert_eq!(again.recorded.as_deref(), Some("EXAMPLE123"));
        let health = again.health();
        assert_eq!(health["origin"], "recorded");
        assert_eq!(health["fingerprint"], fingerprint("EXAMPLE123"));
        assert!(!health.to_string().contains("EXAMPLE123"), "{health}");
        assert!(fingerprint("EXAMPLE123").starts_with("sha256:"));
        assert_eq!(fingerprint("EXAMPLE123").len(), 7 + 16);
        assert_ne!(fingerprint("EXAMPLE123"), fingerprint("OTHER45678"));
        let configured = Anchor::resolve(state.path(), Some("OTHER45678".into())).unwrap();
        assert_eq!(configured.health()["origin"], "configured");
        configured.record("OTHER45678").unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"configured\""));
        // A damaged record is refused, never read as no trust yet.
        std::fs::write(&path, "{").unwrap();
        assert!(Anchor::resolve(state.path(), None)
            .unwrap_err()
            .contains("fix or remove it"));
        std::fs::write(&path, r#"{"team": "lower"}"#).unwrap();
        assert!(Anchor::resolve(state.path(), None).is_err());
    }

    /// DC-238: a refusal that leaves the operator's terminal names
    /// every team by its fingerprint; anything that is not a team ID, such
    /// as `SHA256SUMS`, is left as it is.
    #[test]
    fn a_redacted_refusal_names_teams_by_fingerprint() {
        let refusal = accept(
            &Designated::DeveloperId {
                identifier: IDENTIFIER.into(),
                team: "OTHER45678".into(),
            },
            "1.0.0",
            &anchor(None, Some("EXAMPLE123")),
        )
        .unwrap_err();
        assert!(refusal.contains("EXAMPLE123") && refusal.contains("OTHER45678"));
        let redacted = redact(
            &format!("{refusal}; SHA256SUMS"),
            ["EXAMPLE123", "OTHER45678", "not-a-team"],
        );
        assert!(!redacted.contains("EXAMPLE123"), "{redacted}");
        assert!(!redacted.contains("OTHER45678"), "{redacted}");
        assert!(redacted.contains(&fingerprint("EXAMPLE123")), "{redacted}");
        assert!(redacted.contains(&fingerprint("OTHER45678")), "{redacted}");
        assert!(redacted.contains("SHA256SUMS"), "{redacted}");
    }

    #[test]
    fn signing_never_downgrades() {
        let developer = |team: &str| Signature::DeveloperId { team: team.into() };
        let refused = downgrade(&developer("EXAMPLE123"), &Signature::AdHoc).unwrap();
        assert!(refused.contains("DC-232"), "{refused}");
        assert!(!refused.contains("EXAMPLE123"), "{refused}");
        assert!(downgrade(&developer("EXAMPLE123"), &Signature::Unsigned).is_some());
        assert!(downgrade(&developer("EXAMPLE123"), &Signature::Other).is_some());
        assert_eq!(
            downgrade(&developer("EXAMPLE123"), &developer("EXAMPLE123")),
            None
        );
        assert_eq!(downgrade(&Signature::AdHoc, &Signature::AdHoc), None);
        assert_eq!(downgrade(&Signature::AdHoc, &developer("EXAMPLE123")), None);
        assert_eq!(downgrade(&Signature::Unsigned, &Signature::AdHoc), None);
        assert_eq!(downgrade(&Signature::Other, &Signature::AdHoc), None);
    }

    #[test]
    fn a_signature_in_place_reads_from_codesign_or_a_hello() {
        assert_eq!(
            Signature::from_codesign(&fixture("adhoc.txt")),
            Signature::AdHoc
        );
        assert_eq!(
            Signature::from_codesign(&fixture("developer-id.txt")),
            Signature::DeveloperId {
                team: "EXAMPLE123".into()
            }
        );
        assert_eq!(
            Signature::from_codesign(&fixture("developer-id-other-identifier.txt")),
            Signature::Other
        );
        assert_eq!(
            Signature::from_codesign("/tmp/x: code object is not signed at all\n"),
            Signature::Unsigned
        );
        for signature in [
            Signature::DeveloperId {
                team: "EXAMPLE123".into(),
            },
            Signature::AdHoc,
            Signature::Unsigned,
            Signature::Other,
        ] {
            assert_eq!(
                Signature::from_parts(signature.kind(), signature.team()),
                signature
            );
        }
        assert_eq!(
            Signature::from_parts("developer-id", None),
            Signature::Other
        );
        assert_eq!(
            Signature::from(&Signing::DeveloperId {
                team: "EXAMPLE123".into()
            })
            .team(),
            Some("EXAMPLE123")
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
    /// own signature carries or on first use, and refused by any other.
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
            verify(&path, "1.0.0", &anchor(Some(&team), None)).unwrap(),
            Signing::DeveloperId { team: team.clone() }
        );
        // Trusted on first use, and by the record that use leaves.
        assert!(verify(&path, "1.0.0", &anchor(None, None)).is_ok());
        assert!(verify(&path, "1.0.0", &anchor(None, Some(&team))).is_ok());
        assert_eq!(
            inspect(&path).unwrap(),
            Signature::DeveloperId { team: team.clone() }
        );
        let other = if team == "EXAMPLE123" {
            "OTHER45678"
        } else {
            "EXAMPLE123"
        };
        assert!(verify(&path, "1.0.0", &anchor(Some(other), None)).is_err());
        assert!(verify(&path, "1.0.0", &anchor(None, Some(other))).is_err());
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
        let none = anchor(None, None);
        let ours = sign(IDENTIFIER);
        assert_eq!(verify(&ours, "1.0.0-adhoc", &none).unwrap(), Signing::AdHoc);
        assert_eq!(inspect(&ours).unwrap(), Signature::AdHoc);
        assert!(verify(&ours, "1.0.0", &none)
            .unwrap_err()
            .contains("must be signed with Developer ID"));
        let theirs = sign("com.example.other");
        assert!(verify(&theirs, "1.0.0-adhoc", &none)
            .unwrap_err()
            .contains("does not satisfy"));
        let unsigned = directory.path().join("unsigned");
        std::fs::write(&unsigned, b"not code").unwrap();
        assert!(verify(&unsigned, "1.0.0-adhoc", &none)
            .unwrap_err()
            .contains("not signed"));
        assert_eq!(inspect(&unsigned).unwrap(), Signature::Unsigned);
    }
}
