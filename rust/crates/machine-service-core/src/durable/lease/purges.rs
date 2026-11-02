//! Purges of a relay agent's records that a durable release could not
//! confirm (DC-177, DM-7).
//!
//! Each stays in `<state>/relay-purges.json` until the agent confirms it,
//! and is sent again every time this Mac's relay agent registers. The file
//! is replaced atomically, so a crash leaves either the old list or the new
//! one. The transforms here are pure; [`super::DurableMachine`] reads and
//! writes the file under its own lock.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One purge the relay agent has not confirmed yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingPurge {
    /// The lease whose records the agent still keeps.
    pub lease_id: String,
    /// When the release found the purge unconfirmed.
    pub since: f64,
    /// How many times the purge was sent after that release.
    pub attempts: u64,
    /// When it was last sent, or `since` before any retry.
    pub last_attempt_at: f64,
    /// Why the last attempt was not confirmed.
    pub reason: String,
}

impl PendingPurge {
    /// The operator's view, as the machine record and `/health` show it.
    pub fn to_json(&self) -> Value {
        json!({
            "lease_id": self.lease_id,
            "since": self.since,
            "attempts": self.attempts,
            "last_attempt_at": self.last_attempt_at,
            "reason": self.reason,
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    pending: Vec<PendingPurge>,
}

/// The file that keeps the list in `state_dir`.
pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("relay-purges.json")
}

/// The list in `path`; a missing file is an empty list.
pub fn load(path: &Path) -> Result<Vec<PendingPurge>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<File>(&text)
            .map(|file| file.pending)
            .map_err(|error| format!("cannot read {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// The file's JSON for `pending`.
pub fn to_file(pending: Vec<PendingPurge>) -> Value {
    serde_json::to_value(File { pending }).unwrap_or_else(|_| json!({"pending": []}))
}

/// `pending` with the purge of `lease_id` kept: a lease already listed
/// keeps its `since` and attempts and takes the new reason.
pub fn kept(
    pending: Vec<PendingPurge>,
    lease_id: &str,
    at: f64,
    reason: &str,
) -> Vec<PendingPurge> {
    if pending.iter().any(|purge| purge.lease_id == lease_id) {
        return pending
            .into_iter()
            .map(|purge| {
                if purge.lease_id == lease_id {
                    PendingPurge {
                        reason: reason.to_string(),
                        ..purge
                    }
                } else {
                    purge
                }
            })
            .collect();
    }
    pending
        .into_iter()
        .chain(std::iter::once(PendingPurge {
            lease_id: lease_id.to_string(),
            since: at,
            attempts: 0,
            last_attempt_at: at,
            reason: reason.to_string(),
        }))
        .collect()
}

/// `pending` after one more unconfirmed attempt for `lease_id`.
pub fn attempted(
    pending: Vec<PendingPurge>,
    lease_id: &str,
    at: f64,
    reason: &str,
) -> Vec<PendingPurge> {
    pending
        .into_iter()
        .map(|purge| {
            if purge.lease_id == lease_id {
                PendingPurge {
                    attempts: purge.attempts + 1,
                    last_attempt_at: at,
                    reason: reason.to_string(),
                    ..purge
                }
            } else {
                purge
            }
        })
        .collect()
}

/// `pending` without `lease_id`, whose purge the agent confirmed.
pub fn confirmed(pending: Vec<PendingPurge>, lease_id: &str) -> Vec<PendingPurge> {
    pending
        .into_iter()
        .filter(|purge| purge.lease_id != lease_id)
        .collect()
}

/// The next purge to send in a pass that already sent `sent`.
pub fn next<'a>(pending: &'a [PendingPurge], sent: &[String]) -> Option<&'a PendingPurge> {
    pending.iter().find(|purge| !sent.contains(&purge.lease_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(pending: &[PendingPurge]) -> Vec<&str> {
        pending.iter().map(|p| p.lease_id.as_str()).collect()
    }

    #[test]
    fn dc177_a_purge_is_kept_once_and_its_attempts_counted() {
        let pending = kept(Vec::new(), "lease-a", 10.0, "no session");
        let pending = kept(pending, "lease-b", 11.0, "no session");
        let pending = kept(pending, "lease-a", 12.0, "refused");
        assert_eq!(ids(&pending), ["lease-a", "lease-b"]);
        assert_eq!(pending[0].since, 10.0);
        assert_eq!(pending[0].reason, "refused");

        let pending = attempted(pending, "lease-b", 20.0, "session ended");
        assert_eq!(pending[1].attempts, 1);
        assert_eq!(pending[1].last_attempt_at, 20.0);
        assert_eq!(pending[1].since, 11.0);
        assert_eq!(pending[0].attempts, 0);

        let pending = confirmed(pending, "lease-a");
        assert_eq!(ids(&pending), ["lease-b"]);
    }

    #[test]
    fn dc177_a_pass_sends_each_purge_once() {
        let pending = kept(kept(Vec::new(), "a", 1.0, "x"), "b", 2.0, "x");
        assert_eq!(next(&pending, &[]).unwrap().lease_id, "a");
        assert_eq!(next(&pending, &["a".into()]).unwrap().lease_id, "b");
        assert!(next(&pending, &["a".into(), "b".into()]).is_none());
    }

    #[test]
    fn dc177_the_list_survives_a_round_trip_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(dir.path());
        assert_eq!(load(&path).unwrap(), Vec::new());
        let pending = kept(Vec::new(), "lease-a", 10.0, "no session");
        let text = serde_json::to_string(&to_file(pending.clone())).unwrap();
        std::fs::write(&path, text).unwrap();
        assert_eq!(load(&path).unwrap(), pending);
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_err());
    }
}
