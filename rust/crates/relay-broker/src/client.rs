//! One client session (RP-4 to RP-8): take its calls and queries, collect
//! each upload and check it against its end, then hand the call to the
//! lease's execution record and stream the record's events back under the
//! client's call number.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use relay_agent::agent::MAX_EXECUTION_ID;
use relay_agent::protocol::{Call, CallResult, Message, Op, Outcome, Pending, Ready, PROTOCOL};
use remote_session::Session;

use crate::credentials::Target;
use crate::job::JobEvent;
use crate::Inner;

struct Upload {
    execution: String,
    op: Op,
    bytes: Vec<u8>,
    too_large: bool,
    /// Refused when it came; its chunks and end are taken and dropped.
    refused: bool,
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

/// Serve one client session until it ends.
pub(crate) fn serve(inner: Arc<Inner>, session: Session, target: Target) {
    let Some(id) = inner.add_client(&target, &session) else {
        session.close();
        return;
    };
    send(
        &session,
        &Message::Ready(Ready {
            protocol: PROTOCOL,
            lease: target.name().to_string(),
            agent: inner.agent_info_for(&target),
        }),
    );
    let open: Open = Arc::default();
    let mut uploads: HashMap<u64, Upload> = HashMap::new();
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
            }) => {
                let in_use = call == 0
                    || uploads.contains_key(&call)
                    || open
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&call);
                if in_use {
                    refuse(
                        &session,
                        call,
                        &execution,
                        "bad_request",
                        "the call number is in use",
                    );
                    continue;
                }
                if let Op::Purge { .. } = op {
                    // Only machine-service purges a lease's records, at
                    // release (DC-173).
                    refuse(
                        &session,
                        call,
                        &execution,
                        "forbidden",
                        "purge is machine-service's own operation",
                    );
                    continue;
                }
                if execution.is_empty() || execution.len() > MAX_EXECUTION_ID {
                    let message = format!("an execution ID has 1 to {MAX_EXECUTION_ID} bytes");
                    if op.kind().uploads() {
                        uploads.insert(
                            call,
                            Upload {
                                execution: execution.clone(),
                                op,
                                bytes: Vec::new(),
                                too_large: false,
                                refused: true,
                            },
                        );
                    }
                    refuse(&session, call, &execution, "bad_request", &message);
                    continue;
                }
                if op.kind().uploads() {
                    uploads.insert(
                        call,
                        Upload {
                            execution,
                            op,
                            bytes: Vec::new(),
                            too_large: false,
                            refused: false,
                        },
                    );
                } else {
                    start(
                        &inner,
                        &session,
                        &open,
                        &target,
                        call,
                        execution,
                        op,
                        Vec::new(),
                    );
                }
            }
            Message::Chunk { call, data } => {
                let Some(upload) = uploads.get_mut(&call) else {
                    protocol_error(format!("a chunk for call {call}, which is not open"));
                    break;
                };
                if upload.too_large || upload.refused {
                    continue;
                }
                if upload.bytes.len() + data.len() > inner.config.max_upload {
                    upload.too_large = true;
                    upload.bytes = Vec::new();
                } else {
                    upload.bytes.extend_from_slice(&data);
                }
            }
            Message::End {
                call,
                execution,
                size,
                sha256,
            } => {
                let Some(upload) = uploads.remove(&call) else {
                    protocol_error(format!("an end for call {call}, which is not open"));
                    break;
                };
                if upload.refused {
                    continue;
                }
                if upload.too_large {
                    refuse(
                        &session,
                        call,
                        &upload.execution,
                        "too_large",
                        "the upload exceeds 256 MiB",
                    );
                } else if execution != upload.execution {
                    refuse(
                        &session,
                        call,
                        &upload.execution,
                        "bad_request",
                        "the end names another execution ID",
                    );
                } else if size != upload.bytes.len() as u64
                    || sha256 != guest_actions::sha256_hex(&upload.bytes)
                {
                    refuse(
                        &session,
                        call,
                        &upload.execution,
                        "checksum_mismatch",
                        "the upload does not match its end",
                    );
                } else {
                    start(
                        &inner,
                        &session,
                        &open,
                        &target,
                        call,
                        upload.execution,
                        upload.op,
                        upload.bytes,
                    );
                }
            }
            Message::Query { call, execution } => {
                if call == 0 || !open.lock().unwrap_or_else(|e| e.into_inner()).insert(call) {
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
                    open.lock().unwrap_or_else(|e| e.into_inner()).remove(&call);
                });
            }
            Message::Heartbeat => {}
            other => {
                protocol_error(format!("a client sent {other:?}"));
                break;
            }
        }
    }
    inner.remove_client(&target, id);
}

#[allow(clippy::too_many_arguments)]
fn start(
    inner: &Arc<Inner>,
    session: &Session,
    open: &Open,
    target: &Target,
    call: u64,
    execution: String,
    op: Op,
    upload: Vec<u8>,
) {
    open.lock().unwrap_or_else(|e| e.into_inner()).insert(call);
    let events = inner.submit(target, &execution, op, upload);
    let session = session.clone();
    let open = Arc::clone(open);
    std::thread::spawn(move || {
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
                JobEvent::Chunk(data) => Message::Chunk { call, data },
                JobEvent::End(end) => Message::End {
                    call,
                    execution: execution.clone(),
                    size: end.size,
                    sha256: end.sha256,
                },
                JobEvent::Done { replayed, outcome } => {
                    open.lock().unwrap_or_else(|e| e.into_inner()).remove(&call);
                    send(
                        &session,
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
            if !send(&session, &message) {
                break;
            }
        }
        open.lock().unwrap_or_else(|e| e.into_inner()).remove(&call);
    });
}
