//! The daemon's durable-machine routes (`docs/arch/machines.md`,
//! `docs/arch/durable-machine.md`).
//!
//! The routes sit beside the virtual-machine routes and answer only for the
//! instance's own durable machine. With exposure off every durable request
//! is answered with HTTP 404 before any lock or workspace is touched, and
//! `/machines` and `/health` show no durable machine (DC-39, AC-43).

use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use machine_service_core::durable::lease::DurableRelay;
use machine_service_core::durable::relay_agent;
use machine_service_core::durable::{DurableConfig, DurableMachine, Reply};
use machine_service_core::relay::Relay;
use machine_service_core::relay_broker::{Role, Target};
use machine_service_core::service::Service;

static MACHINE: OnceLock<Arc<DurableMachine>> = OnceLock::new();
static RELAY: OnceLock<Arc<Relay>> = OnceLock::new();

/// The broker's target for this instance's durable machine: its hardware
/// UUID, which survives a rename (MI-1), else its name.
fn durable_target(machine: &DurableMachine) -> Target {
    Target::Durable(
        machine
            .machine_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| machine.name()),
    )
}

/// Accept the durable machine's relay agent on the broker, when exposure is
/// on (RA-4), route the lease's exec, push and pull through it while it has
/// a session and runs the installed release's build (M4, RA-8), and bring
/// this instance's own agent up (DC-213).
pub fn install_relay(relay: &Arc<Relay>, state_dir: &std::path::Path) {
    let Some(machine) = machine() else { return };
    if !machine.exposed() {
        return;
    }
    let _ = RELAY.set(Arc::clone(relay));
    match machine_service_core::relay::durable_agent_credential(state_dir, false) {
        Ok(credential) => relay.broker.admit(
            &durable_target(machine),
            Role::Agent,
            &machine_service_core::relay_broker::credential_sha256(&credential),
            None,
        ),
        Err(error) => machine_service_core::config::log_to_file(
            &machine.config.log_file,
            &format!("WARN: relay: no durable agent credential: {error}"),
        ),
    }
    admit_current_client(machine, relay);
    // DC-236: only a release that the instance trusts names the
    // build its agent must run for the lease's calls to take it.
    let expected_build = relay
        .held_release()
        .and_then(|held| held.build("macos"))
        .map(|bytes| machine_service_core::service_layout::release::sha256_hex(&bytes))
        .map_err(|error| format!("no trusted relay agent release: {error}"));
    if let Err(reason) = &expected_build {
        machine_service_core::config::log_to_file(
            &machine.config.log_file,
            &format!("WARN: relay: durable exec stays in the service's session: {reason}"),
        );
    }
    let launcher = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_else(|_| std::path::PathBuf::from("machine-service"));
    let agent = relay_agent::identity(state_dir, &machine.config.home);
    machine.attach_relay(DurableRelay {
        broker: relay.broker.clone(),
        target: durable_target(machine),
        expected_build,
        launcher,
        agent_wait: relay.settings.agent_wait,
        // An instance told to wait less for its agents waits less for a
        // purge too (MACHINE_SERVICE_RELAY_AGENT_WAIT).
        purge_wait: machine_service_core::durable::lease::PURGE_WAIT.min(relay.settings.agent_wait),
        agent: agent.clone(),
    });
    provision_agent(machine, relay, state_dir, agent);
}

/// Bring this instance's own relay agent to the state it wants, on a thread
/// of its own (DC-213): the release's build, the machine's
/// credential and this broker's URL, loaded in the desktop session. The
/// agent's label and home derive from the state folder, so a second
/// instance gets an agent of its own exactly as the installed one does and
/// never touches the installed one's. A running agent is restarted only
/// when its executable or LaunchAgent changed and no durable lease is
/// current (DC-118).
fn provision_agent(
    machine: &Arc<DurableMachine>,
    relay: &Arc<Relay>,
    state_dir: &std::path::Path,
    agent: relay_agent::Identity,
) {
    let log_file = machine.config.log_file.clone();
    let log = move |message: &str| machine_service_core::config::log_to_file(&log_file, message);
    let release_dir = match machine_service_core::relay::release_dir() {
        Ok(dir) => dir,
        Err(error) => {
            log(&format!(
                "WARN: relay: the relay agent {} is not provisioned: {}",
                agent.label, error.message
            ));
            return;
        }
    };
    let team = match relay_agent::configured_team() {
        Ok(team) => team,
        Err(error) => {
            log(&format!(
                "WARN: relay: the relay agent {} is not provisioned: {error}",
                agent.label
            ));
            return;
        }
    };
    let mut spec = relay_agent::Spec {
        identity: agent,
        state_dir: state_dir.to_path_buf(),
        session_port: relay.settings.session_port,
        release_dir,
        rotate: false,
        team,
    };
    std::thread::spawn(move || {
        spec.identity = spec.identity.clone().with_login_path();
        let may_restart = !relay_agent::durable_lease_current(&spec.state_dir);
        let label = &spec.identity.label;
        match relay_agent::ensure(&spec, may_restart) {
            Ok(relay_agent::Ensured::Unchanged) => {
                log(&format!("relay: the relay agent {label} runs as wanted"))
            }
            Ok(relay_agent::Ensured::Configured) => log(&format!(
                "relay: the relay agent {label}'s configuration was updated; it dials {}",
                spec.broker_url()
            )),
            Ok(relay_agent::Ensured::Started { build }) => log(&format!(
                "relay: the relay agent {label} was started from {} (build {}; broker {})",
                spec.identity.root.display(),
                build.get(..12).unwrap_or(&build),
                spec.broker_url()
            )),
            Err(reason) => log(&format!(
                "WARN: relay: the relay agent {label} is not provisioned: {reason}"
            )),
        }
    });
}

/// Add a lease credential to a durable acquisition's answer. It is good
/// while that lease is the machine's current lease (RA-4).
fn with_relay_credential(machine: &Arc<DurableMachine>, mut body: Value) -> Value {
    let (Some(relay), Some(lease_id)) = (
        RELAY.get(),
        body.get("lease_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    ) else {
        return body;
    };
    let credential = relay.broker.issue(
        &durable_target(machine),
        Role::Client,
        Some(client_validity(machine, &lease_id)),
    );
    // The record keeps its hash, so a restart accepts it again (DM-8).
    if let Err(error) = machine.record_relay_client(
        &lease_id,
        &machine_service_core::relay_broker::credential_sha256(&credential),
    ) {
        machine_service_core::config::log_to_file(
            &machine.config.log_file,
            &format!("WARN: relay: the client credential of {lease_id} is not kept: {error}"),
        );
    }
    body["relay"] = relay.answer(&credential);
    body
}

/// A durable lease's client credential is good while its lease is the
/// machine's current one, up to the end of its TTL plus its grace bounded
/// by its TTL, read from the record at every hello, so a renewal moves it
/// (RP-3).
fn client_validity(
    machine: &Arc<DurableMachine>,
    lease_id: &str,
) -> machine_service_core::relay_broker::Validity {
    let current = Arc::clone(machine);
    let lease_id = lease_id.to_string();
    Arc::new(move || {
        current.current_record().is_some_and(|record| {
            let until = machine_service_core::ops::relay_credential_until(
                record["ttl_expires_at"].as_f64().unwrap_or_default(),
                record["ttl_hours"].as_f64().unwrap_or_default(),
                current.config.grace_hours,
            );
            record["lease_id"].as_str() == Some(lease_id.as_str())
                && record["state"] != "released"
                && machine_service_core::config::unix_now() < until
        })
    })
}

/// Accept again the client credential of the durable lease that survived a
/// restart, by the hash its record keeps, with the same validity, as the
/// credentials of a virtual machine's `relay` lease are (DM-8).
fn admit_current_client(machine: &Arc<DurableMachine>, relay: &Arc<Relay>) {
    let Some(record) = machine.current_record() else {
        return;
    };
    let (Some(lease_id), Some(hash)) = (
        record["lease_id"].as_str(),
        record["relay_client_sha256"].as_str(),
    ) else {
        return;
    };
    if record["state"] == "released" {
        return;
    }
    relay.broker.admit(
        &durable_target(machine),
        Role::Client,
        hash,
        Some(client_validity(machine, lease_id)),
    );
    machine_service_core::config::log_to_file(
        &machine.config.log_file,
        &format!("relay: durable lease {lease_id}: client credential admitted again after restart"),
    );
}

/// Build the durable machine. Call once at startup, before the relay link
/// is installed and before [`start`].
pub fn install(service: &Service) -> Result<(), String> {
    let probe = std::env::current_exe().ok().map(|exe| {
        vec![
            exe.to_string_lossy().to_string(),
            "--probe-host".to_string(),
        ]
    });
    let config = DurableConfig::from_environment(
        &service.config.state_dir,
        &service.config.log_file,
        probe,
    )?;
    let machine = Arc::new(DurableMachine::new(config)?);
    MACHINE
        .set(machine)
        .map_err(|_| "durable machine installed twice".to_string())
}

/// Reconcile the durable machine after a restart and start its GC loop
/// (DM-6, DM-8). Call once, after the relay link is installed, so that a
/// release the restart interrupted can purge the relay agent's records
/// (DC-173, DC-177).
pub fn start(service: &Service) {
    let Some(machine) = machine() else { return };
    machine.startup();
    if machine.exposed() {
        service.log(&format!(
            "durable machine {} exposed (lock folder {})",
            machine.name(),
            machine.config.lock_dir.display()
        ));
    }
    let gc = Arc::clone(machine);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(
            machine_service_core::GC_INTERVAL_S,
        ));
        gc.gc_once();
    });
}

fn machine() -> Option<&'static Arc<DurableMachine>> {
    MACHINE.get()
}

fn reply((status, body): (u16, Value)) -> Option<(u16, Value)> {
    Some((status, body))
}

fn answer(reply_value: Reply) -> Option<(u16, Value)> {
    reply((reply_value.status, reply_value.body))
}

/// Add `instance` and `kinds` to the `/health` answer (FD-1).
pub fn health(body: &mut Value, virtual_kind: bool) {
    let Some((instance, _, kinds)) = identity(virtual_kind) else {
        return;
    };
    body["instance"] = json!(instance);
    body["kinds"] = json!(kinds);
    // MI-1 / DC-87: virtual-only instances report hardware identity too.
    if let Some(machine) = machine() {
        let (name, former_names) = machine
            .identity
            .snapshot(machine_service_core::config::unix_now());
        body["instance"] = json!(name);
        body["machine_id"] = json!(machine.machine_id);
        body["former_names"] = former_names;
        // DC-177: purges of the relay agent's records it has not confirmed.
        if machine.exposed() {
            body["relay_purges_pending"] = json!(machine
                .pending_purges()
                .iter()
                .map(machine_service_core::durable::lease::PendingPurge::to_json)
                .collect::<Vec<_>>());
            // DC-231: the Developer ID team this instance trusts to sign its
            // relay agent, as a fingerprint and where it is set; `null`
            // before the first Developer ID installation.
            body["relay_agent_team"] = relay_agent::trust_health(&machine.config.state_dir);
        }
    }
}

/// The instance ID and the kinds it offers, as `/health` states them and the
/// Bonjour advertisement carries them (FD-1, DC-85).
pub fn identity(virtual_kind: bool) -> Option<(String, String, Vec<&'static str>)> {
    let machine = machine()?;
    let mut kinds = Vec::new();
    if machine.exposed() {
        kinds.push("durable");
    }
    if virtual_kind {
        kinds.push("virtual");
    }
    Some((
        machine.name().to_string(),
        machine.machine_id.clone().unwrap_or_default(),
        kinds,
    ))
}

/// Mark the virtual machines' records and list the durable machine in the
/// `/machines` answer (MM-1, MM-3).
pub fn extend_machines(body: &mut Value) {
    let Some(machine) = machine() else { return };
    let name = machine.name();
    if let Some(offers) = body.get_mut("offers").and_then(Value::as_array_mut) {
        for offer in offers.iter_mut() {
            if let Some(object) = offer.as_object_mut() {
                object.insert("instance".to_string(), json!(name));
                object.insert("machine_id".to_string(), json!(machine.machine_id));
            }
        }
    }
    let Some(machines) = body.get_mut("machines").and_then(Value::as_object_mut) else {
        return;
    };
    for record in machines.values_mut() {
        if let Some(object) = record.as_object_mut() {
            object.insert("kind".to_string(), json!("virtual"));
            object.insert("instance".to_string(), json!(name));
            object.insert("machine_id".to_string(), json!(machine.machine_id));
        }
    }
    if machine.exposed() {
        let mut record = machine.machine_record();
        record["name"] = json!(name);
        record["instance"] = json!(name);
        machines.insert(name, record);
    }
}

/// Run one durable GC pass, for `POST /gc`.
pub fn gc_once() {
    if let Some(machine) = machine() {
        machine.gc_once();
    }
}

/// Answer a `GET` for the durable machine, or `None` for any other path.
pub fn get(path: &str) -> Option<(u16, Value)> {
    let machine = machine()?;
    let rest = path.strip_prefix("/machines/")?;
    let mut segments = rest.split('/');
    let name = segments.next()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        return answer(if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        });
    }
    let tail: Vec<&str> = segments.collect();
    match tail.as_slice() {
        [] if name == machine.name() => reply((200, machine.machine_record())),
        [] => answer(machine.unknown_name(name)),
        ["leases", lease_id, "hygiene"] => answer(machine.hygiene(name, lease_id)),
        _ => reply((404, json!({"error": "not found"}))),
    }
}

/// Answer a `POST` for the durable machine, or `None` for any other path.
/// `loopback` says whether the client is on this Mac (DM-5).
pub fn post(path: &str, body: &Value, loopback: bool) -> Option<(u16, Value)> {
    let machine = machine()?;
    if path == "/acquire" {
        if body.get("machine").is_none_or(Value::is_null) {
            return None;
        }
        let acquired = machine.acquire(body);
        if acquired.status == 200 {
            return reply((200, with_relay_credential(machine, acquired.body)));
        }
        return answer(acquired);
    }
    let rest = path.strip_prefix("/machines/")?;
    let mut segments = rest.split('/');
    let name = segments.next()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        return answer(if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        });
    }
    let action = segments.next().unwrap_or_default();
    if segments.next().is_some() && action != "console" {
        return reply((404, json!({"error": "not found"})));
    }
    match action {
        "exec" => answer(machine.exec(name, body)),
        "push" => answer(machine.transfer(name, body, true, loopback)),
        "pull" => answer(machine.transfer(name, body, false, loopback)),
        "heartbeat" => answer(machine.heartbeat(name, body)),
        "release" => {
            let released = machine.release(name, body);
            if released.status == 200 {
                if let Some(relay) = RELAY.get() {
                    relay
                        .broker
                        .revoke_clients(&durable_target(machine), "released");
                }
            }
            answer(released)
        }
        "refresh" => answer(machine.refresh(name, body)),
        // DM-9: the owner views a durable machine with Screen Sharing.
        "console" => reply((
            404,
            json!({"error": "a durable machine has no console operations"}),
        )),
        _ => reply((404, json!({"error": "not found"}))),
    }
}

/// For the byte-carrying file endpoints (FD-4): `None` when `name` is not
/// the durable machine; otherwise whether the transfer may proceed, checked
/// before any byte is received.
pub fn files_check(name: &str, body: &Value) -> Option<Result<(), (u16, Value)>> {
    let machine = machine()?;
    if !machine.owns_name(name) {
        return None;
    }
    if !machine.exposed() {
        let refused = if name == machine.name() {
            machine.not_exposed()
        } else {
            machine.unknown_name(name)
        };
        return Some(Err((refused.status, refused.body)));
    }
    Some(
        machine
            .check_lease(name, body)
            .map_err(|refused| (refused.status, refused.body)),
    )
}

/// Run a byte-carrying transfer's file-system half on the durable machine:
/// the instance itself holds the staged path, so the path-based transfer
/// applies (FD-4).
pub fn files_transfer(name: &str, body: &Value, push: bool) -> (u16, Value) {
    match machine() {
        Some(machine) => {
            let done = machine.transfer(name, body, push, true);
            (done.status, done.body)
        }
        None => (404, json!({"error": "not found"})),
    }
}

/// Re-read the same source used at startup (MI-2 / DC-88).
pub fn reread_name() -> Result<Option<(String, String)>, String> {
    let Some(machine) = machine() else {
        return Ok(None);
    };
    let next = machine_service_core::durable::config::read_instance_name()?;
    machine
        .identity
        .rename(&next, machine_service_core::config::unix_now())
        .map(|old| old.map(|old| (old, next)))
}
