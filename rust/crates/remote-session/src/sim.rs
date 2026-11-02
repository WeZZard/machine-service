//! TL-1: a randomized simulation of the session state machine. Both sides run
//! the real [`SeqState`], the server keeps its sessions in the real
//! [`SessionTable`] on a simulated clock, and the simulated network drops,
//! duplicates and delays frames, cuts connections at random points and
//! reconnects after random pauses, some longer than the grace period.
//!
//! The property: every body is delivered exactly once and in order, or the
//! session is reported expired, and then what was delivered is an in-order
//! prefix of what was sent, with no duplicate.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

use crate::state::{Accepted, SeqState};
use crate::table::{Resolve, SessionTable};
use crate::wire::{Envelope, ExpiryReason};

#[derive(Debug, Clone)]
enum Frame {
    Env(Envelope),
    Ack(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    ToServer,
    ToClient,
}

struct Side {
    state: SeqState,
    sent: u64,
    delivered: u64,
    /// The acknowledgement last seen to advance, and when.
    progress: (u64, Instant),
}

impl Side {
    fn new() -> Self {
        Self {
            state: SeqState::default(),
            sent: 0,
            delivered: 0,
            progress: (0, Instant::now()),
        }
    }
}

#[derive(Default, Debug)]
struct Stats {
    cuts: u64,
    gaps: u64,
    duplicates: u64,
    resumes: u64,
    expiries: u64,
    delivered: u64,
}

const GRACE: Duration = Duration::from_millis(1000);
const TICK: Duration = Duration::from_millis(10);
const QUOTA: u64 = 150;
const STALL: Duration = Duration::from_millis(1500);

/// The body a side sends as its `n`th message of session `session`.
fn body(session: u64, dir: Dir, n: u64) -> Vec<u8> {
    format!("{session}/{dir:?}/{n}").into_bytes()
}

struct Sim {
    rng: StdRng,
    now: Instant,
    table: SessionTable<usize>,
    servers: Vec<Side>,
    client: Side,
    session: u64,
    session_id: String,
    connected: bool,
    reconnect_at: Instant,
    queues: [VecDeque<(Instant, Frame)>; 2],
    held: Vec<(Instant, Dir, Frame)>,
    faults: bool,
    stats: Stats,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let now = Instant::now();
        let mut sim = Self {
            rng: StdRng::seed_from_u64(seed),
            now,
            table: SessionTable::new(GRACE),
            servers: Vec::new(),
            client: Side::new(),
            session: 0,
            session_id: String::new(),
            connected: false,
            reconnect_at: now,
            queues: [VecDeque::new(), VecDeque::new()],
            held: Vec::new(),
            faults: true,
            stats: Stats::default(),
        };
        sim.new_session();
        sim
    }

    fn new_session(&mut self) {
        self.session += 1;
        self.session_id = format!("s{}", self.session);
        self.servers.push(Side::new());
        self.client = Side::new();
        self.table
            .insert(self.session_id.clone(), "p".into(), self.servers.len() - 1);
        self.client.state.resume(0).unwrap();
        self.server().state.resume(0).unwrap();
        self.connected = true;
        self.queues.iter_mut().for_each(VecDeque::clear);
        // Frames of the old session never reach the new one.
        self.held.clear();
    }

    /// Queue the next body from the side that sends in direction `dir`.
    fn send_one(&mut self, dir: Dir) {
        let session = self.session;
        let sender = match dir {
            Dir::ToServer => &mut self.client,
            Dir::ToClient => self.servers.last_mut().expect("a session"),
        };
        if sender.sent < QUOTA {
            sender.sent += 1;
            let n = sender.sent;
            sender.state.push(body(session, dir, n));
        }
    }

    fn server(&mut self) -> &mut Side {
        self.servers.last_mut().expect("a session")
    }

    fn side(&mut self, dir: Dir) -> &mut Side {
        match dir {
            Dir::ToServer => self.server(),
            Dir::ToClient => &mut self.client,
        }
    }

    fn cut(&mut self) {
        self.connected = false;
        self.stats.cuts += 1;
        self.queues.iter_mut().for_each(VecDeque::clear);
        self.table.detach(&self.session_id, self.now);
        let pause = if self.rng.random_bool(0.1) { 2500 } else { 400 };
        self.reconnect_at = self.now + Duration::from_millis(self.rng.random_range(0..pause));
    }

    fn enqueue(&mut self, dir: Dir, frame: Frame) {
        let latency = Duration::from_millis(self.rng.random_range(0..60));
        let at = self.now + latency;
        let q = &mut self.queues[dir as usize];
        if self.faults {
            let roll: f64 = self.rng.random();
            if roll < 0.01 {
                return; // dropped
            }
            if roll < 0.02 {
                q.push_back((at, frame.clone())); // duplicated
            } else if roll < 0.03 {
                let late = self.now + Duration::from_millis(self.rng.random_range(0..800));
                self.held.push((late, dir, frame));
                return;
            }
        }
        // A connection keeps its order: a frame is never due before the one ahead of it.
        let at = q.back().map_or(at, |(prev, _)| at.max(*prev));
        q.push_back((at, frame));
    }

    /// The connection loop's stall rule: acknowledgements that stop while
    /// bodies are outstanding end the connection.
    fn stalled(&mut self) -> bool {
        let now = self.now;
        let client = &mut self.client;
        let server = self.servers.last_mut().expect("a session");
        [client, server].into_iter().any(|side| {
            let acked = side.state.acked();
            if acked != side.progress.0 || !side.state.awaiting_ack() {
                side.progress = (acked, now);
                false
            } else {
                now.saturating_duration_since(side.progress.1) >= STALL
            }
        })
    }

    /// Hand `frame` to the receiving side. `false` means a protocol error.
    fn receive(&mut self, dir: Dir, frame: Frame) -> bool {
        let session = self.session;
        let receiver = self.side(dir);
        match frame {
            Frame::Ack(ack) => receiver.state.ack(ack).is_ok(),
            Frame::Env(env) => match receiver.state.accept(env) {
                Ok(Accepted::Deliver(got)) => {
                    let expected = body(session, dir, receiver.delivered + 1);
                    assert_eq!(got, expected, "out of order or duplicated delivery");
                    receiver.delivered += 1;
                    self.stats.delivered += 1;
                    true
                }
                Ok(Accepted::Duplicate) => {
                    self.stats.duplicates += 1;
                    true
                }
                Err(_) => {
                    self.stats.gaps += 1;
                    false
                }
            },
        }
    }

    fn reconnect(&mut self) {
        let now = self.now;
        self.client.progress.1 = now;
        self.server().progress.1 = now;
        // The server's side of an expired session is gone with it.
        self.table.expire_due(self.now);
        let received = self.client.state.received();
        match self.table.resume(&self.session_id, "p") {
            Resolve::Resumed(index) => {
                let server = &mut self.servers[index];
                server
                    .state
                    .resume(received)
                    .expect("a live session never goes backwards");
                let welcome_received = server.state.received();
                self.client
                    .state
                    .resume(welcome_received)
                    .expect("a live session never goes backwards");
                self.connected = true;
                self.stats.resumes += 1;
            }
            Resolve::Gone(reason) => {
                assert_eq!(reason, ExpiryReason::Expired);
                self.check_prefix();
                self.stats.expiries += 1;
                self.new_session();
            }
            Resolve::Forbidden => unreachable!("one principal"),
        }
    }

    /// What was delivered is a prefix of what was sent; the per-delivery
    /// assertion already proved order and uniqueness.
    fn check_prefix(&mut self) {
        let (delivered, sent) = (self.client.delivered, self.client.sent);
        let server = self.server();
        assert!(server.delivered <= sent);
        assert!(delivered <= server.sent);
    }

    fn step(&mut self) {
        self.now += TICK;
        // The server's reaper runs on the simulated clock.
        self.table.expire_due(self.now);
        for dir in [Dir::ToServer, Dir::ToClient] {
            if !self.faults || self.rng.random_bool(0.3) {
                self.send_one(dir);
            }
        }
        if !self.connected {
            if self.now >= self.reconnect_at {
                self.reconnect();
            }
            return;
        }
        if self.faults && self.rng.random_bool(0.01) || self.stalled() {
            return self.cut();
        }
        // Each side writes what it has, then an acknowledgement.
        for dir in [Dir::ToServer, Dir::ToClient] {
            let sender_state = match dir {
                Dir::ToServer => &mut self.client.state,
                Dir::ToClient => &mut self.servers.last_mut().expect("a session").state,
            };
            let frames: Vec<Frame> = std::iter::from_fn(|| sender_state.next_to_send())
                .take(8)
                .map(Frame::Env)
                .collect();
            let ack = (frames.is_empty() && sender_state.needs_ack())
                .then(|| sender_state.mark_ack_sent());
            for f in frames.into_iter().chain(ack.map(Frame::Ack)) {
                self.enqueue(dir, f);
            }
        }
        // Delayed frames come back on whatever connection is current.
        let now = self.now;
        let (due, kept): (Vec<_>, Vec<_>) = self.held.drain(..).partition(|(at, _, _)| *at <= now);
        self.held = kept;
        for (_, dir, frame) in due {
            self.queues[dir as usize].push_front((now, frame));
        }
        for dir in [Dir::ToServer, Dir::ToClient] {
            while self.queues[dir as usize]
                .front()
                .is_some_and(|(at, _)| *at <= now)
            {
                let (_, frame) = self.queues[dir as usize].pop_front().unwrap();
                if !self.receive(dir, frame) {
                    // A gap closes the connection; resume fills it.
                    return self.cut();
                }
            }
        }
    }

    fn settled(&self) -> bool {
        let server = self.servers.last().expect("a session");
        self.connected
            && self.client.sent == QUOTA
            && server.sent == QUOTA
            && self.client.delivered == server.sent
            && server.delivered == self.client.sent
    }
}

fn run(seed: u64) -> Stats {
    let mut sim = Sim::new(seed);
    // A fault-filled phase, then a quiet one in which everything must arrive.
    for _ in 0..3000 {
        sim.step();
    }
    sim.faults = false;
    for _ in 0..3000 {
        if sim.settled() {
            break;
        }
        sim.step();
    }
    assert!(
        sim.settled(),
        "seed {seed}: the last session did not deliver everything: connected {}, client {:?}/{}/{}, server {:?}/{}/{}, queues {:?}",
        sim.connected,
        sim.client.state, sim.client.sent, sim.client.delivered,
        sim.servers.last().unwrap().state, sim.servers.last().unwrap().sent, sim.servers.last().unwrap().delivered,
        sim.queues.iter().map(|q| q.len()).collect::<Vec<_>>()
    );
    sim.stats
}

#[test]
fn randomized_faults_deliver_exactly_once_or_expire() {
    let mut total = Stats::default();
    for seed in 0..300 {
        let s = run(seed);
        total.cuts += s.cuts;
        total.gaps += s.gaps;
        total.duplicates += s.duplicates;
        total.resumes += s.resumes;
        total.expiries += s.expiries;
        total.delivered += s.delivered;
    }
    // The faults really happened, so the property was tested under them.
    assert!(total.cuts > 1000, "{total:?}");
    assert!(total.gaps > 100, "{total:?}");
    assert!(total.duplicates > 100, "{total:?}");
    assert!(total.resumes > 1000, "{total:?}");
    assert!(total.expiries > 10, "{total:?}");
}
