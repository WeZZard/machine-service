//! Bearer credentials (RP-3): issued as random tokens, kept only as their
//! SHA-256, checked on every hello.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use base64::Engine;
use sha2::{Digest, Sha256};

/// What a session is for (RP-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// A relay agent: `{"role":"agent"}`.
    Agent,
    /// A client of a lease: `{"role":"client"}`.
    Client,
}

impl Role {
    /// The role in hello attributes.
    pub fn name(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Client => "client",
        }
    }

    /// The role a hello's attributes name.
    pub fn of(attributes: &serde_json::Value) -> Option<Role> {
        match attributes.get("role").and_then(serde_json::Value::as_str) {
            Some("agent") => Some(Role::Agent),
            Some("client") => Some(Role::Client),
            _ => None,
        }
    }
}

/// What a credential reaches: a virtual machine's lease, or a durable
/// machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Target {
    /// A virtual machine, by its lease's machine name.
    Lease(String),
    /// A durable machine, by its name.
    Durable(String),
}

impl Target {
    /// The machine name a client sees.
    pub fn name(&self) -> &str {
        match self {
            Target::Lease(name) | Target::Durable(name) => name,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Lease(name) => write!(f, "vm/{name}"),
            Target::Durable(name) => write!(f, "durable/{name}"),
        }
    }
}

/// The SHA-256 of a credential, lower-case hexadecimal, as the broker and a
/// lease record keep it.
pub fn credential_sha256(credential: &str) -> String {
    Sha256::digest(credential.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A new credential: 32 random bytes as 43 URL-safe base64 characters.
pub fn new_credential() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

/// A check that a credential is still good, asked on every hello, such as a
/// durable lease that must still be the machine's current lease.
pub type Validity = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Grant {
    pub role: Role,
    pub target: Target,
    pub valid: Option<Validity>,
}

/// The credentials the broker accepts, by SHA-256.
#[derive(Default)]
pub(crate) struct Grants {
    by_hash: HashMap<String, Grant>,
}

impl Grants {
    pub fn admit(&mut self, sha256: String, grant: Grant) {
        self.by_hash.insert(sha256, grant);
    }

    /// The grant a credential carries, if it is still good.
    pub fn check(&self, credential: &str) -> Option<(String, Grant)> {
        let hash = credential_sha256(credential);
        let grant = self.by_hash.get(&hash)?.clone();
        if let Some(valid) = &grant.valid {
            if !valid() {
                return None;
            }
        }
        Some((hash, grant))
    }

    /// Drop every grant of `target` for `roles`.
    pub fn revoke(&mut self, target: &Target, roles: &[Role]) -> usize {
        let before = self.by_hash.len();
        self.by_hash
            .retain(|_, grant| !(grant.target == *target && roles.contains(&grant.role)));
        before - self.by_hash.len()
    }

    pub fn has(&self, target: &Target, role: Role) -> bool {
        self.by_hash
            .values()
            .any(|grant| grant.target == *target && grant.role == role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_random_url_safe_and_kept_as_hashes() {
        let a = new_credential();
        let b = new_credential();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let mut grants = Grants::default();
        let target = Target::Lease("vm-a".into());
        grants.admit(
            credential_sha256(&a),
            Grant {
                role: Role::Client,
                target: target.clone(),
                valid: None,
            },
        );
        assert!(grants.check(&a).is_some());
        assert!(grants.check(&b).is_none());
        assert_eq!(grants.revoke(&target, &[Role::Agent]), 0);
        assert_eq!(grants.revoke(&target, &[Role::Client]), 1);
        assert!(grants.check(&a).is_none());
    }

    #[test]
    fn a_validity_check_runs_on_every_use() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let seen = flag.clone();
        let mut grants = Grants::default();
        grants.admit(
            credential_sha256("t"),
            Grant {
                role: Role::Client,
                target: Target::Durable("m".into()),
                valid: Some(Arc::new(move || {
                    seen.load(std::sync::atomic::Ordering::SeqCst)
                })),
            },
        );
        assert!(grants.check("t").is_some());
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(grants.check("t").is_none());
    }

    #[test]
    fn roles_come_from_the_attributes() {
        use serde_json::json;
        assert_eq!(Role::of(&json!({"role": "agent"})), Some(Role::Agent));
        assert_eq!(Role::of(&json!({"role": "client"})), Some(Role::Client));
        assert_eq!(Role::of(&json!({"role": "admin"})), None);
        assert_eq!(Role::of(&json!(null)), None);
        assert_eq!(Target::Lease("x".into()).to_string(), "vm/x");
        assert_eq!(Target::Durable("m".into()).name(), "m");
    }
}
