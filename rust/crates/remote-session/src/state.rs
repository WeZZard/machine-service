//! The sans-IO half of RS-2: sequence numbers, acknowledgements, the replay
//! buffer, duplicate drop and gap detection. Both sides of a session run one
//! [`SeqState`]; the connection loop and the TL-1 simulation drive it alike.

use std::collections::VecDeque;

use crate::wire::Envelope;

/// A broken rule of the protocol. It closes the connection that carried it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("gap in sequence: expected {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("acknowledgement {ack} is beyond the last sent sequence {last}")]
    AckBeyond { ack: u64, last: u64 },
    #[error("peer reports {received} received, below its earlier acknowledgement {acked}")]
    WentBackwards { received: u64, acked: u64 },
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("unexpected message: {0}")]
    Unexpected(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Accepted {
    Deliver(Vec<u8>),
    Duplicate,
}

#[derive(Debug, Default)]
pub(crate) struct SeqState {
    /// The highest sequence number this side assigned.
    last_seq: u64,
    /// Bodies the peer has not acknowledged, in order, starting at `acked + 1`.
    outbox: VecDeque<Vec<u8>>,
    outbox_bytes: usize,
    /// The highest sequence number the peer acknowledged.
    acked: u64,
    /// The highest sequence number received without a gap.
    received: u64,
    /// The highest sequence number written on the current connection.
    cursor: u64,
    /// The `received` value last told to the peer on the current connection.
    ack_sent: u64,
    /// Bytes of the bodies written on the current connection and not yet
    /// acknowledged: those from `acked + 1` to `cursor`.
    in_flight: usize,
    /// Bytes of every body the peer acknowledged, over the session.
    acked_bytes: u64,
}

impl SeqState {
    pub fn push(&mut self, body: Vec<u8>) -> u64 {
        self.last_seq += 1;
        self.outbox_bytes += body.len();
        self.outbox.push_back(body);
        self.last_seq
    }

    pub fn accept(&mut self, envelope: Envelope) -> Result<Accepted, ProtocolError> {
        self.ack(envelope.ack)?;
        let expected = self.received + 1;
        match envelope.seq {
            seq if seq < expected => Ok(Accepted::Duplicate),
            seq if seq == expected => {
                self.received = seq;
                Ok(Accepted::Deliver(envelope.body))
            }
            got => Err(ProtocolError::Gap { expected, got }),
        }
    }

    /// Drop every buffered body up to `ack`. A stale, lower `ack` is ignored.
    pub fn ack(&mut self, ack: u64) -> Result<(), ProtocolError> {
        if ack > self.last_seq {
            return Err(ProtocolError::AckBeyond {
                ack,
                last: self.last_seq,
            });
        }
        while self.acked < ack {
            let body = self
                .outbox
                .pop_front()
                .expect("outbox covers acked..last_seq");
            self.outbox_bytes -= body.len();
            if self.acked < self.cursor {
                self.in_flight -= body.len();
            }
            self.acked_bytes += body.len() as u64;
            self.acked += 1;
        }
        Ok(())
    }

    /// Start a new connection on which the peer reports `peer_received`:
    /// everything above it is resent, everything up to it is dropped.
    pub fn resume(&mut self, peer_received: u64) -> Result<(), ProtocolError> {
        if peer_received < self.acked {
            return Err(ProtocolError::WentBackwards {
                received: peer_received,
                acked: self.acked,
            });
        }
        self.ack(peer_received)?;
        self.cursor = peer_received;
        self.in_flight = 0;
        self.ack_sent = 0;
        Ok(())
    }

    /// The next envelope to write on the current connection, if any.
    #[cfg(test)]
    pub fn next_to_send(&mut self) -> Option<Envelope> {
        self.next_within(usize::MAX)
    }

    /// The next envelope to write on the current connection, if the bytes in
    /// flight stay within `window`. With nothing in flight one body always
    /// goes, so a body larger than the window still moves.
    pub fn next_within(&mut self, window: usize) -> Option<Envelope> {
        if self.cursor < self.acked {
            self.cursor = self.acked;
            self.in_flight = 0;
        }
        let index = usize::try_from(self.cursor - self.acked).ok()?;
        let body = self.outbox.get(index)?.clone();
        if self.in_flight > 0 && self.in_flight + body.len() > window {
            return None;
        }
        self.in_flight += body.len();
        self.cursor += 1;
        self.ack_sent = self.received;
        Some(Envelope {
            seq: self.cursor,
            ack: self.received,
            body,
        })
    }

    /// Forget that `seq` and everything after it were written, because the
    /// connection refused it; they go out again.
    pub fn rewind(&mut self, seq: u64) {
        while self.cursor >= seq && self.cursor > self.acked {
            let index = usize::try_from(self.cursor - self.acked - 1).expect("in the outbox");
            self.in_flight -= self.outbox[index].len();
            self.cursor -= 1;
        }
    }

    pub fn needs_ack(&self) -> bool {
        self.received > self.ack_sent
    }

    pub fn mark_ack_sent(&mut self) -> u64 {
        self.ack_sent = self.received;
        self.received
    }

    /// The highest sequence number the peer acknowledged.
    pub fn acked(&self) -> u64 {
        self.acked
    }

    /// Whether bodies written on the current connection wait for an
    /// acknowledgement.
    pub fn awaiting_ack(&self) -> bool {
        self.cursor > self.acked
    }

    /// Bytes written on the current connection and not yet acknowledged.
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Bytes the peer acknowledged over the session.
    pub fn acked_bytes(&self) -> u64 {
        self.acked_bytes
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn outbox_bytes(&self) -> usize {
        self.outbox_bytes
    }

    pub fn unacked(&self) -> usize {
        self.outbox.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(seq: u64, ack: u64, body: &[u8]) -> Envelope {
        Envelope {
            seq,
            ack,
            body: body.to_vec(),
        }
    }

    #[test]
    fn delivers_in_order_drops_duplicates_and_refuses_gaps() {
        let mut s = SeqState::default();
        assert_eq!(
            s.accept(env(1, 0, b"a")).unwrap(),
            Accepted::Deliver(b"a".to_vec())
        );
        assert_eq!(s.accept(env(1, 0, b"a")).unwrap(), Accepted::Duplicate);
        assert_eq!(
            s.accept(env(3, 0, b"c")),
            Err(ProtocolError::Gap {
                expected: 2,
                got: 3
            })
        );
        assert_eq!(s.received(), 1);
    }

    #[test]
    fn replays_what_the_peer_has_not_received() {
        let mut s = SeqState::default();
        for b in [b"a", b"b", b"c"] {
            s.push(b.to_vec());
        }
        s.resume(0).unwrap();
        assert_eq!(s.next_to_send().unwrap().seq, 1);
        assert_eq!(s.next_to_send().unwrap().seq, 2);
        // The connection drops; the peer had received only 1.
        s.resume(1).unwrap();
        assert_eq!(s.unacked(), 2);
        let again: Vec<u64> = std::iter::from_fn(|| s.next_to_send())
            .map(|e| e.seq)
            .collect();
        assert_eq!(again, [2, 3]);
        s.ack(3).unwrap();
        assert_eq!((s.unacked(), s.outbox_bytes()), (0, 0));
        assert!(s.ack(4).is_err());
        assert!(s.resume(2).is_err());
    }

    #[test]
    fn the_window_bounds_the_bytes_in_flight() {
        let mut s = SeqState::default();
        for _ in 0..5 {
            s.push(vec![0; 10]);
        }
        let sent: Vec<u64> = std::iter::from_fn(|| s.next_within(25))
            .map(|e| e.seq)
            .collect();
        assert_eq!((sent, s.in_flight()), (vec![1, 2], 20));
        s.ack(1).unwrap();
        assert_eq!(s.in_flight(), 10);
        assert_eq!(s.next_within(25).unwrap().seq, 3);
        assert!(s.next_within(25).is_none());
        s.rewind(3);
        assert_eq!(s.in_flight(), 10);
        // A body above the window goes alone.
        s.ack(2).unwrap();
        assert_eq!((s.in_flight(), s.acked_bytes()), (0, 20));
        assert_eq!(s.next_within(5).unwrap().seq, 3);
        assert!(s.next_within(5).is_none());
        s.resume(2).unwrap();
        assert_eq!(s.in_flight(), 0);
    }

    #[test]
    fn rewind_resends_refused_writes() {
        let mut s = SeqState::default();
        s.push(b"a".to_vec());
        s.push(b"b".to_vec());
        s.next_to_send();
        s.next_to_send();
        s.rewind(2);
        assert_eq!(s.next_to_send().unwrap().seq, 2);
        assert!(s.next_to_send().is_none());
    }
}
