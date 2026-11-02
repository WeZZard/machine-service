//! The relay protocol: the messages between a relay agent and the broker
//! (`docs/arch/relay-agent.md`, RA-5 and RA-6).
//!
//! Every message is one opaque payload on a [`Link`](crate::link::Link).
//! A [`Message::Chunk`] is binary: the byte `0x01`, the 8-byte big-endian
//! call number and up to [`MAX_CHUNK`] bytes of data. Every other message is
//! a JSON object, which never starts with `0x01`.
//!
//! A call names the client's execution ID. `exec`, `script` and `write`
//! upload their bytes as chunks and end them with [`Message::End`], which is
//! the call's commit point: nothing runs before the bytes matched it. A
//! command's output streams back as chunks and ends with its own
//! [`Message::End`] before the [`Message::Result`]; a read streams the
//! file's bytes the same way.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The version of this protocol, announced in [`Hello`].
pub const PROTOCOL: u32 = 1;
/// The largest message a link must carry (TC-8).
pub const MAX_MESSAGE: usize = 1 << 20;
/// The most data a chunk carries. It leaves room under [`MAX_MESSAGE`] for
/// the chunk's own header and for the envelope of a session that carries it.
pub const MAX_CHUNK: usize = MAX_MESSAGE - 4096;
/// The first byte of a binary chunk.
const CHUNK_TAG: u8 = 0x01;

/// A message between the relay agent and the broker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    /// Agent → broker, first on every link: who the agent is and what it
    /// had in flight.
    Hello(Hello),
    /// Either way, at least every 30 seconds (TC-3), so an idle link is not
    /// closed. It needs no answer.
    Heartbeat,
    /// Broker → agent: run one operation under an execution ID.
    Call(Call),
    /// Either way: up to [`MAX_CHUNK`] bytes of a call's stream; binary on
    /// the wire.
    #[serde(skip)]
    Chunk {
        /// The call the bytes belong to.
        call: u64,
        /// The bytes.
        data: Vec<u8>,
    },
    /// Either way: the end of a call's stream, with the size and SHA-256 of
    /// all its chunks.
    End {
        /// The call whose stream ends.
        call: u64,
        /// The call's execution ID, so an end can never complete another
        /// call's stream.
        execution: String,
        /// The number of bytes in the stream.
        size: u64,
        /// The SHA-256 of the stream, in lower-case hexadecimal.
        sha256: String,
    },
    /// Agent → broker: the outcome of a call; the call's last message.
    Result(CallResult),
    /// Broker → agent: what the agent knows of an execution ID.
    Query {
        /// A number the answer repeats.
        call: u64,
        /// The execution ID asked about.
        execution: String,
    },
    /// Agent → broker, and broker → client: the answer to a query.
    QueryResult {
        /// The query's number.
        call: u64,
        /// The execution ID asked about.
        execution: String,
        /// What the agent knows of it.
        state: ExecutionState,
    },
    /// Broker → client, first on every new client session
    /// (`docs/arch/relay-protocol.md` RP-4).
    Ready(Ready),
    /// Broker → client: a call or query waits for the lease's relay agent
    /// (RP-8).
    Pending(Pending),
    /// Broker → client: the lease's credentials were revoked; the broker
    /// closes the session next (RP-10).
    Revoked {
        /// `released` or `expired`.
        reason: String,
    },
}

/// The broker's first message to a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// The lease's machine name.
    pub lease: String,
    /// The lease's relay agent, when it has a session.
    pub agent: Option<AgentInfo>,
}

/// What a client learns of a relay agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// `macos` or `linux`.
    pub os: String,
    /// The processor architecture.
    pub arch: String,
    /// The SHA-256 of the agent's executable.
    pub build: String,
    /// The agent's protocol version.
    pub protocol: u32,
}

/// A call or query that waits for the relay agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// The client's call number.
    pub call: u64,
    /// The call's execution ID.
    pub execution: String,
    /// Why it waits: `agent_not_connected`.
    pub code: String,
    /// What it waits for, for people.
    pub message: String,
    /// How long it waits at most.
    pub wait_ms: u64,
}

/// The agent's first message on a link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// The SHA-256 of the agent's executable (RA-6).
    pub build: String,
    /// `macos` or `linux`.
    pub os: String,
    /// The processor architecture, such as `aarch64`.
    pub arch: String,
    /// How often the agent sends a heartbeat.
    pub heartbeat_ms: u64,
    /// Executions still running, and executions whose outcome no link has
    /// carried yet (RA-3).
    pub in_flight: Vec<Report>,
}

/// One execution in a [`Hello`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The execution ID.
    pub execution: String,
    /// Its state.
    pub state: ExecutionState,
}

/// One operation the broker asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    /// The number this link's chunks, end and result carry for the call.
    pub call: u64,
    /// The client's execution ID. The agent runs an execution ID once.
    pub execution: String,
    /// The operation.
    pub op: Op,
    /// The lease the call belongs to, which the agent keeps in the call's
    /// record so that [`Op::Purge`] can remove a lease's records (RA-5).
    /// The broker sets it on every call it hands on and replaces whatever a
    /// client sent; a client omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
}

/// The operations of RA-5.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Run a command line through the user's shell. Chunks with its standard
    /// input follow, possibly none, and an end.
    Exec {
        /// The command line.
        command: String,
        /// Whether the command reads the uploaded bytes on standard input;
        /// otherwise it reads `/dev/null`.
        stdin: bool,
        /// Seconds before the command's process group is killed.
        timeout_s: f64,
    },
    /// Run a script, given on standard input to `zsh -s` on macOS or
    /// `bash -s` on Linux. Chunks with the script follow, and an end.
    Script {
        /// Seconds before the script's process group is killed.
        timeout_s: f64,
    },
    /// Write a file atomically. Chunks with its bytes follow, and an end.
    Write {
        /// The destination, resolved against the home directory.
        path: String,
        /// The file name to use when `path` is a directory.
        #[serde(default)]
        name: String,
        /// The file's mode, when set.
        #[serde(default)]
        mode: Option<u32>,
    },
    /// Read a file; its bytes stream back as chunks and an end.
    Read {
        /// The file, resolved against the home directory.
        path: String,
    },
    /// Write a folder atomically. Chunks with its tar stream follow, and an
    /// end (`docs/arch/relay-protocol.md`, RP-13).
    WriteTree {
        /// The destination, resolved against the home directory.
        path: String,
        /// The folder's name to use when `path` is an existing folder.
        #[serde(default)]
        name: String,
    },
    /// Read a folder; its tar stream comes back as chunks and an end.
    ReadTree {
        /// The folder, resolved against the home directory.
        path: String,
    },
    /// Cancel another execution.
    Cancel {
        /// The execution ID to cancel.
        target: String,
    },
    /// Report the agent's state.
    Status,
    /// Restart the agent onto the executable at its path, which may hold a
    /// new build (RA-6). Refused with `busy` while any operation runs.
    Restart,
    /// Remove the records of every execution of `lease`, with the output
    /// they keep for replay (DC-173). Only the broker sends it, and the
    /// agent takes it only when the call itself belongs to `lease`. Refused
    /// with `busy` while an operation of the lease runs. It keeps no record
    /// of its own: a repeated purge finds nothing more to remove.
    Purge {
        /// The lease whose records go.
        lease: String,
    },
}

impl Op {
    /// The kind of the operation.
    pub fn kind(&self) -> OpKind {
        match self {
            Op::Exec { .. } => OpKind::Exec,
            Op::Script { .. } => OpKind::Script,
            Op::Write { .. } => OpKind::Write,
            Op::Read { .. } => OpKind::Read,
            Op::WriteTree { .. } => OpKind::WriteTree,
            Op::ReadTree { .. } => OpKind::ReadTree,
            Op::Cancel { .. } => OpKind::Cancel,
            Op::Status => OpKind::Status,
            Op::Restart => OpKind::Restart,
            Op::Purge { .. } => OpKind::Purge,
        }
    }
}

/// The kind of an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// [`Op::Exec`].
    Exec,
    /// [`Op::Script`].
    Script,
    /// [`Op::Write`].
    Write,
    /// [`Op::Read`].
    Read,
    /// [`Op::WriteTree`].
    WriteTree,
    /// [`Op::ReadTree`].
    ReadTree,
    /// [`Op::Cancel`].
    Cancel,
    /// [`Op::Status`].
    Status,
    /// [`Op::Restart`].
    Restart,
    /// [`Op::Purge`].
    Purge,
}

impl OpKind {
    /// Whether the operation's output streams back as chunks.
    pub fn streams_output(self) -> bool {
        matches!(self, OpKind::Exec | OpKind::Script)
    }

    /// Whether the call uploads chunks and an end before it commits.
    pub fn uploads(self) -> bool {
        matches!(
            self,
            OpKind::Exec | OpKind::Script | OpKind::Write | OpKind::WriteTree
        )
    }
}

/// The answer to a call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallResult {
    /// The call's number.
    pub call: u64,
    /// The call's execution ID.
    pub execution: String,
    /// True when the execution ID had run before and this is its recorded
    /// outcome; the operation did not run again.
    pub replayed: bool,
    /// The outcome.
    pub outcome: Outcome,
}

/// The size and SHA-256 of a byte stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// The number of bytes.
    pub size: u64,
    /// Their SHA-256, in lower-case hexadecimal.
    pub sha256: String,
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A command or script ended.
    Exit {
        /// The exit status, when it exited.
        rc: Option<i32>,
        /// The signal that ended it, when one did.
        signal: Option<i32>,
        /// Whether its timeout killed it.
        timed_out: bool,
        /// Whether a cancel killed it.
        cancelled: bool,
        /// The size and SHA-256 of its combined output.
        output: Digest,
    },
    /// A file was renamed into place.
    Written(Digest),
    /// A file was read whole.
    Read(Digest),
    /// A cancel was handled.
    Cancel {
        /// The execution it named.
        target: String,
        /// What the cancel found.
        found: CancelFound,
    },
    /// The agent's state.
    Status(Status),
    /// A lease's records were removed.
    Purged {
        /// The lease.
        lease: String,
        /// How many execution records were removed.
        records: u64,
        /// How many bytes of kept output went with them.
        bytes: u64,
    },
    /// The operation failed. When `committed` is false nothing ran, nothing
    /// was recorded, and the execution ID may be sent again.
    Failed {
        /// A stable code such as `checksum_mismatch` or `not_found`.
        code: String,
        /// What went wrong, for people.
        message: String,
        /// Whether the call had passed its commit point.
        committed: bool,
    },
    /// The agent stopped while the operation ran, so its effect is
    /// uncertain. It is never run again.
    Interrupted {
        /// What happened.
        message: String,
    },
}

impl Outcome {
    /// A failure before the commit point: nothing ran.
    pub fn refused(code: &str, message: impl Into<String>) -> Outcome {
        Outcome::Failed {
            code: code.to_string(),
            message: message.into(),
            committed: false,
        }
    }

    /// A failure after the commit point.
    pub fn failed(code: &str, message: impl Into<String>) -> Outcome {
        Outcome::Failed {
            code: code.to_string(),
            message: message.into(),
            committed: true,
        }
    }
}

/// What a cancel found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelFound {
    /// The target was running; its process group was killed.
    Signalled,
    /// The target had not reached its commit point; it never runs.
    Discarded,
    /// The target had already completed.
    Completed,
    /// The agent has no record of the target.
    Unknown,
}

/// The agent's state, answered by [`Op::Status`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// The SHA-256 of the agent's executable.
    pub build: String,
    /// `macos` or `linux`.
    pub os: String,
    /// The execution IDs running now.
    pub running: Vec<String>,
    /// How the operating system owns the agent's commands (DC-139):
    /// `scope`, or `process-group (<why scopes are not used>)`.
    #[serde(default)]
    pub ownership: String,
}

/// What the agent knows of an execution ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    /// It ran to an outcome.
    Completed {
        /// The operation.
        op: OpKind,
        /// Its outcome.
        outcome: Outcome,
    },
    /// It is running.
    Running {
        /// The operation.
        op: OpKind,
    },
    /// The agent has no record of it: it never passed its commit point, or
    /// its record expired.
    Unknown,
}

/// A payload that is not a message.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// A chunk shorter than its header.
    #[error("a chunk of {0} bytes is shorter than its header")]
    ShortChunk(usize),
    /// A chunk with more data than [`MAX_CHUNK`].
    #[error("a chunk of {0} bytes exceeds the limit")]
    LargeChunk(usize),
    /// A control message that is not valid JSON for a message.
    #[error("not a relay message: {0}")]
    Json(#[from] serde_json::Error),
}

impl Message {
    /// The message as a link payload.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Message::Chunk { call, data } => {
                let mut payload = Vec::with_capacity(9 + data.len());
                payload.push(CHUNK_TAG);
                payload.extend_from_slice(&call.to_be_bytes());
                payload.extend_from_slice(data);
                payload
            }
            control => serde_json::to_vec(control).expect("a control message serializes"),
        }
    }

    /// The message in a link payload.
    pub fn decode(payload: &[u8]) -> Result<Message, DecodeError> {
        match payload.first() {
            Some(&CHUNK_TAG) => {
                if payload.len() < 9 {
                    return Err(DecodeError::ShortChunk(payload.len()));
                }
                if payload.len() - 9 > MAX_CHUNK {
                    return Err(DecodeError::LargeChunk(payload.len() - 9));
                }
                let call = u64::from_be_bytes(payload[1..9].try_into().expect("8 bytes"));
                Ok(Message::Chunk {
                    call,
                    data: payload[9..].to_vec(),
                })
            }
            _ => Ok(serde_json::from_slice(payload)?),
        }
    }
}
