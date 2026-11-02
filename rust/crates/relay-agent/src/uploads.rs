//! Uploads between their call and their end (`docs/arch/relay-protocol.md`,
//! RP-15): each is a stream into a spool keyed by its execution ID, which
//! outlives the link that carried it, so a sender that lost its session
//! continues from the bytes the agent committed instead of starting again.
//!
//! An upload's record is `<state>/uploads/<SHA-256 of the ID>/upload.json`:
//! the call, where its bytes land, and the committed offset, the bytes that
//! reached the disk. A file's bytes are spooled to the hidden sibling of its
//! destination, a folder's tar stream beside the folder, and a command's
//! standard input to `input` in the record's folder. The record is written
//! after the bytes it counts were flushed, so an agent that restarts keeps
//! every byte it counts and drops whatever came after.

use std::collections::HashMap;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use guest_actions::{file_target, tree_target, ActionError, Spool};
use serde::{Deserialize, Serialize};

use crate::protocol::Op;
use crate::store::{now_ms, Store};

/// How long an upload nobody continues keeps its spool.
pub const UPLOAD_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// How often an upload's bytes are flushed to the disk and its record
/// written, besides when its link ends and at its end.
pub const SYNC_STEP: u64 = 64 << 20;
/// The name the agent's spools carry beside a destination.
const TAG: &str = "relay-agent";

/// Where an upload's bytes go at its commit point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Landing {
    /// A file, renamed from its spool into place.
    File {
        /// The file's path.
        destination: PathBuf,
        /// The spool, a hidden sibling of the destination.
        partial: PathBuf,
        /// The file's mode.
        mode: Option<u32>,
    },
    /// A folder, unpacked from its spooled tar stream and put in place.
    Tree {
        /// The folder's path.
        destination: PathBuf,
        /// The spooled tar stream.
        partial: PathBuf,
        /// Where the stream is unpacked before it is put in place.
        staged: PathBuf,
    },
    /// A command's standard input.
    Input {
        /// The spool.
        partial: PathBuf,
    },
}

impl Landing {
    /// Where an upload of `op` lands, keyed by the upload's `key`.
    pub fn of(op: &Op, home: &Path, directory: &Path, key: &str) -> Result<Landing, ActionError> {
        let key = &key[..16];
        match op {
            Op::Write { path, name, mode } => {
                let target = file_target(path, name, home, TAG, key)?;
                Ok(Landing::File {
                    destination: target.destination,
                    partial: target.partial,
                    mode: *mode,
                })
            }
            Op::WriteTree { path, name } => {
                let target = tree_target(path, name, home, TAG, key)?;
                Ok(Landing::Tree {
                    destination: target.destination,
                    partial: target.partial,
                    staged: target.staged,
                })
            }
            _ => Ok(Landing::Input {
                partial: directory.join("input"),
            }),
        }
    }

    /// The spool's path.
    pub fn partial(&self) -> &Path {
        match self {
            Landing::File { partial, .. }
            | Landing::Tree { partial, .. }
            | Landing::Input { partial } => partial,
        }
    }
}

/// What an upload's record holds on the disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Meta {
    execution: String,
    op: Op,
    lease: Option<String>,
    landing: Landing,
    committed: u64,
    updated_ms: u64,
}

/// One upload the agent holds.
pub struct Upload {
    /// The client's execution ID.
    pub execution: String,
    /// The operation the upload belongs to.
    pub op: Op,
    /// The lease of the call that started it.
    pub lease: Option<String>,
    /// Where its bytes land.
    pub landing: Landing,
    /// Its spool, once opened in this process; an upload an earlier
    /// process left is reopened when a call continues it.
    pub spool: Option<Spool>,
    /// Its committed offset while its spool is not open.
    pub committed: u64,
    /// The link and call that feed it, while one does.
    pub owner: Option<(u64, u64)>,
    /// The bytes written since the last flush to the disk.
    pub unsynced: u64,
    /// When a byte last arrived, in Unix milliseconds.
    pub updated_ms: u64,
    directory: PathBuf,
}

impl Upload {
    /// Start an upload of `op` with an empty spool, and write its record.
    pub fn create(
        root: &Path,
        home: &Path,
        execution: &str,
        op: Op,
        lease: Option<String>,
    ) -> Result<Upload, ActionError> {
        let key = Store::key(execution);
        let directory = root.join(&key);
        let failed = |error: io::Error| ActionError::new("record_failed", error.to_string());
        let _ = fs::remove_dir_all(&directory);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(failed)?;
        let landing = match Landing::of(&op, home, &directory, &key) {
            Ok(landing) => landing,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let spool = match Spool::create(landing.partial()) {
            Ok(spool) => spool,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let upload = Upload {
            execution: execution.to_string(),
            op,
            lease,
            landing,
            spool: Some(spool),
            committed: 0,
            owner: None,
            unsynced: 0,
            updated_ms: now_ms(),
            directory,
        };
        upload.save().map_err(failed)?;
        Ok(upload)
    }

    /// The bytes the upload holds: what a sender continues from.
    pub fn held(&self) -> u64 {
        self.spool.as_ref().map_or(self.committed, Spool::written)
    }

    /// Open the spool of an upload an earlier process left, keeping its
    /// committed bytes. A spool that lost them starts again, empty.
    pub fn open_spool(&mut self) -> Result<(), ActionError> {
        if self.spool.is_some() {
            return Ok(());
        }
        let spool = match Spool::reopen(self.landing.partial(), self.committed) {
            Ok(spool) => spool,
            Err(_) => Spool::create(self.landing.partial())?,
        };
        self.committed = spool.committed();
        self.spool = Some(spool);
        Ok(())
    }

    /// Append the next bytes; flush them and the record every
    /// [`SYNC_STEP`].
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), ActionError> {
        let spool = self.spool.as_mut().expect("an open spool");
        spool.append(bytes)?;
        self.updated_ms = now_ms();
        self.unsynced += bytes.len() as u64;
        if self.unsynced >= SYNC_STEP {
            self.sync()?;
        }
        Ok(())
    }

    /// Flush the spool and write the record with its committed offset.
    pub fn sync(&mut self) -> Result<(), ActionError> {
        if let Some(spool) = self.spool.as_mut() {
            self.committed = spool.sync()?;
        }
        self.unsynced = 0;
        self.save()
            .map_err(|error| ActionError::new("record_failed", error.to_string()))
    }

    /// Remove the spool, whatever was unpacked of it, and the record.
    pub fn discard(mut self) {
        match self.spool.take() {
            Some(spool) => spool.discard(),
            None => {
                let _ = fs::remove_file(self.landing.partial());
            }
        }
        if let Landing::Tree { staged, .. } = &self.landing {
            let _ = fs::remove_dir_all(staged);
        }
        self.forget();
    }

    /// Remove the record only: the upload reached its commit point, which
    /// now owns its bytes.
    pub fn forget(&self) {
        let _ = fs::remove_dir_all(&self.directory);
    }

    /// The record is written through a temporary file and a rename, after
    /// the bytes it counts were flushed. It is not flushed itself: a record
    /// the disk lost names fewer bytes, and the sender sends them again.
    fn save(&self) -> io::Result<()> {
        let meta = Meta {
            execution: self.execution.clone(),
            op: self.op.clone(),
            lease: self.lease.clone(),
            landing: self.landing.clone(),
            committed: self.committed,
            updated_ms: self.updated_ms,
        };
        let temporary = self.directory.join("upload.json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&meta).expect("an upload record serializes"),
        )?;
        fs::rename(&temporary, self.directory.join("upload.json"))
    }
}

/// The uploads under `<state>/uploads`.
pub type Uploads = HashMap<String, Arc<Mutex<Upload>>>;

/// The directory of the upload records under a state directory.
pub fn root(state: &Path) -> PathBuf {
    state.join("uploads")
}

/// Open the upload records an earlier process left. An upload whose
/// execution already has a record passed its commit point and is removed,
/// as is a record that cannot be read.
pub fn load(root: &Path, store: &Store) -> io::Result<Uploads> {
    DirBuilder::new().recursive(true).mode(0o700).create(root)?;
    let mut uploads = Uploads::new();
    for entry in fs::read_dir(root)? {
        let directory = entry?.path();
        let meta = fs::read(directory.join("upload.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok())
            .filter(|meta| directory.file_name() == Some(Store::key(&meta.execution).as_ref()));
        let Some(meta) = meta else {
            let _ = fs::remove_dir_all(&directory);
            continue;
        };
        let upload = Upload {
            execution: meta.execution.clone(),
            op: meta.op,
            lease: meta.lease,
            landing: meta.landing,
            spool: None,
            committed: meta.committed,
            owner: None,
            unsynced: 0,
            updated_ms: meta.updated_ms,
            directory,
        };
        if store.get(&meta.execution).is_some() {
            upload.discard();
            continue;
        }
        uploads.insert(meta.execution, Arc::new(Mutex::new(upload)));
    }
    Ok(uploads)
}
