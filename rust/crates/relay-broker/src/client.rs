//! One client session (RP-4 to RP-8, RP-15): take its calls and queries,
//! pass each upload on to the lease's execution record as it comes, and
//! stream the record's events back under the client's call number.
//!
//! A client of protocol 2, which names it in its hello's attributes, streams
//! an upload of any size within the window beyond the agent's progress, and
//! continues it after a loss. A client of protocol 1 sends an upload whole,
//! which the broker holds, up to [`crate::BrokerConfig::max_upload`], and
//! checks against its end before it hands the call on, as it always did.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use relay_agent::agent::MAX_EXECUTION_ID;
use relay_agent::protocol::{
    Call, CallResult, Digest, Message, Op, Outcome, Pending, Ready, PROTOCOL,
};
use remote_session::Session;

use crate::credentials::Target;
use crate::job::{Events, Fed, Feed, Job, JobEvent, JobSpec, Upload};
use crate::Inner;

/// An upload between its call and its end.
enum Slot {
    /// A client of protocol 2 feeds the record as it goes; `None` once the
    /// record takes no more bytes, or when the call attached to a record
    /// that has its upload already.
    Feeding {
        execution: String,
        job: Option<Arc<Job>>,
    },
    /// A client of protocol 1: the whole upload, held until its end.
    Whole {
        execution: String,
        op: Op,
        bytes: Vec<u8>,
        too_large: bool,
    },
    /// Refused when it came; its chunks and end are taken and dropped.
    Refused,
}

type Open = Arc<Mutex<HashSet<u64>>>;

fn send(session: &Session, message: &Message) -> bool {
    session.send(message.encode()).is_ok()
}

fn refuse(session: &Session, call: u64, execution: &str, code: &str, message: &str) {
    send(
        session,
        &Message::Result(CallResult {
            call,
            execution: execution.to_string(),
            replayed: false,
            outcome: Outcome::refused(code, message),
        }),
    );
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The relay protocol a client speaks: its hello's `attributes.protocol`,
/// else 1, and never more than the broker's.
fn protocol_of(session: &Session) -> u32 {
    session
        .attributes()
        .get("protocol")
        .and_then(serde_json::Value::as_u64)
        .map_or(1, |protocol| protocol.min(u64::from(PROTOCOL)) as u32)
}

/// Serve one client session until it ends.
pub(crate) fn serve(inner: Arc<Inner>, session: Session, target: Target) {
    let Some(id) = inner.add_client(&target, &session) else {
        session.close();
        return;
    };
    let protocol = protocol_of(&session);
    send(
        &session,
        &Message::Ready(Ready {
            protocol: PROTOCOL,
            lease: target.name().to_string(),
            agent: inner.agent_info_for(&target),
        }),
    );
    let open: Open = Arc::default();
    let mut slots: HashMap<u64, Slot> = HashMap::new();
    let protocol_error = |why: String| {
        (inner.log)(&format!("relay: {target}: closing a client session: {why}"));
        session.close();
    };
    while let Ok(body) = session.recv() {
        let message = match Message::decode(&body) {
            Ok(message) => message,
            Err(error) => {
                protocol_error(error.to_string());
                break;
            }
        };
        match message {
            Message::Call(Call {
                call,
                execution,
                op,
                // The broker names the lease itself (RA-5).
                lease: _,
                resume,
                offset,
            }) => {
                // A call number whose result went out may be used again;
                // what its old upload still sends is dropped.
                if call == 0 || lock(&open).contains(&call) {
                    refuse(
                        &session,
                        call,
                        &execution,
                        "bad_request",
                        "the call number is in use",
                    );
                    continue;
                }
                slots.remove(&call);
                let uploads = op.kind().uploads();
                let refused = if let Op::Purge { .. } = op {
                    // Only machine-service purges a lease's records, at
                    // release (DC-173).
                    Some((
                        "forbidden",
                        "purge is machine-service's own operation".into(),
                    ))
                } else if execution.is_empty() || execution.len() > MAX_EXECUTION_ID {
                    Some((
                        "bad_request",
                        format!("an execution ID has 1 to {MAX_EXECUTION_ID} bytes"),
                    ))
                } else {
                    None
                };
                if let Some((code, message)) = refused {
                    if uploads {
                        slots.insert(call, Slot::Refused);
                    }
                    refuse(&session, call, &execution, code, &message);
                    continue;
                }
                let call_of = Started {
                    inner: &inner,
                    session: &session,
                    open: &open,
                    target: &target,
                    call,
                };
                if !uploads {
                    call_of.start(execution, op, Feed::Local(Upload::None), false, offset);
                } else if protocol >= 2 {
                    let job = call_of.start(execution.clone(), op, Feed::Client, resume, offset);
                    slots.insert(call, Slot::Feeding { execution, job });
                } else {
                    slots.insert(
                        call,
                        Slot::Whole {
                            execution,
                            op,
                            bytes: Vec::new(),
                            too_large: false,
                        },
                    );
                }
            }
            Message::Chunk { call, data } => match slots.get_mut(&call) {
                // A chunk of a call that ended before its stream did.
                None | Some(Slot::Refused) | Some(Slot::Feeding { job: None, .. }) => {}
                Some(Slot::Feeding { job, .. }) => {
                    let feeding = job.clone().expect("a fed record");
                    if let Fed::Closed = feeding.feed(data) {
                        inner.forget_unrun(&feeding);
                        *job = None;
                    }
                }
                Some(Slot::Whole {
                    bytes, too_large, ..
                }) => {
                    if *too_large {
                        continue;
                    }
                    if bytes.len() + data.len() > inner.config.max_upload {
                        *too_large = true;
                        *bytes = Vec::new();
                    } else {
                        bytes.extend_from_slice(&data);
                    }
                }
            },
            Message::End {
                call,
                execution,
                size,
                sha256,
            } => match slots.remove(&call) {
                None | Some(Slot::Refused) | Some(Slot::Feeding { job: None, .. }) => {}
                Some(Slot::Feeding {
                    execution: own,
                    job: Some(job),
                }) => {
                    if execution == own {
                        job.finish(Digest { size, sha256 });
                    } else {
                        job.refuse("bad_request", "the end names another execution ID");
                        inner.forget_unrun(&job);
                    }
                }
                Some(Slot::Whole {
                    execution: own,
                    op,
                    bytes,
                    too_large,
                }) => {
                    if too_large {
                        refuse(
                            &session,
                            call,
                            &own,
                            "too_large",
                            "the upload exceeds 256 MiB; a client of protocol 2 streams it",
                        );
                    } else if execution != own {
                        refuse(
                            &session,
                            call,
                            &own,
                            "bad_request",
                            "the end names another execution ID",
                        );
                    } else if size != bytes.len() as u64
                        || sha256 != guest_actions::sha256_hex(&bytes)
                    {
                        refuse(
                            &session,
                            call,
                            &own,
                            "checksum_mismatch",
                            "the upload does not match its end",
                        );
                    } else {
                        Started {
                            inner: &inner,
                            session: &session,
                            open: &open,
                            target: &target,
                            call,
                        }
                        .start(
                            own,
                            op,
                            Feed::Local(Upload::bytes(bytes)),
                            false,
                            0,
                        );
                    }
                }
            },
            Message::Query { call, execution } => {
                if call == 0 || !lock(&open).insert(call) {
                    refuse(
                        &session,
                        call,
                        &execution,
                        "bad_request",
                        "the call number is in use",
                    );
                    continue;
                }
                let inner = Arc::clone(&inner);
                let session = session.clone();
                let open = Arc::clone(&open);
                let target = target.clone();
                std::thread::spawn(move || {
                    let tell = |left: std::time::Duration| {
                        send(
                            &session,
                            &Message::Pending(Pending {
                                call,
                                execution: execution.clone(),
                                code: "agent_not_connected".into(),
                                message: format!("the relay agent of {target} has no session"),
                                wait_ms: u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
                            }),
                        );
                    };
                    let state = inner.query(&target, &execution, &tell);
                    send(
                        &session,
                        &Message::QueryResult {
                            call,
                            execution,
                            state,
                        },
                    );
                    lock(&open).remove(&call);
                });
            }
            // A client's own progress paces nothing: the broker paces the
            // agent's streams by what it passed on to the client.
            Message::Heartbeat | Message::Progress { .. } => {}
            other => {
                protocol_error(format!("a client sent {other:?}"));
                break;
            }
        }
    }
    // Uploads whose end never came cannot complete; the agent keeps what
    // it committed for the client's next session.
    for slot in slots.into_values() {
        if let Slot::Feeding { job: Some(job), .. } = slot {
            job.abandon();
            inner.forget_unrun(&job);
        }
    }
    inner.remove_client(&target, id);
}

/// A call of this session, about to start.
struct Started<'a> {
    inner: &'a Arc<Inner>,
    session: &'a Session,
    open: &'a Open,
    target: &'a Target,
    call: u64,
}

impl Started<'_> {
    /// Start or attach to the call's record, and stream its events back to
    /// the client. Returns the record when the client feeds its upload.
    fn start(
        self,
        execution: String,
        op: Op,
        feed: Feed,
        resume: bool,
        offset: u64,
    ) -> Option<Arc<Job>> {
        let call = self.call;
        lock(self.open).insert(call);
        let (events, job) = self.inner.submit(JobSpec {
            target: self.target.clone(),
            lease: String::new(),
            execution: execution.clone(),
            op,
            feed,
            resume,
            offset,
            transient: false,
        });
        let session = self.session.clone();
        let open = Arc::clone(self.open);
        std::thread::spawn(move || forward(events, &session, &open, call, execution));
        job
    }
}

fn forward(events: Events, session: &Session, open: &Open, call: u64, execution: String) {
    for event in events {
        let message = match event {
            JobEvent::Pending {
                code,
                message,
                wait_ms,
            } => Message::Pending(Pending {
                call,
                execution: execution.clone(),
                code,
                message,
                wait_ms,
            }),
            JobEvent::Progress(committed) => Message::Progress {
                call,
                execution: execution.clone(),
                committed,
            },
            JobEvent::Chunk(data) => Message::Chunk { call, data },
            JobEvent::End(end) => Message::End {
                call,
                execution: execution.clone(),
                size: end.size,
                sha256: end.sha256,
            },
            JobEvent::Done { replayed, outcome } => {
                lock(open).remove(&call);
                send(
                    session,
                    &Message::Result(CallResult {
                        call,
                        execution,
                        replayed,
                        outcome,
                    }),
                );
                return;
            }
        };
        if !send(session, &message) {
            break;
        }
    }
    lock(open).remove(&call);
}
