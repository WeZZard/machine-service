//! TL-2: the broker's memory is bounded whatever a stream's size
//! (`docs/arch/relay-protocol.md` RP-15). A 3 GB synthetic stream is
//! uploaded to a relay agent and read back through a broker whose sessions
//! hold at most 8 MiB unacknowledged, and the broker never holds more than
//! about one window of it, nor does this process's memory grow toward the
//! stream's size. The size comes from `RELAY_BOUNDED_MIB` (3072 by default).
//!
//! The test is its own binary, so the process's peak memory is this test's
//! alone.

mod common;

use std::time::{Duration, Instant};

use common::*;
use relay_agent::protocol::{Op, Outcome, MAX_CHUNK, UPLOAD_WINDOW};
use sha2::{Digest as _, Sha256};

const BLOCK: usize = 1 << 20;

/// A synthetic stream: pseudo-random 1 MiB blocks, each stamped with its
/// number, generated at any offset without holding the stream.
struct Synthetic {
    size: u64,
    base: Vec<u8>,
    sha256: String,
}

impl Synthetic {
    fn new(size: u64) -> Synthetic {
        let mut state = 0x9e37_79b9_u32;
        let base = (0..BLOCK)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let mut synthetic = Synthetic {
            size,
            base,
            sha256: String::new(),
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; BLOCK];
        let mut at = 0;
        while at < size {
            let count = (size - at).min(BLOCK as u64) as usize;
            synthetic.read(at, &mut buffer[..count]);
            hasher.update(&buffer[..count]);
            at += count as u64;
        }
        synthetic.sha256 = guest_actions::hex(&hasher.finalize());
        synthetic
    }
}

impl Source for Synthetic {
    fn size(&self) -> u64 {
        self.size
    }

    fn sha256(&self) -> String {
        self.sha256.clone()
    }

    fn read(&self, offset: u64, buffer: &mut [u8]) {
        let mut at = offset;
        let mut filled = 0;
        while filled < buffer.len() {
            let block = at / BLOCK as u64;
            let within = (at % BLOCK as u64) as usize;
            let count = (BLOCK - within).min(buffer.len() - filled);
            buffer[filled..filled + count].copy_from_slice(&self.base[within..within + count]);
            // The block's number in its first bytes.
            for (index, byte) in block.to_be_bytes().iter().enumerate() {
                if (within..within + count).contains(&index) {
                    buffer[filled + index - within] = *byte;
                }
            }
            filled += count;
            at += count as u64;
        }
    }
}

/// The most memory this process held at once, in bytes.
fn peak_memory() -> u64 {
    // SAFETY: `getrusage` fills the structure it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let peak = usage.ru_maxrss as u64;
    // macOS reports bytes, Linux kibibytes.
    if cfg!(target_os = "macos") {
        peak
    } else {
        peak * 1024
    }
}

#[test]
fn a_three_gigabyte_stream_passes_a_capped_broker_in_bounded_memory() {
    let mib: u64 = std::env::var("RELAY_BOUNDED_MIB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3072);
    let size = mib << 20;
    let started = Instant::now();
    let source = Synthetic::new(size);
    let h = Harness::start(Options {
        max_buffered: Some(8 << 20),
        ..Options::default()
    });
    let a = lease(&h.broker, "vm-a");
    let agent = TestAgent::start(&h.url("ws"), &a.agent, None);
    let client = TestClient::connect_with(client_config_v2(&h.url("ws"), &a.client)).unwrap();
    let wait = Duration::from_secs(3600);

    let streamed = client.stream(
        1,
        "big-up",
        Op::Write {
            path: "big.bin".into(),
            name: String::new(),
            mode: None,
        },
        &source,
        false,
        None,
    );
    assert_eq!(streamed, Streamed::Sent { from: 0 });
    let result = client.wait(1, wait).result.unwrap();
    assert!(
        matches!(&result.outcome, Outcome::Written(d) if d.size == size && d.sha256 == source.sha256),
        "{:?}",
        result.outcome
    );
    let uploaded = started.elapsed();

    client.hash_only(2);
    client.call(
        2,
        "big-down",
        Op::Read {
            path: "big.bin".into(),
        },
        b"",
    );
    let answer = client.wait(2, wait);
    let (received, sha256) = client.hashed(2);
    assert_eq!(received, size);
    assert_eq!(sha256, source.sha256);
    assert_eq!(answer.end.unwrap().sha256, source.sha256);
    let _ = std::fs::remove_file(agent.home.join("big.bin"));

    let held = h.broker.held_most();
    let peak = peak_memory();
    eprintln!(
        "{mib} MiB up and down in {:?} (upload {uploaded:?}); the broker held at most {held} bytes; \
the process peaked at {} MiB",
        started.elapsed(),
        peak >> 20
    );
    assert!(
        held <= UPLOAD_WINDOW + 2 * MAX_CHUNK as u64,
        "the broker held {held} bytes of a {size}-byte stream"
    );
    // The client, the broker and the agent share this process: each holds
    // about a window and a session buffer, never the stream.
    assert!(
        peak < 768 << 20,
        "the process peaked at {} MiB for a {mib} MiB stream",
        peak >> 20
    );
}
