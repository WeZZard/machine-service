//! The relay acceptance scenarios, AC-55 to AC-70
//! (`docs/acceptance/relay.acceptance`), each under the profiles its tags
//! name.
//!
//! The order is the suite's, not the file's: the scenarios that disturb the
//! guest or the broker run late, and AC-60 releases the lease, so AC-66,
//! which checks that the released credentials are refused, runs last.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use network_rig::cell::{CellContext, Outcome as Verdict, Scenario, Stop};
use network_rig::{Fault, Select, Side, Stream};
use relay_agent::protocol::{CancelFound, ExecutionState, Op, Outcome};
use remote_session::{Error as SessionError, LinkEvent, RejectCode};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::client::{exec_op, hex, new_id, sha256_hex, Answer, RelayClient};
use crate::guest::Os;
use crate::stats::Summary;
use crate::target::RealTarget;

type Ctx<'a> = CellContext<'a, RealTarget>;

const ALL: &[&str] = &["N0", "N1", "N2", "N3"];
const FAULTED: &[&str] = &["N1", "N2", "N3"];
const SPEED: &[&str] = &["N0", "N2"];

/// The suite, in the order it runs.
pub fn suite() -> Vec<Scenario<RealTarget>> {
    vec![
        Scenario {
            id: "AC-68",
            profiles: ALL,
            run: ac68,
        },
        Scenario {
            id: "AC-55",
            profiles: ALL,
            run: ac55,
        },
        Scenario {
            id: "AC-56",
            profiles: ALL,
            run: ac56,
        },
        Scenario {
            id: "AC-57",
            profiles: ALL,
            run: ac57,
        },
        Scenario {
            id: "AC-58",
            profiles: ALL,
            run: ac58,
        },
        Scenario {
            id: "AC-61",
            profiles: FAULTED,
            run: ac61,
        },
        Scenario {
            id: "AC-62",
            profiles: FAULTED,
            run: ac62,
        },
        Scenario {
            id: "AC-63",
            profiles: FAULTED,
            run: ac63,
        },
        Scenario {
            id: "AC-67",
            profiles: FAULTED,
            run: ac67,
        },
        Scenario {
            id: "AC-69",
            profiles: FAULTED,
            run: ac69,
        },
        Scenario {
            id: "AC-70",
            profiles: SPEED,
            run: ac70,
        },
        Scenario {
            id: "AC-59",
            profiles: ALL,
            run: ac59,
        },
        Scenario {
            id: "AC-64",
            profiles: FAULTED,
            run: ac64,
        },
        Scenario {
            id: "AC-65",
            profiles: FAULTED,
            run: ac65,
        },
        Scenario {
            id: "AC-60",
            profiles: ALL,
            run: ac60,
        },
        Scenario {
            id: "AC-66",
            profiles: ALL,
            run: ac66,
        },
    ]
}

/// The scenarios whose IDs are in `only`, in the suite's order.
pub fn select(only: &[String]) -> Vec<Scenario<RealTarget>> {
    suite()
        .into_iter()
        .filter(|s| only.is_empty() || only.iter().any(|id| id.eq_ignore_ascii_case(s.id)))
        .collect()
}

fn lease_client(ctx: &mut Ctx<'_>) -> Result<Arc<RelayClient>, Stop> {
    let rig = ctx.client.clone();
    ctx.target.client(&rig)
}

fn minutes_from_env(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &f64| *v > 0.0)
        .unwrap_or(default)
}

fn wait_for(
    timeout: Duration,
    mut done: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if done()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("not done in {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn expect(ok: bool, why: impl FnOnce() -> String) -> Result<(), Stop> {
    if ok {
        Ok(())
    } else {
        Err(Stop::Failed(why()))
    }
}

/// Deterministic bytes for uploads: xorshift64* from a seed.
struct Seeded {
    state: u64,
    left: u64,
}

impl Seeded {
    fn new(seed: u64, size: u64) -> Seeded {
        Seeded {
            state: seed | 1,
            left: size,
        }
    }

    fn sha256(seed: u64, size: u64) -> String {
        let mut reader = Seeded::new(seed, size);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buffer).unwrap_or(0);
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        hex(&hasher.finalize())
    }
}

impl Read for Seeded {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let n = (buffer.len() as u64).min(self.left) as usize;
        for chunk in buffer[..n].chunks_mut(8) {
            self.state ^= self.state >> 12;
            self.state ^= self.state << 25;
            self.state ^= self.state >> 27;
            let word = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        self.left -= n as u64;
        Ok(n)
    }
}

// AC-68: the client finds the broker by name.
fn ac68(ctx: &mut Ctx<'_>) -> Verdict {
    let discovery = ctx
        .target
        .discovery
        .clone()
        .ok_or("the target did not start")?;
    let (host, port) = crate::instance::url_host_port(&discovery.relay_url)?;
    ctx.record("relay_url", json!(discovery.relay_url));
    ctx.check(
        "relay_url names a host name",
        expect(host.parse::<std::net::IpAddr>().is_err(), || {
            format!("relay_url {} names an address", discovery.relay_url)
        }),
    );
    let base = crate::instance::base_host(ctx.target.instance().base());
    ctx.check(
        "the instance was named, not addressed",
        expect(base.parse::<std::net::IpAddr>().is_err(), || {
            format!("the instance was given as {base}")
        }),
    );
    // A session through the rig opens even when the lease's relay agent has
    // none, which other scenarios judge.
    let opened = lease_client(ctx);
    let session_opened = opened.is_ok() || ctx.target.facts.contains_key("ready_agent");
    let lease_url = ctx.target.lease.as_ref().map(|l| l.url.clone());
    ctx.record("acquisition_url", json!(lease_url));
    let dials = ctx.target.options_for(&ctx.client, "", "client").url;
    ctx.record("client_dials", json!(dials));
    ctx.check(
        "the client dials the advertised relay_url",
        expect(dials == discovery.relay_url, || {
            "the client was given another URL".into()
        }),
    );
    ctx.check(
        "the client connects by that name through the rig",
        expect(session_opened, || {
            format!("no session opened: {:?}", opened.as_ref().err())
        }),
    );
    ctx.check(
        "the rig serves only that name",
        expect(
            ctx.client.host == host.to_ascii_lowercase() && port > 0,
            || format!("the rig serves {}", ctx.client.host),
        ),
    );
    Ok(())
}

/// AC-55's checks on the lease's guest, so that AC-65 can repeat them.
fn desktop_session_checks(ctx: &mut Ctx<'_>, prefix: &str) -> Verdict {
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    let durable = ctx.target.durable();
    let name = |what: &str| format!("{prefix}{what}");
    match os {
        Os::Macos => {
            let secret = format!("acc-{}", uuid::Uuid::new_v4());
            let keychain = client.exec(
                &format!(
                    "security add-generic-password -U -s relay-acceptance -a ac55 -w {secret} && \
                     security find-generic-password -s relay-acceptance -a ac55 -w"
                ),
                60.0,
                Duration::from_secs(300),
            )?;
            ctx.check(
                &name("the login keychain is read without unlocking it"),
                match keychain.rc()? {
                    Some(0) if keychain.text().trim() == secret => Ok(()),
                    rc => Err(Stop::Failed(format!(
                        "security exited {rc:?} (36 is a locked keychain): {}",
                        keychain.text().trim()
                    ))),
                },
            );
            let finder = client.exec(
                "osascript -e 'tell application \"Finder\" to get name of startup disk'",
                90.0,
                Duration::from_secs(300),
            )?;
            let grants = client.exec(
                "for db in \"$HOME/Library/Application Support/com.apple.TCC/TCC.db\" \"/Library/Application Support/com.apple.TCC/TCC.db\"; do \
                 sqlite3 \"$db\" \"select service || ' ' || client from access where client like '%sshd-keygen-wrapper%' or client = 'com.wezzard.relay-agent'\" 2>/dev/null; done",
                60.0,
                Duration::from_secs(300),
            )?;
            let grants = grants.text();
            ctx.record(&name("tcc"), json!(grants.lines().collect::<Vec<_>>()));
            let agent_events = grants.lines().any(|l| {
                l.contains("kTCCServiceAppleEvents") && l.contains("com.wezzard.relay-agent")
            });
            ctx.record(&name("finder"), json!(finder.text().trim()));
            ctx.check(
                &name("Finder answers osascript without a consent dialog"),
                match finder.rc()? {
                    Some(0) => Ok(()),
                    _ if !agent_events && durable => Err(Stop::Blocked(format!(
                        "blocked by the Mac's own grants: its operator has not granted Apple \
                         Events to com.wezzard.relay-agent, so macOS asks for consent on the \
                         Mac's display ({}); the operator grants it once, as for CuaDriver (CN-3)",
                        finder.text().trim()
                    ))),
                    _ if !agent_events => Err(Stop::Blocked(format!(
                        "blocked by the base image: it grants no Apple Events permission to \
                         com.wezzard.relay-agent, so macOS asks for consent ({}); pilot-images M5 \
                         adds the grant, and the bases are not rebuilt yet",
                        finder.text().trim()
                    ))),
                    rc => Err(Stop::Failed(format!(
                        "osascript exited {rc:?}: {}",
                        finder.text().trim()
                    ))),
                },
            );
            ctx.check(
                &name("no TCC grant for sshd-keygen-wrapper"),
                if grants.contains("sshd-keygen-wrapper") {
                    Err(Stop::Blocked(
                        "blocked by the base image: it still grants privacy permissions to \
                         sshd-keygen-wrapper (pilot-images phase 65), so the run cannot show that \
                         none is needed; a base built with M5 drops them"
                            .into(),
                    ))
                } else {
                    Ok(())
                },
            );
            // `/tmp` is a link on macOS, and cua-driver refuses an output
            // path whose existing ancestor is not a folder; only the output
            // file is named, which every cua-driver release takes.
            let png = format!("/private/tmp/acc-{prefix}screen.png").replace(' ', "-");
            let capture = client.exec(
                &format!(
                    "rm -f {png}; cua-driver call get_desktop_state '{{\"screenshot_out_file\":\"{png}\"}}' && \
                     head -c 8 {png} | od -An -tx1 | tr -d ' \\n'"
                ),
                120.0,
                Duration::from_secs(300),
            )?;
            let ok =
                capture.rc()? == Some(0) && capture.text().trim_end().ends_with("89504e470d0a1a0a");
            if ok {
                let call =
                    client.call(&new_id("read"), Op::Read { path: png.clone() }, b"", true)?;
                if let Ok(answer) = client.wait(call, Duration::from_secs(600)) {
                    let _ = std::fs::write(
                        ctx.run_dir
                            .join(format!("{prefix}ac55-screen.png").replace(' ', "-")),
                        &answer.bytes,
                    );
                }
            }
            ctx.check(
                &name("cua-driver captures the screen"),
                expect(ok, || format!("cua-driver: {}", capture.text().trim())),
            );
        }
        Os::Linux => {
            let session = client.exec(
                "echo \"DISPLAY=$DISPLAY WAYLAND_DISPLAY=$WAYLAND_DISPLAY XDG_SESSION_TYPE=$XDG_SESSION_TYPE DBUS=$DBUS_SESSION_BUS_ADDRESS\"; \
                 gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.GetId",
                60.0,
                Duration::from_secs(300),
            )?;
            let text = session.text();
            ctx.record(&name("session"), json!(text.trim()));
            let value = |key: &str| {
                text.split_whitespace()
                    .find_map(|word| word.strip_prefix(key))
                    .unwrap_or_default()
                    .to_string()
            };
            let display = !value("DISPLAY=").is_empty() || !value("WAYLAND_DISPLAY=").is_empty();
            ctx.check(
                &name("the desktop session's display and session bus"),
                expect(
                    session.rc()? == Some(0) && display && text.contains("DBUS=unix:"),
                    || text.trim().to_string(),
                ),
            );
            let secret = format!("acc-{}", uuid::Uuid::new_v4());
            let keyring = client.exec(
                &format!(
                    "command -v secret-tool >/dev/null || {{ echo no-secret-tool; exit 3; }}; \
                     printf %s {secret} | secret-tool store --label=relay-acceptance service relay-acceptance account ac55 && \
                     secret-tool lookup service relay-acceptance account ac55"
                ),
                30.0,
                Duration::from_secs(300),
            )?;
            let out = keyring.text();
            ctx.record(&name("keyring"), json!(out.trim()));
            let timed_out = matches!(
                keyring.outcome()?,
                Outcome::Exit {
                    timed_out: true,
                    ..
                }
            );
            ctx.check(
                &name("the login keyring is read"),
                match keyring.rc()? {
                    Some(0) if out.trim() == secret => Ok(()),
                    _ if timed_out
                        || out.contains("no-secret-tool")
                        || out.contains("org.freedesktop.secrets")
                        || out.to_ascii_lowercase().contains("locked") =>
                    {
                        Err(Stop::Blocked(format!(
                            "blocked by the base image: no unlocked login keyring in the session \
                             ({}); the ubuntu2404 fix is in pilot-images, and the base is not \
                             rebuilt yet",
                            if timed_out {
                                "secret-tool waited for an unlock prompt".to_string()
                            } else {
                                out.trim().to_string()
                            }
                        )))
                    }
                    rc => Err(Stop::Failed(format!(
                        "secret-tool exited {rc:?}: {}",
                        out.trim()
                    ))),
                },
            );
        }
    }
    Ok(())
}

// AC-55: an action runs in the desktop session.
fn ac55(ctx: &mut Ctx<'_>) -> Verdict {
    desktop_session_checks(ctx, "")
}

// AC-56: a command behaves as it would in a terminal of the desktop session.
fn ac56(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    for (command, want) in [
        ("exit 0", 0),
        ("exit 1", 1),
        ("acc-no-such-command-56", 127),
    ] {
        let answer = client.exec(command, 60.0, Duration::from_secs(300))?;
        ctx.check(
            &format!("`{command}` exits {want}"),
            expect(answer.rc()? == Some(want), || {
                format!("it exited {:?}", answer.rc())
            }),
        );
    }

    let input: Vec<u8> = {
        let mut bytes = vec![0u8; 1 << 20];
        Seeded::new(56, bytes.len() as u64)
            .read_exact(&mut bytes)
            .ok();
        bytes
    };
    let call = client.call(
        &new_id("stdin"),
        exec_op(os.sha256_stdin(), true, 300.0),
        &input,
        true,
    )?;
    let answer = client.wait(call, Duration::from_secs(900))?;
    ctx.check(
        "1 MiB on standard input hashes to the sent bytes",
        expect(answer.text().trim() == sha256_hex(&input), || {
            format!("the guest printed {:?}", answer.text().trim())
        }),
    );

    let no_survivors = |pattern: &str| -> Result<(), Stop> {
        let mut last = String::new();
        let result = wait_for(Duration::from_secs(20), || {
            last = client.run(&format!("pgrep -lf '{pattern}' || true"))?;
            Ok(last.trim().is_empty())
        });
        result.map_err(|_| Stop::Failed(format!("processes survived: {}", last.trim())))
    };
    let timeout = client.exec(
        "sleep 3561 & sleep 3561 & (sleep 3561 &); wait",
        3.0,
        Duration::from_secs(300),
    )?;
    ctx.check(
        "a command past its timeout is killed",
        expect(
            matches!(
                timeout.outcome()?,
                Outcome::Exit {
                    timed_out: true,
                    ..
                }
            ),
            || format!("{:?}", timeout.outcome()),
        ),
    );
    ctx.check(
        "every process of a timed-out group exits",
        no_survivors("sleep 356[1]"),
    );

    let target = new_id("cancel-target");
    let running = client.call(
        &target,
        exec_op(
            "sleep 3562 & sleep 3562 & (sleep 3562 &); wait",
            false,
            600.0,
        ),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(3));
    let cancel = client.call(
        &new_id("cancel"),
        Op::Cancel {
            target: target.clone(),
        },
        b"",
        true,
    )?;
    let cancel = client.wait(cancel, Duration::from_secs(300))?;
    let cancelled = client.wait(running, Duration::from_secs(300))?;
    ctx.check(
        "a cancel signals the running command",
        expect(
            matches!(
                cancel.outcome()?,
                Outcome::Cancel {
                    found: CancelFound::Signalled,
                    ..
                }
            ) && matches!(
                cancelled.outcome()?,
                Outcome::Exit {
                    cancelled: true,
                    ..
                }
            ),
            || {
                format!(
                    "cancel {:?}, command {:?}",
                    cancel.outcome(),
                    cancelled.outcome()
                )
            },
        ),
    );
    ctx.check(
        "every process of a cancelled group exits",
        no_survivors("sleep 356[2]"),
    );
    Ok(())
}

// AC-57: large output arrives whole.
fn ac57(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    let sha = os.sha256_stdin();
    let sums = client.run(&format!(
        "head -c 200000000 /dev/urandom > ~/acc57.out && head -c 1000000 /dev/urandom > ~/acc57.err && \
         cat ~/acc57.out ~/acc57.err | {sha}"
    ))?;
    let want = sums.trim().to_string();
    let started = Instant::now();
    let call = client.call(
        &new_id("ac57"),
        exec_op("cat ~/acc57.out; cat ~/acc57.err >&2", false, 7200.0),
        b"",
        false,
    )?;
    let answer = client.wait(call, Duration::from_secs(7200))?;
    let seconds = started.elapsed().as_secs_f64();
    ctx.record("seconds", json!(seconds));
    ctx.record(
        "mb_per_s",
        json!(answer.size as f64 / 1e6 / seconds.max(0.001)),
    );
    let _ = client.run("rm -f ~/acc57.out ~/acc57.err");
    expect(answer.rc()? == Some(0), || {
        format!("the command exited {:?}", answer.rc())
    })?;
    ctx.check(
        "201,000,000 bytes arrive",
        expect(answer.size == 201_000_000, || {
            format!("{} bytes arrived", answer.size)
        }),
    );
    ctx.check(
        "the streams hash as the command computed",
        expect(answer.sha256 == want && answer.end_matches(), || {
            format!("received {}, the guest computed {want}", answer.sha256)
        }),
    );
    Ok(())
}

// AC-58: files move whole or not at all.
fn ac58(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    let sha = os.sha256_stdin();
    let size_of = os.size_of_f();

    // 1 KiB each way.
    let small: Vec<u8> = (0..1024u32).map(|i| (i * 7 % 251) as u8).collect();
    let call = client.call(
        &new_id("push-small"),
        Op::Write {
            path: "acc58-small.bin".into(),
            name: String::new(),
            mode: Some(0o644),
        },
        &small,
        true,
    )?;
    let written = client.wait(call, Duration::from_secs(600))?;
    let call = client.call(
        &new_id("pull-small"),
        Op::Read {
            path: "acc58-small.bin".into(),
        },
        b"",
        true,
    )?;
    let read = client.wait(call, Duration::from_secs(600))?;
    let in_guest = client.run(&format!("cat ~/acc58-small.bin | {sha}"))?;
    ctx.check(
        "1 KiB pushed and pulled with its SHA-256",
        expect(
            matches!(written.outcome()?, Outcome::Written(d) if d.sha256 == sha256_hex(&small))
                && read.bytes == small
                && read.end_matches()
                && in_guest.trim() == sha256_hex(&small),
            || {
                format!(
                    "written {:?}, read {} bytes",
                    written.outcome(),
                    read.bytes.len()
                )
            },
        ),
    );

    // 200 MiB pushed while a reader in the guest watches the path.
    let size: u64 = 200 << 20;
    let want = Seeded::sha256(58, size);
    let watcher = client.call(
        &new_id("watch"),
        exec_op(
            &format!(
                "f=~/acc58-large.bin; rm -f \"$f\" ~/acc58.done; seen=0; \
                 while [ ! -e ~/acc58.done ]; do s=$({size_of}); \
                 if [ -n \"$s\" ]; then seen=$((seen+1)); [ \"$s\" = {size} ] || echo \"PARTIAL $s\"; fi; sleep 0.05; done; \
                 echo \"watched $seen\""
            ),
            false,
            7200.0,
        ),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(2));
    let started = Instant::now();
    let call = client.call_from(
        &new_id("push-large"),
        Op::Write {
            path: "acc58-large.bin".into(),
            name: String::new(),
            mode: Some(0o644),
        },
        &mut Seeded::new(58, size),
        true,
    )?;
    let written = client.wait(call, Duration::from_secs(7200))?;
    ctx.record(
        "push_200mib_seconds",
        json!(started.elapsed().as_secs_f64()),
    );
    std::thread::sleep(Duration::from_secs(1));
    client.run("touch ~/acc58.done")?;
    let watched = client.wait(watcher, Duration::from_secs(300))?;
    let in_guest = client.run(&format!("cat ~/acc58-large.bin | {sha}"))?;
    let call = client.call(
        &new_id("pull-large"),
        Op::Read {
            path: "acc58-large.bin".into(),
        },
        b"",
        false,
    )?;
    let pulled = client.wait(call, Duration::from_secs(7200))?;
    ctx.record("watcher", json!(watched.text().trim()));
    ctx.check(
        "200 MiB pushed and pulled with its SHA-256",
        expect(
            matches!(written.outcome()?, Outcome::Written(d) if d.sha256 == want)
                && in_guest.trim() == want
                && pulled.sha256 == want
                && pulled.end_matches(),
            || {
                format!(
                    "written {:?}, guest {}, pulled {}",
                    written.outcome(),
                    in_guest.trim(),
                    pulled.sha256
                )
            },
        ),
    );
    ctx.check(
        "no reader in the guest sees a partly written file",
        expect(
            !watched.text().contains("PARTIAL") && watched.text().contains("watched"),
            || watched.text().trim().to_string(),
        ),
    );

    // 2 GB pulled; pushed only when large uploads exist.
    let made = client.run(&format!(
        "f=~/acc58-2g.bin; n=1908; {}; cat \"$f\" | {sha}",
        os.random_file()
    ))?;
    let want = made.trim().to_string();
    let started = Instant::now();
    let call = client.call(
        &new_id("pull-2g"),
        Op::Read {
            path: "acc58-2g.bin".into(),
        },
        b"",
        false,
    )?;
    let pulled = client.wait(call, Duration::from_secs(4 * 3600))?;
    ctx.record("pull_2gb_seconds", json!(started.elapsed().as_secs_f64()));
    let _ = client.run("rm -f ~/acc58-2g.bin ~/acc58-large.bin ~/acc58-small.bin ~/acc58.done");
    ctx.check(
        "a 2 GB file is pulled with its SHA-256",
        expect(
            pulled.sha256 == want && pulled.end_matches() && pulled.size == 1908 << 20,
            || format!("{} bytes, {} against {want}", pulled.size, pulled.sha256),
        ),
    );
    // 2 GB pushed as a stream that continues after any loss (RP-15).
    let size: u64 = 1908 << 20;
    let want = Seeded::sha256(5858, size);
    let started = Instant::now();
    let (written, resumed) = client
        .push(
            &new_id("push-2g"),
            Op::Write {
                path: "acc58-2g-up.bin".into(),
                name: String::new(),
                mode: Some(0o644),
            },
            &|| Box::new(Seeded::new(5858, size)),
            (size, want.clone()),
            Duration::from_secs(4 * 3600),
        )
        .map_err(Stop::Failed)?;
    ctx.record("push_2gb_seconds", json!(started.elapsed().as_secs_f64()));
    ctx.record("push_2gb_continued_from", json!(resumed));
    let in_guest = client.run(&format!("cat ~/acc58-2g-up.bin | {sha}"))?;
    let _ = client.run("rm -f ~/acc58-2g-up.bin");
    ctx.check(
        "a 2 GB file is pushed with its SHA-256",
        expect(
            matches!(written.outcome()?, Outcome::Written(d) if d.sha256 == want && d.size == size)
                && in_guest.trim() == want,
            || format!("written {:?}, guest {}", written.outcome(), in_guest.trim()),
        ),
    );

    // A tree of 10,000 files each way, as one tar stream (RP-13).
    ctx.check(
        "a tree of 10,000 files moves each way",
        ac58_tree(&client, os),
    );
    Ok(())
}

/// The files of AC-58's tree: 100 folders of 100 files, each a different
/// size.
fn tree_files() -> impl Iterator<Item = (String, Vec<u8>)> {
    (0..100).flat_map(|folder| {
        (0..100).map(move |file| {
            let n = folder * 100 + file;
            let line = format!("acc58 tree d{folder:03}/f{file:03}\n");
            let content = line.repeat(1 + (n * 7919) % 97).into_bytes();
            (format!("d{folder:03}/f{file:03}.txt"), content)
        })
    })
}

/// The SHA-256 of a tree's sorted relative file paths, one per line, and
/// of its files' contents in that order: what the guest computes with
/// `find`, `sort` and `cat`.
fn tree_manifest(root: &std::path::Path) -> Result<(usize, String, String), String> {
    fn walk(
        root: &std::path::Path,
        at: &std::path::Path,
        out: &mut Vec<String>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(root.join(at))? {
            let entry = entry?;
            let relative = at.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                walk(root, &relative, out)?;
            } else {
                out.push(format!("./{}", relative.display()));
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(root, std::path::Path::new(""), &mut paths).map_err(|e| e.to_string())?;
    paths.sort();
    let mut names = Sha256::new();
    let mut contents = Sha256::new();
    for path in &paths {
        names.update(path.as_bytes());
        names.update(b"\n");
        let bytes = std::fs::read(root.join(&path[2..])).map_err(|e| e.to_string())?;
        contents.update(&bytes);
    }
    Ok((
        paths.len(),
        hex(&names.finalize()),
        hex(&contents.finalize()),
    ))
}

fn ac58_tree(client: &RelayClient, os: crate::guest::Os) -> Result<(), Stop> {
    let sha = os.sha256_stdin();
    let scratch = tempfile::tempdir().map_err(|e| Stop::Failed(e.to_string()))?;
    let source = scratch.path().join("acc58-tree");
    for (path, content) in tree_files() {
        let path = source.join(path);
        std::fs::create_dir_all(path.parent().expect("a folder"))
            .and_then(|()| std::fs::write(&path, content))
            .map_err(|e| Stop::Failed(e.to_string()))?;
    }
    let want = tree_manifest(&source).map_err(Stop::Failed)?;
    let archive = scratch.path().join("up.tar");
    byte_transfer::pack(&source, &archive).map_err(Stop::Failed)?;
    let whole = byte_transfer::hash_file(&archive).map_err(|e| Stop::Failed(e.to_string()))?;
    let (written, _) = client
        .push(
            &new_id("tree-up"),
            Op::WriteTree {
                path: "acc58-tree".into(),
                name: String::new(),
            },
            &|| match std::fs::File::open(&archive) {
                Ok(file) => Box::new(file),
                Err(_) => Box::new(std::io::empty()),
            },
            whole.clone(),
            Duration::from_secs(3600),
        )
        .map_err(Stop::Failed)?;
    expect(
        matches!(written.outcome()?, Outcome::Written(d) if d.sha256 == whole.1),
        || format!("the tree's upload: {:?}", written.outcome()),
    )?;
    let in_guest = client.run(&format!(
        "cd ~/acc58-tree && find . -type f | LC_ALL=C sort > ../acc58-tree.list && \
         wc -l < ../acc58-tree.list | tr -d ' ' && cat ../acc58-tree.list | {sha} && \
         tr '\\n' '\\0' < ../acc58-tree.list | xargs -0 cat | {sha}; rm -f ../acc58-tree.list"
    ))?;
    let lines: Vec<&str> = in_guest.lines().map(str::trim).collect();
    expect(
        lines
            == [
                want.0.to_string().as_str(),
                want.1.as_str(),
                want.2.as_str(),
            ],
        || format!("the guest's tree: {lines:?}, the client's: {want:?}"),
    )?;
    let call = client.call(
        &new_id("tree-down"),
        Op::ReadTree {
            path: "acc58-tree".into(),
        },
        b"",
        true,
    )?;
    let read = client.wait(call, Duration::from_secs(3600))?;
    let _ = client.run("rm -rf ~/acc58-tree");
    expect(read.end_matches(), || {
        "the tree's stream does not match its end".into()
    })?;
    let back = scratch.path().join("down.tar");
    std::fs::write(&back, &read.bytes).map_err(|e| Stop::Failed(e.to_string()))?;
    let copy = scratch.path().join("copy");
    byte_transfer::unpack(&back, &copy).map_err(Stop::Failed)?;
    let got = tree_manifest(&copy).map_err(Stop::Failed)?;
    expect(got == want, || format!("pulled {got:?}, pushed {want:?}"))
}

// AC-59: a long action completes.
fn ac59(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let minutes = minutes_from_env("RELAY_ACCEPTANCE_LONG_MINUTES", 30.0).round() as u64;
    ctx.record("minutes", json!(minutes));
    let call = client.call(
        &new_id("ac59"),
        exec_op(
            &format!("i=1; while [ $i -le {minutes} ]; do echo \"line $i\"; i=$((i+1)); sleep 60; done; exit 7"),
            false,
            (minutes * 60 + 900) as f64,
        ),
        b"",
        true,
    )?;
    let answer = client.wait(call, Duration::from_secs(minutes * 60 + 1800))?;
    let lines: Vec<String> = answer.text().lines().map(str::to_string).collect();
    let want: Vec<String> = (1..=minutes).map(|i| format!("line {i}")).collect();
    ctx.record(
        "chunk_arrivals_s",
        json!(answer
            .chunks
            .iter()
            .map(|(at, _)| at.as_secs())
            .collect::<Vec<_>>()),
    );
    ctx.record("resumes", json!(client.resumes()));
    ctx.check(
        "every line arrives once and in order",
        expect(lines == want, || {
            format!("{} lines: {:?}", lines.len(), lines.first())
        }),
    );
    ctx.check(
        "the exit status arrives",
        expect(answer.rc()? == Some(7), || {
            format!("{:?}", answer.outcome())
        }),
    );
    Ok(())
}

// AC-60: registration before running, refusal after release.
fn ac60(ctx: &mut Ctx<'_>) -> Verdict {
    let agent = lease_client(ctx).map(drop);
    let client = ctx.target.current_client();
    let (name, record, acquired_in) = {
        let lease = ctx.target.lease.as_ref().ok_or("no lease")?;
        (lease.name.clone(), lease.record.clone(), lease.acquired_in)
    };
    ctx.record("acquired_s", json!(acquired_in.as_secs_f64()));
    ctx.record(
        "relay_agent",
        record.get("relay_agent").cloned().unwrap_or(Value::Null),
    );
    let durable = record.get("kind").and_then(Value::as_str) == Some("durable");
    let registered = match &agent {
        Err(Stop::Blocked(why)) => Err(Stop::Blocked(why.clone())),
        _ => expect(
            (durable || record.get("link").and_then(Value::as_str) == Some("relay"))
                && ctx
                    .target
                    .facts
                    .get("ready_agent")
                    .is_some_and(|a| !a.is_null()),
            || {
                format!(
                    "link {:?}, agent {:?}",
                    record.get("link"),
                    ctx.target.facts.get("ready_agent")
                )
            },
        ),
    };
    ctx.check(
        "the lease ran only after its relay agent registered",
        registered,
    );
    let released = ctx.target.release()?;
    ctx.record("released", released);
    if let Some(client) = client {
        let revoked = wait_for(Duration::from_secs(60), || Ok(client.revoked().is_some()));
        ctx.check(
            "the open session is told it is revoked",
            revoked.map_err(|_| Stop::Failed("no revoked message in 60 s".into())),
        );
    }
    ctx.target.new_session();
    let (client_credential, agent_credential) = ctx
        .target
        .released
        .clone()
        .ok_or("no released credentials")?;
    let refused = |result: Result<_, SessionError>| match result {
        Err(SessionError::Rejected {
            code: RejectCode::Unauthorized,
            ..
        }) => Ok(()),
        Err(other) => Err(Stop::Failed(format!("refused otherwise: {other}"))),
        Ok(_) => Err(Stop::Failed("accepted".into())),
    };
    let options = ctx
        .target
        .options_for(&ctx.client, &client_credential, "client");
    ctx.check(
        "the lease credential is refused",
        refused(crate::client::try_hello(&options)),
    );
    match agent_credential {
        // A durable Mac's agent credential is the machine's, not the
        // lease's (RA-4): the agent keeps its session through the release,
        // and the record says the next lease's calls take it.
        _ if durable => {
            let mut seen = Value::Null;
            let kept = wait_for(Duration::from_secs(60), || {
                seen = ctx.target.instance().record(&name).unwrap_or(Value::Null);
                Ok(seen["link"] == "relay" && seen["relay_agent"].is_object())
            });
            ctx.check(
                "the machine's agent credential outlives the lease",
                kept.map_err(|_| {
                    Stop::Failed(format!(
                        "after the release the record says link {:?}, {}",
                        seen.get("link"),
                        seen.get("link_reason").cloned().unwrap_or(Value::Null)
                    ))
                }),
            );
        }
        Some(credential) => {
            let options = ctx.target.options_for(&ctx.client, &credential, "agent");
            ctx.check(
                "the agent credential is refused",
                refused(crate::client::try_hello(&options)),
            );
        }
        None => ctx.check(
            "the agent credential is refused",
            Err(match &agent {
                Err(Stop::Blocked(why)) => Stop::Blocked(why.clone()),
                _ => Stop::Failed("the agent credential was not read".into()),
            }),
        ),
    }
    ctx.record(
        "record_after",
        json!(format!("{:?}", ctx.target.instance().record(&name))),
    );
    Ok(())
}

/// Wait for a call, and fail unless one result arrived for it.
fn once(client: &RelayClient, call: u64, wait: Duration) -> Result<Answer, Stop> {
    let answer = client.wait(call, wait)?;
    std::thread::sleep(Duration::from_secs(2));
    expect(client.results_for(call) == 1, || {
        format!("{} results arrived", client.results_for(call))
    })?;
    Ok(answer)
}

fn agent_connections(ctx: &mut Ctx<'_>) -> Result<Value, Stop> {
    let host = ctx.target.host()?.clone();
    let text = host.run(&["rig-ctl", "\"connections\""])?;
    serde_json::from_str(text.trim()).map_err(|e| Stop::Failed(format!("rig-ctl: {e}: {text}")))
}

// AC-61: a cut during an action does not run it twice.
fn ac61(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    client.run("rm -f ~/acc61.txt")?;
    let before = agent_connections(ctx)?;
    let resumes = client.resumes();
    let call = client.call(
        &new_id("ac61"),
        exec_op(
            "sleep 8; echo once >> ~/acc61.txt; sleep 8; echo done",
            false,
            600.0,
        ),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(3));
    let cut = ctx.rig.control().cut(Select::All, Side::Both);
    let host = ctx.target.host()?.clone();
    let agent_cut = host.run(&["rig-ctl", "{\"cut\":{\"side\":\"both\"}}"])?;
    ctx.record("client_cut", json!(cut));
    ctx.record("agent_cut", json!(agent_cut.trim()));
    let answer = once(&client, call, Duration::from_secs(900))?;
    let after = agent_connections(ctx)?;
    ctx.record(
        "agent_connections",
        json!({"before": before, "after": after}),
    );
    ctx.check(
        "the client resumed its session",
        expect(
            client.resumes() > resumes && client.ended().is_none(),
            || {
                format!(
                    "resumes {} -> {}, ended {:?}",
                    resumes,
                    client.resumes(),
                    client.ended()
                )
            },
        ),
    );
    let ids = |v: &Value| -> Vec<u64> {
        v["connections"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["id"].as_u64()).collect())
            .unwrap_or_default()
    };
    ctx.check(
        "the relay agent reconnected and resumed",
        expect(
            ids(&after).iter().any(|id| !ids(&before).contains(id))
                && !matches!(answer.outcome()?, Outcome::Interrupted { .. })
                && !answer.result.as_ref().is_some_and(|r| r.replayed),
            || {
                format!(
                    "agent connections {before} -> {after}, outcome {:?}",
                    answer.outcome()
                )
            },
        ),
    );
    ctx.check(
        "the result arrives once",
        expect(answer.text() == "done\n" && answer.rc()? == Some(0), || {
            answer.text()
        }),
    );
    let lines = client.run("wc -l < ~/acc61.txt")?;
    ctx.check(
        "the file holds one line",
        expect(lines.trim() == "1", || format!("{} lines", lines.trim())),
    );
    Ok(())
}

fn first_disconnect_after(client: &RelayClient, since: Duration) -> Option<Duration> {
    client
        .links()
        .into_iter()
        .find(|(at, e)| *at >= since && matches!(e, LinkEvent::Disconnected { .. }))
        .map(|(at, _)| at)
}

// AC-62: a silent connection is detected and resumed.
fn ac62(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let call = client.call(
        &new_id("ac62"),
        exec_op("sleep 25; echo done", false, 600.0),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(2));
    let holes = [Stream::Upstream, Stream::Downstream].map(|stream| Fault::Blackhole {
        stream,
        duration_ms: 0,
    });
    let at = client.age();
    for hole in &holes {
        ctx.rig.control().apply(hole).map_err(|e| e.to_string())?;
    }
    let detected = wait_for(Duration::from_secs(40), || {
        Ok(first_disconnect_after(&client, at).is_some())
    });
    for hole in &holes {
        let _ = ctx.rig.control().revert(hole);
    }
    let after = first_disconnect_after(&client, at).map(|d| d.saturating_sub(at));
    ctx.record("detected_after_s", json!(after.map(|d| d.as_secs_f64())));
    detected.map_err(|_| Stop::Failed("the dead connection was not detected in 40 s".into()))?;
    ctx.check(
        "the dead connection is detected within 15 seconds",
        expect(
            after.is_some_and(|d| d <= Duration::from_millis(15_500)),
            || format!("detected after {after:?}"),
        ),
    );
    let answer = once(&client, call, Duration::from_secs(900))?;
    ctx.check(
        "the session resumes and the result arrives once",
        expect(
            client.ended().is_none() && answer.text() == "done\n",
            || format!("ended {:?}, output {:?}", client.ended(), answer.text()),
        ),
    );
    Ok(())
}

// AC-63: a client that changes its address keeps its session.
fn ac63(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let call = client.call(
        &new_id("ac63"),
        exec_op("sleep 12; echo done", false, 600.0),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(3));
    let at = client.age();
    ctx.rig
        .control()
        .change_address()
        .map_err(|e| e.to_string())?;
    ctx.record("address_change", json!(ctx.rig.control().address_change()));
    let answer = once(&client, call, Duration::from_secs(900))?;
    let peers: Vec<String> = client
        .links()
        .into_iter()
        .filter_map(|(t, e)| match e {
            LinkEvent::Connected {
                resumed: true,
                peer,
            } if t >= at => Some(peer.to_string()),
            _ => None,
        })
        .collect();
    ctx.record("resumed_from", json!(peers));
    ctx.check(
        "the broker resumes the same session",
        expect(!peers.is_empty() && client.ended().is_none(), || {
            format!("no resume after the change; ended {:?}", client.ended())
        }),
    );
    ctx.check(
        "the result arrives once",
        expect(answer.text() == "done\n", || answer.text()),
    );
    Ok(())
}

/// Ask about an execution until it is completed or `wait` passes, and
/// return every state seen.
fn settle(ctx: &mut Ctx<'_>, execution: &str, wait: Duration) -> Result<Vec<ExecutionState>, Stop> {
    let deadline = Instant::now() + wait;
    let mut states = Vec::new();
    loop {
        let client = lease_client(ctx)?;
        match client.query(execution, Duration::from_secs(900)) {
            Ok(state) => {
                let done = matches!(state, ExecutionState::Completed { .. });
                states.push(state);
                if done || Instant::now() >= deadline {
                    return Ok(states);
                }
            }
            Err(error) if Instant::now() < deadline => {
                eprintln!("query {execution}: {error}; again");
                ctx.target.new_session();
            }
            Err(error) => return Err(Stop::Failed(error)),
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn restart_case(
    ctx: &mut Ctx<'_>,
    what: &str,
    restart: &dyn Fn(&mut Ctx<'_>) -> Result<String, Stop>,
) -> Verdict {
    let client = lease_client(ctx)?;
    let file = format!("~/acc64-{what}.txt");
    client.run(&format!("rm -f {file}"))?;
    let execution = new_id(&format!("ac64-{what}"));
    let op = exec_op(
        &format!("sleep 15; echo once >> {file}; sleep 5; echo done"),
        false,
        600.0,
    );
    let call = client.call(&execution, op.clone(), b"", true)?;
    std::thread::sleep(Duration::from_secs(4));
    let restarted = restart(ctx)?;
    ctx.record(&format!("{what}_restart"), json!(restarted.trim()));
    // The open session may resume, or be answered expired; either way the
    // outcome is asked for by execution ID.
    let direct = client.wait(call, Duration::from_secs(120));
    ctx.record(
        &format!("{what}_direct"),
        json!(format!(
            "{:?}",
            direct.as_ref().map(|a| a.outcome().cloned())
        )),
    );
    drop(client);
    let states = settle(ctx, &execution, Duration::from_secs(600))?;
    ctx.record(&format!("{what}_states"), json!(format!("{states:?}")));
    ctx.check(
        &format!("after the {what} restart the action is reported completed, running or unknown"),
        expect(!states.is_empty(), || "no state".into()),
    );
    // The same execution again is answered from the record, never run.
    let client = lease_client(ctx)?;
    let again = client.call(&execution, op, b"", true)?;
    let again = client.wait(again, Duration::from_secs(900))?;
    std::thread::sleep(Duration::from_secs(25));
    let lines = client.run(&format!("cat {file} 2>/dev/null | wc -l"))?;
    let lines: u32 = lines.trim().parse().unwrap_or(99);
    ctx.record(&format!("{what}_lines"), json!(lines));
    ctx.check(
        &format!("after the {what} restart the action never runs again"),
        expect(
            lines <= 1 && again.result.as_ref().is_some_and(|r| r.replayed),
            || format!("{lines} lines; the resend was {:?}", again.result),
        ),
    );
    Ok(())
}

// AC-64: a restart of the broker or the relay agent never replays an action.
fn ac64(ctx: &mut Ctx<'_>) -> Verdict {
    let host = ctx.target.host()?.clone();
    let broker = restart_case(ctx, "broker", &|_ctx| Ok(host.run(&["restart"])?));
    if let Err(stop) = broker {
        ctx.check("the broker restart", Err(stop));
    }
    let agent = restart_case(ctx, "agent", &|ctx| {
        let os = ctx.target.os();
        let label = ctx.target.agent().label;
        let client = lease_client(ctx)?;
        Ok(client.run(&os.detached(&format!("sleep 1; {}", os.kill_agent(&label))))?)
    });
    if let Err(stop) = agent {
        ctx.check("the relay agent restart", Err(stop));
    }
    Ok(())
}

// AC-65 on a durable Mac: the operator's machine, with a CI runner that is
// never paused (DC-25), is not rebooted, nor logged out, which would end the
// runner too. Its stand-in is what launchd does in a new desktop session:
// the agent's LaunchAgent job started afresh (`launchctl kickstart -k`),
// after which the agent registers again by itself (DC-1xx-DUR2-4).
fn ac65_durable(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    let label = ctx.target.agent().label;
    let before = client.run(&os.agent_pid(&label))?.trim().to_string();
    client.run(&os.detached(&format!("sleep 2; {}", os.restart_agent_job(&label))))?;
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(5));
    let mut after = None;
    while started.elapsed() < Duration::from_secs(300) {
        let client = lease_client(ctx)?;
        match client.exec(&os.agent_pid(&label), 60.0, Duration::from_secs(300)) {
            Ok(answer) if answer.rc() == Ok(Some(0)) => {
                let pid = answer.text().trim().to_string();
                if !pid.is_empty() && pid != before {
                    after = Some(pid);
                    break;
                }
            }
            Ok(other) => eprintln!("AC-65: {:?}", other.outcome()),
            Err(error) => {
                eprintln!("AC-65: {error}; a new session");
                ctx.target.new_session();
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    ctx.record("agent_pid_before", json!(before));
    ctx.record("agent_pid_after", json!(after));
    ctx.record("back_after_s", json!(started.elapsed().as_secs_f64()));
    let after =
        after.ok_or("the relay agent did not answer in 5 minutes after its job restarted")?;
    ctx.check(
        "the relay agent registered again without a manual step",
        expect(after != before, || format!("pid {before} -> {after}")),
    );
    desktop_session_checks(ctx, "after the agent's job restarted: ")
}

// AC-65: the relay agent returns after a reboot.
fn ac65(ctx: &mut Ctx<'_>) -> Verdict {
    if ctx.cell.target == network_rig::cell::TargetKind::DurableMac {
        return ac65_durable(ctx);
    }
    let client = lease_client(ctx)?;
    let os = ctx.target.os();
    let booted = client.run("uptime")?;
    client.run(&os.detached(&format!("sleep 2; {}", os.reboot())))?;
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(20));
    // Calls wait for the relay agent's next session (RP-8); the first one
    // that answers is served by the agent the reboot started.
    let mut back = None;
    while started.elapsed() < Duration::from_secs(900) {
        let client = lease_client(ctx)?;
        match client.exec(
            &format!("uptime; {}", os.sshd_is_off()),
            60.0,
            Duration::from_secs(900),
        ) {
            Ok(answer) if answer.rc() == Ok(Some(0)) => {
                back = Some(answer.text());
                break;
            }
            Ok(other) => eprintln!("AC-65: {:?}", other.outcome()),
            Err(error) => {
                eprintln!("AC-65: {error}; a new session");
                ctx.target.new_session();
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    ctx.record("before", json!(booted.trim()));
    ctx.record("after", json!(back));
    ctx.record("back_after_s", json!(started.elapsed().as_secs_f64()));
    let back = back.ok_or("the relay agent did not answer in 15 minutes after the reboot")?;
    ctx.check(
        "the relay agent registered again without a manual step",
        expect(back.contains("up "), || back.clone()),
    );
    ctx.check(
        "sshd stays off after the reboot",
        expect(back.contains("ssh-off"), || back.clone()),
    );
    desktop_session_checks(ctx, "after reboot: ")
}

// AC-66: credentials and ports are guarded.
fn ac66(ctx: &mut Ctx<'_>) -> Verdict {
    let refused = |result: Result<_, SessionError>| match result {
        Err(SessionError::Rejected {
            code: RejectCode::Unauthorized,
            ..
        }) => Ok(()),
        Err(other) => Err(Stop::Failed(format!("refused otherwise: {other}"))),
        Ok(_) => Err(Stop::Failed("accepted".into())),
    };
    let missing = ctx.target.options_for(&ctx.client, "", "client");
    ctx.check(
        "a missing credential is refused",
        refused(crate::client::try_hello(&missing)),
    );
    let wrong: String = (0..43).map(|i| ['A', 'b', '7', '-', '_'][i % 5]).collect();
    let wrong = ctx.target.options_for(&ctx.client, &wrong, "client");
    ctx.check(
        "a wrong credential is refused",
        refused(crate::client::try_hello(&wrong)),
    );
    match ctx.target.released.clone() {
        Some((credential, agent)) => {
            let options = ctx.target.options_for(&ctx.client, &credential, "client");
            let mut result = refused(crate::client::try_hello(&options));
            if let (Ok(()), Some(agent)) = (&result, agent) {
                let options = ctx.target.options_for(&ctx.client, &agent, "agent");
                result = refused(crate::client::try_hello(&options));
            }
            ctx.check("a released credential is refused", result);
        }
        None => ctx.check(
            "a released credential is refused",
            Err(ctx.target.blocked.clone().unwrap_or(Stop::Failed(
                "no lease was released (AC-60 did not run)".into(),
            ))),
        ),
    }
    let expired = expired_credential(ctx);
    ctx.check("an expired credential is refused", expired);
    let facts = ctx.target.facts.clone();
    ctx.record("facts", Value::Object(facts.clone()));
    match facts.get("agent_listening").and_then(Value::as_str) {
        Some(listening) => ctx.check(
            "the relay agent listens on no port",
            expect(listening.trim().is_empty(), || listening.to_string()),
        ),
        None => ctx.check(
            "the relay agent listens on no port",
            Err(ctx
                .target
                .blocked
                .clone()
                .unwrap_or(Stop::Failed("the lease was not set up".into()))),
        ),
    }
    // Every hop: client to edge (the client trusts only the edge), edge to
    // broker (the edge verifies the broker's authority), agent to its edge
    // (wss), and the broker's port itself speaks only TLS.
    let agent_url = facts
        .get("agent_broker_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let plain = ctx.target.broker_addr.map(plain_probe);
    ctx.record("plain_probe", json!(plain));
    let blocked = ctx
        .target
        .blocked
        .clone()
        .filter(|b| matches!(b, Stop::Blocked(_)));
    ctx.check(
        "the client's hops and the broker's port run TLS",
        expect(
            plain
                .as_ref()
                .is_some_and(|answer| !answer.starts_with("HTTP/"))
                && facts.contains_key("ready_agent"),
            || format!("a plain request to the broker got {plain:?}"),
        ),
    );
    ctx.check(
        "the relay agent's hop runs TLS",
        match &blocked {
            Some(stop) if agent_url.is_empty() => Err(stop.clone()),
            _ => expect(agent_url.starts_with("wss://"), || {
                format!("the agent dials {agent_url:?}")
            }),
        },
    );
    ctx.check(
        "the credentials work when a proxy ends TLS on the way",
        match &blocked {
            Some(stop) => Err(stop.clone()),
            None => expect(
                facts.get("ready_agent").is_some_and(|a| !a.is_null()),
                || "no session through the edge reached the relay agent".into(),
            ),
        },
    );
    Ok(())
}

/// The longest AC-66 waits for a short lease's credentials to end.
const LONGEST_EXPIRY_WAIT: Duration = Duration::from_secs(15 * 60);

/// AC-66's expired credential: a lease of the shortest TTL, whose relay
/// credentials end at the end of its TTL plus the instance's grace bounded
/// by the TTL (RP-3). Its credential is accepted before that end and
/// refused after it.
fn expired_credential(ctx: &mut Ctx<'_>) -> Result<(), Stop> {
    let short = ctx.target.short_lease()?;
    let result = (|| {
        let credential = short.credential.clone().ok_or_else(|| {
            Stop::Failed(format!(
                "the short lease is not served by the relay link: link {}",
                short.link
            ))
        })?;
        let until = short.until.ok_or("the short lease has no ttl_expires_at")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        let wait = Duration::from_secs_f64((until - now).max(0.0) + 5.0);
        ctx.record("short_lease_wait_s", json!(wait.as_secs_f64()));
        if wait > LONGEST_EXPIRY_WAIT {
            return Err(Stop::Blocked(format!(
                "the instance's grace makes a 6-minute lease's credentials end in {} minutes; \
                 the acceptance host runs its instance with MACHINE_SERVICE_GRACE_HOURS=0 \
                 (RELAY_HOST_GRACE_HOURS)",
                wait.as_secs() / 60
            )));
        }
        let options = ctx.target.options_for(&ctx.client, &credential, "client");
        crate::client::try_hello(&options).map_err(|error| {
            Stop::Failed(format!("before its end the credential is refused: {error}"))
        })?;
        std::thread::sleep(wait);
        match crate::client::try_hello(&options) {
            Err(SessionError::Rejected {
                code: RejectCode::Unauthorized,
                ..
            }) => Ok(()),
            Err(other) => Err(Stop::Failed(format!("refused otherwise: {other}"))),
            Ok(_) => Err(Stop::Failed("accepted after its end".into())),
        }
    })();
    ctx.target.release_short(&short);
    result
}

/// Send a plain-text WebSocket upgrade to the broker's port, and return what
/// came back first.
fn plain_probe(addr: std::net::SocketAddr) -> String {
    use std::io::Write;
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(10))
    else {
        return "no connection".into();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.write_all(
        b"GET /relay HTTP/1.1\r\nHost: broker\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    );
    let mut buffer = [0u8; 64];
    match stream.read(&mut buffer) {
        Ok(0) => "closed".into(),
        Ok(n) => String::from_utf8_lossy(&buffer[..n])
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .chars()
            .take(40)
            .collect(),
        Err(error) => format!("error: {error}"),
    }
}

// AC-67: an expired session still reports its operations.
fn ac67(ctx: &mut Ctx<'_>) -> Verdict {
    lease_client(ctx)?;
    let mut options = ctx.target.options(&ctx.client)?;
    options.grace_ms = Some(2000);
    let short = RelayClient::connect(&options)?;
    let execution = new_id("ac67");
    short.call(
        &execution,
        exec_op("sleep 6; echo done", false, 600.0),
        b"",
        true,
    )?;
    std::thread::sleep(Duration::from_secs(1));
    // Keep the client away for 3 s: reset its connections, and every new one
    // while the reset is armed.
    let reset = Fault::Reset {
        stream: Stream::Upstream,
        window_ms: 0,
    };
    ctx.rig.control().apply(&reset).map_err(|e| e.to_string())?;
    let cut = ctx.rig.control().cut(Select::All, Side::Both);
    std::thread::sleep(Duration::from_millis(3500));
    ctx.rig
        .control()
        .revert(&reset)
        .map_err(|e| e.to_string())?;
    ctx.record("cut", json!(cut));
    let ended = wait_for(Duration::from_secs(60), || Ok(short.ended().is_some()));
    ctx.record("ended", json!(short.ended()));
    ctx.check(
        "the client is answered expired",
        match ended {
            Ok(()) if short.ended().is_some_and(|e| e.contains("Expired")) => Ok(()),
            _ => Err(Stop::Failed(format!(
                "the session ended {:?}",
                short.ended()
            ))),
        },
    );
    drop(short);
    let states = settle(ctx, &execution, Duration::from_secs(300))?;
    ctx.record("states", json!(format!("{states:?}")));
    ctx.check(
        "a query by execution ID returns the outcome",
        expect(
            matches!(
                states.last(),
                Some(ExecutionState::Completed {
                    outcome: Outcome::Exit { rc: Some(0), .. },
                    ..
                })
            ),
            || format!("{states:?}"),
        ),
    );
    Ok(())
}

// AC-69: a silent action survives the edge's idle close.
fn ac69(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let seconds = minutes_from_env("RELAY_ACCEPTANCE_SILENT_SECONDS", 300.0).round() as u64;
    ctx.record("silent_s", json!(seconds));
    let at = client.age();
    let call = client.call(
        &new_id("ac69"),
        exec_op(
            &format!("sleep {seconds}; exit 3"),
            false,
            (seconds + 600) as f64,
        ),
        b"",
        true,
    )?;
    let answer = client.wait(call, Duration::from_secs(seconds + 1800))?;
    let drops = client
        .links()
        .into_iter()
        .filter(|(t, e)| *t >= at && matches!(e, LinkEvent::Disconnected { .. }))
        .count();
    ctx.record("disconnects", json!(drops));
    if ctx.profile != "N3" {
        ctx.check(
            "the session stays open through heartbeats",
            expect(drops == 0, || {
                format!("{drops} connections dropped during the silence")
            }),
        );
    }
    ctx.check(
        "the exit status arrives",
        expect(answer.rc()? == Some(3), || {
            format!("{:?}", answer.outcome())
        }),
    );
    Ok(())
}

// AC-70: speed is measured and reported, never judged (DC-193); what is
// judged is that each action costs one round trip of the open session
// (DC-194), counted from the session's own counters.
fn ac70(ctx: &mut Ctx<'_>) -> Verdict {
    let client = lease_client(ctx)?;
    let command = "perl -MTime::HiRes=time -e '$s=time; system(\"ls -la ~ >/dev/null\"); printf \"%.6f\\n\", time-$s'";
    let mut total = Vec::new();
    let mut guest = Vec::new();
    let mut trips = Vec::new();
    let mut path_rtt = Vec::new();
    for _ in 0..100 {
        let before = client.session().counters();
        let answer = client.exec(command, 120.0, Duration::from_secs(600))?;
        let after = client.session().counters();
        let g: f64 = answer
            .text()
            .trim()
            .parse()
            .map_err(|_| format!("the action printed {:?}", answer.text()))?;
        total.push(answer.elapsed.as_secs_f64());
        guest.push(g);
        trips.push(after.round_trips_since(&before));
        // The call's envelope to the acknowledgement that covers it: the
        // broker acknowledges on receipt, so this is the path's own round
        // trip, without the guest's work.
        if after.rtt.samples > before.rtt.samples {
            if let Some(rtt) = after.rtt.last {
                path_rtt.push(rtt.as_secs_f64());
            }
        }
    }
    let link: Vec<f64> = total
        .iter()
        .zip(&guest)
        .map(|(t, g)| (t - g).max(0.0))
        .collect();
    let extra: Vec<(usize, u64)> = trips
        .iter()
        .copied()
        .enumerate()
        .filter(|&(_, n)| n != 1)
        .collect();
    let report = json!({
        "cell": ctx.cell.id(),
        "profile": ctx.profile,
        "actions": total.len(),
        "total_s": Summary::of(&total),
        "guest_s": Summary::of(&guest),
        "link_s": Summary::of(&link),
        "path_rtt_s": Summary::of(&path_rtt),
        "round_trips": {
            "per_action": Summary::of(&trips.iter().map(|&n| n as f64).collect::<Vec<_>>()),
            "not_one": extra.iter().map(|&(i, n)| json!({"action": i, "round_trips": n})).collect::<Vec<_>>(),
        },
        "method": "guest time is the action's own clock around its work; link time is the client's round trip less that; the path's round trip is the call's envelope to the broker's acknowledgement; round trips are the client session's flights and new connections during the action",
    });
    let _ = std::fs::write(ctx.run_dir.join("speed.json"), format!("{report:#}\n"));
    ctx.record("speed", report);
    if extra.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} of {} actions cost other than one round trip of the open session: {extra:?}",
            extra.len(),
            trips.len()
        )
        .into())
    }
}
