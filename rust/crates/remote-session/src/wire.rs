//! What travels on the WebSocket.
//!
//! Control messages are JSON text messages tagged by `type`. Envelopes are
//! binary messages: `seq` and `ack` as two big-endian `u64`, then the opaque
//! body. Keeping bodies binary lets them carry anything without an encoding
//! step, and keeps the 1 MiB message cap of TC-8 a plain byte count.

use serde::{Deserialize, Serialize};

use crate::state::ProtocolError;

/// The largest WebSocket message either side sends or accepts (TC-8).
pub const MAX_MESSAGE: usize = 1 << 20;

/// The bytes an envelope adds in front of its body.
pub const ENVELOPE_HEADER: usize = 16;

/// The largest body one [`Session::send`](crate::Session::send) accepts.
pub const MAX_BODY: usize = MAX_MESSAGE - ENVELOPE_HEADER;

/// The protocol version a hello carries.
pub const PROTOCOL_VERSION: u32 = 1;

/// Split a payload into bodies of at most `max` bytes, each small enough for
/// one envelope when `max <= MAX_BODY`. An empty payload gives one empty body,
/// so a caller that sends every piece always sends at least one message.
/// Reassembly is the payload layer's business, as in RS-3's binary frames.
pub fn split_body(payload: &[u8], max: usize) -> impl Iterator<Item = &[u8]> {
    let max = max.clamp(1, MAX_BODY);
    let empty = payload.is_empty().then_some(&payload[..0]);
    empty.into_iter().chain(payload.chunks(max))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Envelope {
    pub seq: u64,
    pub ack: u64,
    pub body: Vec<u8>,
}

impl Envelope {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENVELOPE_HEADER + self.body.len());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.ack.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < ENVELOPE_HEADER {
            return Err(ProtocolError::Malformed(format!(
                "envelope of {} bytes is shorter than its header",
                bytes.len()
            )));
        }
        let word = |at: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[at..at + 8]);
            u64::from_be_bytes(b)
        };
        Ok(Self {
            seq: word(0),
            ack: word(8),
            body: bytes[ENVELOPE_HEADER..].to_vec(),
        })
    }
}

/// Why a session that a client asked to resume is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    /// The server held the session and dropped it when its grace period ended.
    Expired,
    /// The server has no record of the session, for example after a restart.
    Unknown,
    /// The client could not reach the server within the grace period, so the
    /// server has certainly dropped the session; no server answered.
    GraceElapsed,
}

/// Why a server refused a hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    /// The credential was missing or wrong.
    Unauthorized,
    /// The credential is valid but the session belongs to someone else.
    Forbidden,
    /// The peer broke the protocol.
    Protocol,
    /// The hello's protocol version is not served.
    UnsupportedVersion,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub version: u32,
    #[serde(default)]
    pub session: Option<String>,
    pub received: u64,
    pub credential: String,
    #[serde(default)]
    pub attributes: serde_json::Value,
}

impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hello")
            .field("version", &self.version)
            .field("session", &self.session)
            .field("received", &self.received)
            .field("credential", &"<redacted>")
            .field("attributes", &self.attributes)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Welcome {
    pub session: String,
    pub resumed: bool,
    pub received: u64,
    pub heartbeat_ms: u64,
    pub grace_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Control {
    Hello(Hello),
    Welcome(Welcome),
    Expired {
        session: String,
        reason: ExpiryReason,
    },
    Error {
        code: RejectCode,
        message: String,
    },
    Ack {
        ack: u64,
    },
    Bye,
}

impl Control {
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("control messages always serialize")
    }

    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        serde_json::from_str(text).map_err(|e| ProtocolError::Malformed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips() {
        let e = Envelope {
            seq: u64::MAX - 1,
            ack: 7,
            body: b"opaque".to_vec(),
        };
        assert_eq!(Envelope::decode(&e.encode()).unwrap(), e);
        assert!(Envelope::decode(&[0; 15]).is_err());
    }

    #[test]
    fn control_messages_are_tagged_json() {
        let hello = Control::Hello(Hello {
            version: 1,
            session: None,
            received: 0,
            credential: "t".into(),
            attributes: serde_json::json!({"role": "agent"}),
        });
        let text = hello.to_text();
        assert!(text.contains(r#""type":"hello""#), "{text}");
        assert_eq!(Control::parse(&text).unwrap(), hello);
        assert!(!format!("{hello:?}").contains("\"t\""));
        assert_eq!(Control::parse(r#"{"type":"bye"}"#).unwrap(), Control::Bye);
        assert_eq!(
            Control::parse(r#"{"type":"expired","session":"s","reason":"unknown"}"#).unwrap(),
            Control::Expired {
                session: "s".into(),
                reason: ExpiryReason::Unknown
            }
        );
    }

    #[test]
    fn split_body_respects_the_cap() {
        let payload = vec![1u8; MAX_BODY * 2 + 3];
        let parts: Vec<_> = split_body(&payload, usize::MAX).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= MAX_BODY));
        assert_eq!(parts.concat(), payload);
        assert_eq!(split_body(&[], 10).count(), 1);
        assert_eq!(split_body(b"abcde", 2).count(), 3);
    }
}
