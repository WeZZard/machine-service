//! The server's sessions and their grace periods (RS-5), without I/O, so that
//! TL-1 can drive expiry with a simulated clock.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::wire::ExpiryReason;

/// How long the server remembers that it expired a session, to answer
/// `expired` rather than `unknown` on a late resume.
pub(crate) const TOMBSTONE_TTL: Duration = Duration::from_secs(3600);

pub(crate) enum Resolve<T> {
    Resumed(T),
    Gone(ExpiryReason),
    Forbidden,
}

struct Entry<T> {
    principal: String,
    value: T,
    detached_since: Option<Instant>,
    grace: Duration,
}

pub(crate) struct SessionTable<T> {
    grace: Duration,
    live: HashMap<String, Entry<T>>,
    tombstones: HashMap<String, Instant>,
}

impl<T: Clone> SessionTable<T> {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            live: HashMap::new(),
            tombstones: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn insert(&mut self, id: String, principal: String, value: T) {
        let grace = self.grace;
        self.insert_with_grace(id, principal, value, grace);
    }

    /// Insert a session whose own grace period is `grace`, at most the
    /// table's.
    pub fn insert_with_grace(&mut self, id: String, principal: String, value: T, grace: Duration) {
        self.live.insert(
            id,
            Entry {
                principal,
                value,
                detached_since: None,
                grace: grace.min(self.grace),
            },
        );
    }

    /// The grace period of a live session.
    pub fn grace_of(&self, id: &str) -> Option<Duration> {
        self.live.get(id).map(|e| e.grace)
    }

    /// Reattach a session. Expiry runs first, so a session past its grace
    /// period is never resumed; the caller ends what [`Self::expire_due`] returns.
    pub fn resume(&mut self, id: &str, principal: &str) -> Resolve<T> {
        match self.live.get_mut(id) {
            Some(entry) if entry.principal != principal => Resolve::Forbidden,
            Some(entry) => {
                entry.detached_since = None;
                Resolve::Resumed(entry.value.clone())
            }
            None if self.tombstones.contains_key(id) => Resolve::Gone(ExpiryReason::Expired),
            None => Resolve::Gone(ExpiryReason::Unknown),
        }
    }

    pub fn detach(&mut self, id: &str, now: Instant) {
        if let Some(entry) = self.live.get_mut(id) {
            entry.detached_since.get_or_insert(now);
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<T> {
        self.live.remove(id).map(|e| e.value)
    }

    /// Drop every session detached for longer than the grace period, and
    /// return them so the caller can tell their owners.
    pub fn expire_due(&mut self, now: Instant) -> Vec<T> {
        let due: Vec<String> = self
            .live
            .iter()
            .filter(|(_, e)| e.detached_since.is_some_and(|t| now >= t + e.grace))
            .map(|(id, _)| id.clone())
            .collect();
        self.tombstones
            .retain(|_, at| now.saturating_duration_since(*at) < TOMBSTONE_TTL);
        due.into_iter()
            .filter_map(|id| {
                let entry = self.live.remove(&id)?;
                self.tombstones.insert(id, now);
                Some(entry.value)
            })
            .collect()
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.live.values().map(|e| &e.value)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.live.retain(|_, e| keep(&e.value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_period_expires_and_remembers() {
        let t0 = Instant::now();
        let mut t = SessionTable::new(Duration::from_secs(2));
        t.insert("s".into(), "p".into(), 1);
        t.detach("s", t0);
        assert!(t.expire_due(t0 + Duration::from_secs(1)).is_empty());
        assert!(matches!(t.resume("s", "q"), Resolve::Forbidden));
        assert!(matches!(t.resume("s", "p"), Resolve::Resumed(1)));
        t.detach("s", t0 + Duration::from_secs(1));
        assert_eq!(t.expire_due(t0 + Duration::from_secs(3)), vec![1]);
        assert!(matches!(
            t.resume("s", "p"),
            Resolve::Gone(ExpiryReason::Expired)
        ));
        assert!(matches!(
            t.resume("other", "p"),
            Resolve::Gone(ExpiryReason::Unknown)
        ));
    }

    #[test]
    fn a_session_may_have_a_shorter_grace_but_never_a_longer_one() {
        let t0 = Instant::now();
        let mut t = SessionTable::new(Duration::from_secs(10));
        t.insert_with_grace("short".into(), "p".into(), 1, Duration::from_secs(2));
        t.insert_with_grace("long".into(), "p".into(), 2, Duration::from_secs(60));
        assert_eq!(t.grace_of("short"), Some(Duration::from_secs(2)));
        assert_eq!(t.grace_of("long"), Some(Duration::from_secs(10)));
        t.detach("short", t0);
        t.detach("long", t0);
        assert_eq!(t.expire_due(t0 + Duration::from_secs(3)), vec![1]);
        assert_eq!(t.expire_due(t0 + Duration::from_secs(10)), vec![2]);
    }
}
