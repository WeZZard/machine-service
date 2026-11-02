//! The durable record of execution IDs (`docs/arch/relay-agent.md`, RA-5).
//!
//! Each execution ID that passed its commit point has a directory under
//! `<state>/executions/`, named by the SHA-256 of the ID so that any ID is a
//! safe name. It holds `record.json` and, for a command, `output`: the
//! command's combined output, which a repeated call replays. A record is
//! written to the disk before the operation takes effect, so an agent that
//! restarts finds every execution it started and runs none of them again.

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::protocol::{OpKind, Outcome};

/// One execution ID and what became of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The client's execution ID.
    pub execution: String,
    /// The operation.
    pub op: OpKind,
    /// The outcome; `None` while the operation runs.
    pub outcome: Option<Outcome>,
    /// When the operation passed its commit point, in Unix milliseconds.
    pub started_ms: u64,
    /// When it completed, in Unix milliseconds.
    pub finished_ms: Option<u64>,
    /// Whether a link has carried the outcome.
    pub reported: bool,
}

/// The records of one state directory, kept in memory and written through.
pub struct Store {
    root: PathBuf,
    records: HashMap<String, Record>,
}

/// The current time in Unix milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

impl Store {
    /// Open the records under `state`, creating the directory private to the
    /// user. A record left running by an agent that stopped becomes
    /// [`Outcome::Interrupted`]: its effect is uncertain and it never runs
    /// again. Completed records older than `retention` are dropped.
    pub fn open(state: &Path, retention: Duration) -> io::Result<Store> {
        let root = state.join("executions");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        let mut store = Store {
            root,
            records: HashMap::new(),
        };
        for entry in fs::read_dir(&store.root)? {
            let directory = entry?.path();
            let record = fs::read(directory.join("record.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
            match record {
                Some(record) if store.directory(&record.execution) == directory => {
                    store.records.insert(record.execution.clone(), record);
                }
                // A directory without a readable record never passed its
                // commit point: the record is written before anything runs.
                _ => {
                    let _ = fs::remove_dir_all(&directory);
                }
            }
        }
        let stale: Vec<String> = store
            .records
            .values()
            .filter(|record| record.outcome.is_none())
            .map(|record| record.execution.clone())
            .collect();
        for execution in stale {
            store.complete(
                &execution,
                Outcome::Interrupted {
                    message: "the relay agent stopped while the operation ran".into(),
                },
            )?;
        }
        store.prune(retention);
        Ok(store)
    }

    /// The record of `execution`, if any.
    pub fn get(&self, execution: &str) -> Option<&Record> {
        self.records.get(execution)
    }

    /// Every record.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    /// The file that holds a command's output.
    pub fn output(&self, execution: &str) -> PathBuf {
        self.directory(execution).join("output")
    }

    /// Record that `execution` passed its commit point. This reaches the
    /// disk before the operation may take effect.
    pub fn begin(&mut self, execution: &str, op: OpKind) -> io::Result<()> {
        let directory = self.directory(execution);
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .or_else(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })?;
        let record = Record {
            execution: execution.to_string(),
            op,
            outcome: None,
            started_ms: now_ms(),
            finished_ms: None,
            reported: false,
        };
        self.save(&record)?;
        self.records.insert(execution.to_string(), record);
        Ok(())
    }

    /// Record the outcome of `execution`.
    pub fn complete(&mut self, execution: &str, outcome: Outcome) -> io::Result<()> {
        let Some(mut record) = self.records.get(execution).cloned() else {
            return Ok(());
        };
        record.outcome = Some(outcome);
        record.finished_ms = Some(now_ms());
        record.reported = false;
        // The record stays in memory even if the disk fails, so this agent
        // still never runs the ID again.
        self.records.insert(execution.to_string(), record.clone());
        self.save(&record)
    }

    /// Record that a link carried the outcome of `execution`.
    pub fn mark_reported(&mut self, execution: &str) {
        if let Some(record) = self.records.get(execution) {
            if record.outcome.is_some() && !record.reported {
                let mut record = record.clone();
                record.reported = true;
                let _ = self.save(&record);
                self.records.insert(execution.to_string(), record);
            }
        }
    }

    /// Drop completed records older than `retention`, with their output.
    pub fn prune(&mut self, retention: Duration) {
        let horizon = now_ms().saturating_sub(retention.as_millis() as u64);
        let expired: Vec<String> = self
            .records
            .values()
            .filter(|record| record.finished_ms.is_some_and(|ms| ms < horizon))
            .map(|record| record.execution.clone())
            .collect();
        for execution in expired {
            let _ = fs::remove_dir_all(self.directory(&execution));
            self.records.remove(&execution);
        }
    }

    fn directory(&self, execution: &str) -> PathBuf {
        self.root
            .join(guest_actions::sha256_hex(execution.as_bytes()))
    }

    fn save(&self, record: &Record) -> io::Result<()> {
        let directory = self.directory(&record.execution);
        let temporary = directory.join("record.json.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec(record).expect("a record serializes"))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, directory.join("record.json"))?;
        File::open(&directory)?.sync_all()
    }
}
