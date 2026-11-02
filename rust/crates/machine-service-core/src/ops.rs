//! Lease lifecycle operations: acquire, provision, release, heartbeat, and
//! guest execution and transfers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use acquisition_options::GuestNetwork;
use guest_session::client::{Op, SessionError, Transfer};
use serde_json::{Map, Value};

use crate::config::{unix_now, GuestLink};
use crate::error::{OpError, OpResult};
use crate::guest_link::{GuestError, GuestTarget};
use crate::service::Service;
use crate::ssh::shell_quote;
use crate::GC_INTERVAL_S;

/// Time one push or pull gets, as the `scp` transfer does.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum concurrent macOS guests on the whole host.
pub const MAX_MACOS_RUNNING: usize = 2;
/// The reclamation grace period after TTL expiry, in hours, unless the
/// instance sets `MACHINE_SERVICE_GRACE_HOURS` (DM-6).
pub const GRACE_HOURS: i64 = 6;
/// The instance setting of the reclamation grace period, in hours.
pub const GRACE_HOURS_VARIABLE: &str = "MACHINE_SERVICE_GRACE_HOURS";

/// Parse `MACHINE_SERVICE_GRACE_HOURS`: a finite number of hours, 0 or
/// more; unset or empty is [`GRACE_HOURS`].
pub fn parse_grace_hours(value: Option<&str>) -> Result<f64, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(GRACE_HOURS as f64),
        Some(text) => text
            .parse::<f64>()
            .ok()
            .filter(|hours| hours.is_finite() && *hours >= 0.0)
            .ok_or_else(|| {
                format!("{GRACE_HOURS_VARIABLE}={text:?} is not a number of hours, 0 or more")
            }),
    }
}

/// The instance's reclamation grace period, in hours, read once from its
/// environment; an invalid setting stops the daemon at startup, so here it
/// is only ever valid.
pub fn grace_hours() -> f64 {
    static GRACE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *GRACE.get_or_init(|| {
        parse_grace_hours(std::env::var(GRACE_HOURS_VARIABLE).ok().as_deref())
            .unwrap_or(GRACE_HOURS as f64)
    })
}

/// The end of a lease's relay credentials (RP-3, DC-1xx-DUR2-2): the end of
/// its TTL plus a grace that is the instance's reclamation grace bounded by
/// the TTL itself, so a short lease's credentials never outlive it by more
/// than its own length. A renewal moves the TTL, and with it this end;
/// release and reclamation revoke the credentials before it.
pub fn relay_credential_until(ttl_expires_at: f64, ttl_hours: f64, grace_hours: f64) -> f64 {
    ttl_expires_at + grace_hours.min(ttl_hours).max(0.0) * 3600.0
}
/// Consecutive GC passes a `running` record's VM may be missing from Tart's
/// running set before GC reclaims the lease. Two passes tolerate a record
/// that became `running` between the `tart list` snapshot and the state read.
pub const ABSENT_PASSES_BEFORE_RECLAIM: u32 = 2;

fn vms_mut(data: &mut Map<String, Value>) -> &mut Map<String, Value> {
    data.get_mut("vms")
        .and_then(Value::as_object_mut)
        .expect("state always has a vms object")
}

fn active(record: &Value) -> bool {
    let state = record.get("state").and_then(Value::as_str).unwrap_or("");
    crate::lines::ACTIVE_STATES.contains(&state)
}

/// Compute the rounded hours remaining, matching Python `round(x, 2)`.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

impl Service {
    /// Acquire a fresh lease, reserving under the state lock and provisioning
    /// outside it.
    #[allow(clippy::too_many_arguments)]
    pub fn acquire(
        &self,
        purpose: &str,
        image: &str,
        env: &str,
        ttl_hours: &Value,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
        network: &str,
        profile: Option<&str>,
        source: &str,
        expected_source_fingerprint: Option<&Value>,
        vnc: bool,
    ) -> OpResult<Value> {
        let configuration =
            acquisition_options::resolve(cpu, memory_mb, disk_gb, wait, ttl_hours, vnc)
                .map_err(|error| OpError::new(error.to_string()))?;
        let effective_ttl = configuration
            .get("effective")
            .and_then(|value| value.get("initial_ttl_hours"))
            .and_then(Value::as_f64)
            .unwrap_or(24.0);

        let mut request = Map::new();
        request.insert("network".to_string(), Value::String(network.to_string()));
        request.insert("env".to_string(), Value::String(env.to_string()));
        if let Some(profile) = profile {
            request.insert("profile".to_string(), Value::String(profile.to_string()));
        }
        let request = Value::Object(request);
        let guest_network = match crate::network::validate_request(&request)? {
            crate::network::RequestedNetwork::ControlOnly => {
                control_only::require_available()
                    .map_err(|error| OpError::new(error.to_string()))?;
                // This service has no way to boot the control-only network.
                return Err(OpError::new(
                    "control-only unavailable: this service cannot boot it; no NAT fallback",
                ));
            }
            crate::network::RequestedNetwork::Guest(network) => network,
        };
        // An internet guest needs Softnet, the guest firewall, and public
        // resolvers; each is checked before the reservation and the clone,
        // and none falls back to NAT (DC-13, DC-14).
        let guest_dns = if guest_network == GuestNetwork::Internet {
            self.host.softnet().map_err(OpError::new)?;
            self.host.guest_firewall().map_err(OpError::new)?;
            let variable = std::env::var(crate::isolation::GUEST_DNS_VARIABLE).ok();
            Some(crate::isolation::parse_guest_dns(variable.as_deref()).map_err(OpError::new)?)
        } else {
            None
        };

        if !valid_purpose(purpose) {
            return Err(OpError::new("purpose must match [a-z0-9][a-z0-9-]{0,63}"));
        }
        if !(0.1..=24.0 * 30.0).contains(&effective_ttl) {
            return Err(OpError::new("ttl_hours must be within [0.1, 720]"));
        }

        let cfg = self.line_cfg(image)?;
        // Omitted CPU and memory take the image's configured defaults.
        let configured = |name: &str| {
            cfg.get("defaults")
                .and_then(|defaults| defaults.get(name))
                .and_then(Value::as_i64)
                .filter(|value| *value > 0)
        };
        let image_defaults = acquisition_options::ImageDefaults {
            cpu: configured("cpu"),
            memory_mb: configured("memory_mb"),
        };
        let configuration = acquisition_options::resolve_for_image(
            cpu,
            memory_mb,
            disk_gb,
            wait,
            ttl_hours,
            vnc,
            image_defaults,
            guest_network,
        )
        .map_err(|error| OpError::new(error.to_string()))?;
        let effective_resource = |name: &str| {
            configuration
                .get("effective")
                .and_then(|effective| effective.get(name))
                .and_then(Value::as_i64)
        };
        let effective_cpu = effective_resource("cpu");
        let effective_memory_mb = effective_resource("memory_mb");
        let kind = cfg
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("macos")
            .to_string();
        if guest_network == GuestNetwork::Internet && kind == "macos" {
            // A macOS guest resolves over DNS over TLS through the forwarder.
            self.host.dot_forwarder()?;
        }
        if vnc {
            self.consoles
                .require_available(&kind)
                .map_err(|error| OpError::new(error.to_string()))?;
        }
        if source != "base" && source != "work" {
            return Err(OpError::new("source must be base or work"));
        }
        if source == "base" && expected_source_fingerprint.is_some() {
            return Err(OpError::new(
                "expected_source_fingerprint requires source=work",
            ));
        }
        let mut base = cfg
            .get("base_vm")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut source_fingerprint: Option<Value> = None;
        if source == "work" {
            if env != "none" {
                return Err(OpError::new("work-image acceptance requires env=none"));
            }
            let work = cfg
                .get("work_vm")
                .and_then(Value::as_str)
                .filter(|value| valid_vm_name(value))
                .filter(|value| *value != base)
                .ok_or_else(|| OpError::new("image has no distinct configured WORK_VM"))?;
            base = work.to_string();
            let fingerprint = self.stopped_work_fingerprint(&base, &kind)?;
            if let Some(expected) = expected_source_fingerprint {
                if &fingerprint != expected {
                    return Err(OpError::new(
                        "work source fingerprint differs from expected acceptance source",
                    ));
                }
            }
            source_fingerprint = Some(fingerprint);
        }

        let chosen: Option<String> = if env.is_empty() || env == "none" {
            None
        } else {
            Some(env.to_string())
        };
        if let Some(name) = &chosen {
            self.pack_dir(name)?;
        }

        // Phase 1: reserve a lease slot under the cross-process lock.
        let clone_prefix = cfg
            .get("clone_prefix")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind_for_record = kind.clone();
        let image_for_record = image.to_string();
        let purpose_for_record = purpose.to_string();
        let cpu_for_record = cpu;
        let memory_for_record = memory_mb;
        let disk_for_record = disk_gb;
        let fingerprint = self.config.fingerprint();
        let source_for_record = source.to_string();
        let base_for_record = base.clone();
        let chosen_for_record = chosen.clone();
        let slot_holders = std::cell::RefCell::new(None::<String>);
        let reservation = self.state.update(
            |data| {
                let vms = vms_mut(data);
                for record in vms.values() {
                    let same_purpose = record.get("purpose").and_then(Value::as_str)
                        == Some(purpose_for_record.as_str());
                    let same_image = record.get("image").and_then(Value::as_str)
                        == Some(image_for_record.as_str());
                    if same_purpose && same_image && active(record) {
                        let name = record.get("vm").and_then(Value::as_str).unwrap_or("");
                        return Err(OpError::new(format!(
                            "purpose '{purpose_for_record}' already leased on image {image_for_record}: {name}"
                        )));
                    }
                }
                let active_records: Vec<&Value> =
                    vms.values().filter(|record| active(record)).collect();
                if kind_for_record == "macos" {
                    let mac_active = active_records
                        .iter()
                        .filter(|record| {
                            record.get("image_kind").and_then(Value::as_str) == Some("macos")
                        })
                        .count();
                    if mac_active >= MAX_MACOS_RUNNING {
                        let holders = active_records
                            .iter()
                            .filter(|record| {
                                record.get("image_kind").and_then(Value::as_str) == Some("macos")
                            })
                            .map(|record| describe_holder(record))
                            .collect::<Vec<_>>()
                            .join(", ");
                        *slot_holders.borrow_mut() = Some(holders);
                        let extra = match self.host.host_macos_guests() {
                            Some(guests) => format!(
                                "; host-wide Virtualization.framework guests: {guests} ({} not ours)",
                                guests as i64 - mac_active as i64
                            ),
                            None => "; host-wide gauge unavailable".to_string(),
                        };
                        return Err(OpError::new(format!(
                            "macOS VM limit reached ({MAX_MACOS_RUNNING} active); release one first{extra}"
                        )));
                    }
                }
                let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
                let vm = format!("{clone_prefix}{purpose_for_record}-{suffix}");
                let now = unix_now();
                let mut record = Map::new();
                record.insert("vm".to_string(), Value::String(vm.clone()));
                record.insert(
                    "purpose".to_string(),
                    Value::String(purpose_for_record.clone()),
                );
                record.insert(
                    "image".to_string(),
                    Value::String(image_for_record.clone()),
                );
                record.insert(
                    "image_kind".to_string(),
                    Value::String(kind_for_record.clone()),
                );
                record.insert(
                    "lease_id".to_string(),
                    Value::String(uuid::Uuid::new_v4().simple().to_string()),
                );
                record.insert("configuration".to_string(), configuration.clone());
                record.insert(
                    "network".to_string(),
                    Value::String(guest_network.as_str().to_string()),
                );
                if let Some(servers) = &guest_dns {
                    record.insert(
                        "guest_dns".to_string(),
                        Value::Array(
                            servers.iter().map(|server| Value::String(server.to_string())).collect(),
                        ),
                    );
                }
                if let Some(fingerprint) = &fingerprint {
                    record.insert(
                        "environment_fingerprint".to_string(),
                        Value::String(fingerprint.clone()),
                    );
                }
                record.insert(
                    "env".to_string(),
                    chosen_for_record
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                record.insert("state".to_string(), Value::String("pending".to_string()));
                record.insert("created_at".to_string(), number(now));
                record.insert(
                    "ttl_expires_at".to_string(),
                    number(now + effective_ttl * 3600.0),
                );
                record.insert("grace_until".to_string(), Value::Null);
                record.insert("warned".to_string(), Value::Bool(false));
                record.insert("ip".to_string(), Value::Null);
                record.insert(
                    "cpu".to_string(),
                    cpu_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "memory_mb".to_string(),
                    memory_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "disk_gb".to_string(),
                    disk_for_record.map_or(Value::Null, number_i),
                );
                record.insert(
                    "ssh_auth".to_string(),
                    Value::String("lease-key".to_string()),
                );
                record.insert(
                    "ssh_user".to_string(),
                    cfg.get("ssh_user").cloned().unwrap_or(Value::String(String::new())),
                );
                record.insert("ssh_verified".to_string(), Value::Bool(false));
                record.insert(
                    "ssh_host_trust".to_string(),
                    Value::String("lease-tofu".to_string()),
                );
                if source_for_record == "work" {
                    record.insert(
                        "source".to_string(),
                        Value::String("work".to_string()),
                    );
                    record.insert(
                        "source_vm".to_string(),
                        Value::String(base_for_record.clone()),
                    );
                    record.insert(
                        "source_fingerprint".to_string(),
                        source_fingerprint.clone().unwrap_or(Value::Null),
                    );
                }
                let record_value = Value::Object(record);
                vms.insert(vm.clone(), record_value.clone());
                if vnc {
                    self.consoles
                        .reserve(record_value.as_object().expect("record is an object"))
                        .map_err(|error| OpError::new(error.to_string()))?;
                }
                Ok(vm.clone())
            },
            Option::<fn(&Map<String, Value>)>::None,
        );
        let vm = match reservation {
            Ok(vm) => vm,
            Err(error) => {
                let holders = slot_holders
                    .borrow()
                    .as_ref()
                    .map(|holders| format!("; macOS slots held by: {holders}"))
                    .unwrap_or_default();
                self.log(&format!(
                    "acquire refused (purpose={purpose}, image={image}): {error}{holders}"
                ));
                return Err(error);
            }
        };

        // Phase 2 holds only this VM's operation lock, not the state lock.
        let lock = self.operation_lock(&vm);
        let _guard = lock.lock();
        self.provision(
            &vm,
            &base,
            &cfg,
            chosen,
            effective_cpu,
            effective_memory_mb,
            disk_gb,
            wait,
        )?;
        let mut record = self.get_record(&vm)?;
        // The lease credential travels in this answer only (RP-11).
        if let Some(relay) = self.relay() {
            if let Some(credential) = relay.take_credential(&vm) {
                record["relay"] = relay.answer(&credential);
            }
        }
        Ok(record)
    }

    /// Read-only verification that a configured work image exists and is stopped.
    pub fn stopped_work_fingerprint(&self, vm: &str, kind: &str) -> OpResult<Value> {
        if !self.host.vm_exists(vm)? || self.host.vm_running(vm)? {
            return Err(OpError::new("work source must exist and be stopped"));
        }
        let root = self.tart_store_root().join("vms");
        application_catalog::fingerprint_base(&root, vm, kind)
            .map_err(|error| OpError::new(format!("work source metadata unavailable: {error}")))
    }

    fn tart_store_root(&self) -> PathBuf {
        self.host.tart_store_root()
    }

    /// Clone, boot, and verify a lease outside the state lock.
    #[allow(clippy::too_many_arguments)]
    pub fn provision(
        &self,
        vm: &str,
        base: &str,
        cfg: &Value,
        chosen: Option<String>,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
    ) -> OpResult<()> {
        let clone_owned = AtomicBool::new(false);
        let keys_created = AtomicBool::new(false);
        let result = self.provision_inner(
            vm,
            base,
            cfg,
            &chosen,
            cpu,
            memory_mb,
            disk_gb,
            wait,
            &clone_owned,
            &keys_created,
        );
        if let Err(error) = result {
            self.log(&format!("acquire failed for {vm}: {error}; rolling back"));
            let cleanup = (|| -> OpResult<()> {
                if clone_owned.load(Ordering::SeqCst) {
                    self.begin_release(vm, "acquire-failed", false)?;
                } else {
                    if keys_created.load(Ordering::SeqCst) {
                        let _ = lease_keys::cleanup(&self.config.state_dir, vm);
                    }
                    self.state.update(
                        |data| {
                            vms_mut(data).remove(vm);
                            Ok(())
                        },
                        Option::<fn(&Map<String, Value>)>::None,
                    )?;
                }
                Ok(())
            })();
            if let Err(cleanup_error) = cleanup {
                return Err(OpError::new(format!(
                    "Acquisition failed; {vm} retained for cleanup: {cleanup_error}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn provision_inner(
        &self,
        vm: &str,
        base: &str,
        cfg: &Value,
        chosen: &Option<String>,
        cpu: Option<i64>,
        memory_mb: Option<i64>,
        disk_gb: Option<i64>,
        wait: bool,
        clone_owned: &AtomicBool,
        keys_created: &AtomicBool,
    ) -> OpResult<()> {
        let record = self.get_record(vm)?;
        if record.get("state").and_then(Value::as_str) == Some("releasing") {
            return Err(OpError::new("Acquisition cancelled by release"));
        }
        let kind = cfg.get("kind").and_then(Value::as_str).unwrap_or("macos");
        let expected = record.get("source_fingerprint").cloned();
        if record.get("source").and_then(Value::as_str) == Some("work")
            && self.stopped_work_fingerprint(base, kind)? != expected.unwrap_or(Value::Null)
        {
            return Err(OpError::new("work source changed before clone"));
        }
        if !self.host.vm_exists(base)? {
            return Err(OpError::new(format!("golden base '{base}' not built yet")));
        }
        if self.host.vm_exists(vm)? {
            return Err(OpError::new(format!("clone name collision: {vm}")));
        }
        let key_dir = lease_keys::create(&self.config.state_dir, vm)
            .map_err(|error| OpError::new(error.to_string()))?;
        keys_created.store(true, Ordering::SeqCst);
        clone_owned.store(true, Ordering::SeqCst);
        self.log(&format!("cloning {base} -> {vm}"));
        self.host.tart(
            &["clone".to_string(), base.to_string(), vm.to_string()],
            true,
            1800,
        )?;
        if record.get("source").and_then(Value::as_str) == Some("work")
            && self.stopped_work_fingerprint(base, kind)?
                != record
                    .get("source_fingerprint")
                    .cloned()
                    .unwrap_or(Value::Null)
        {
            return Err(OpError::new("work source changed during clone"));
        }
        let mut set_args = vec![
            "set".to_string(),
            vm.to_string(),
            "--cpu".to_string(),
            (cpu.unwrap_or(6)).to_string(),
            "--memory".to_string(),
            (memory_mb.unwrap_or(16384)).to_string(),
        ];
        if let Some(disk) = disk_gb {
            set_args.push("--disk-size".to_string());
            set_args.push(disk.to_string());
        }
        self.host.tart(&set_args, true, 300)?;

        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new("Acquisition cancelled by release"));
                }
                record["state"] = Value::String("provisioning".to_string());
                if let Some(applied) = record
                    .get_mut("configuration")
                    .and_then(|config| config.get_mut("resources_applied"))
                    .and_then(Value::as_object_mut)
                {
                    applied.insert("status".to_string(), Value::String("applied".to_string()));
                    applied.insert("observed_at".to_string(), number(unix_now()));
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;

        let network = GuestNetwork::of_record(&record)
            .ok_or_else(|| OpError::new(format!("{vm} has a network this service cannot boot")))?;
        self.log(&format!(
            "booting {vm} headless ({}); log {}",
            match network {
                GuestNetwork::Nat => "NAT",
                GuestNetwork::Internet => "Softnet, internet only",
            },
            crate::tart::run_log_path(vm)
        ));
        let mut child = self.host.spawn_run(vm, network)?;
        self.host.boot_refused(&mut *child, vm, None)?;
        let ip = self.host.wait_ip(vm, 420).ok_or_else(|| {
            OpError::new(format!(
                "no IP for {vm} within 420s (see {})",
                crate::tart::run_log_path(vm)
            ))
        })?;
        let ip_for_record = ip.clone();
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                record["ip"] = Value::String(ip_for_record.clone());
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        if network == GuestNetwork::Internet {
            // DC-15: the guest's address enters the firewall's table as soon
            // as it is known, before the guest is reachable for any work.
            self.add_to_firewall(vm, &ip)?;
        }

        let ssh_user = cfg
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ssh_pass = cfg
            .get("ssh_pass")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let bootstrap_timeout = if wait { 420.0 } else { 30.0 };
        self.host
            .bootstrap(&ip, &ssh_user, &ssh_pass, &key_dir, bootstrap_timeout)?;
        if !self
            .host
            .wait_ssh(&ip, &ssh_user, &key_dir, if wait { 420 } else { 0 })
        {
            return Err(OpError::new(format!(
                "Key-only SSH never became ready for {vm}"
            )));
        }
        self.host.verify_transfer(&ip, &ssh_user, &key_dir)?;
        let isolation = if network == GuestNetwork::Internet {
            Some(self.isolate_guest(vm, &record, kind, &ip, &ssh_user, &ssh_pass, &key_dir)?)
        } else {
            None
        };
        if let Some(name) = chosen {
            let pack = self
                .pack_dir(name)?
                .ok_or_else(|| OpError::new(format!("credential pack not found: {name}")))?;
            if !self.inject_pack(&ip, cfg, &pack, &key_dir)? {
                return Err(OpError::new(format!(
                    "env pack injection failed for pack {name}"
                )));
            }
        }

        // RA-7: the relay agent registers before the lease is `running`, or
        // the lease is served over SSH (RA-8).
        let link = self.bootstrap_relay(vm, kind, network, &ip, &ssh_user, &key_dir);

        let wants_vnc = record
            .get("configuration")
            .and_then(|config| config.get("effective"))
            .and_then(|effective| effective.get("vnc"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut console_record: Option<Map<String, Value>> = None;
        if wants_vnc {
            let fresh = self.get_record(vm)?;
            let map = fresh
                .as_object()
                .cloned()
                .ok_or_else(|| OpError::new("lease record is not an object"))?;
            let console = self
                .consoles
                .prepare(&map, &key_dir, if wait { 180 } else { 30 })
                .map_err(|error| OpError::new(error.to_string()))?;
            console_record = Some(console);
        }

        let console_for_ready = console_record.clone();
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new("Acquisition cancelled by release"));
                }
                record["state"] = Value::String("running".to_string());
                record["ssh_verified"] = Value::Bool(true);
                if let Some(console) = &console_for_ready {
                    record["console"] = Value::Object(console.clone());
                }
                if let Some(isolation) = &isolation {
                    record["isolation"] = isolation.clone();
                }
                if let Some(fields) = link.as_object() {
                    for (key, value) in fields {
                        record[key.as_str()] = value.clone();
                    }
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        self.relay_credentials_until(vm);
        self.log(&format!(
            "acquired {vm} (key-only SSH verified, env={}, ip={ip})",
            chosen.as_deref().unwrap_or("none")
        ));
        Ok(())
    }

    /// Add an `internet` guest's address to the firewall's table and record
    /// where, so release removes it (DC-15).
    fn add_to_firewall(&self, vm: &str, ip: &str) -> OpResult<()> {
        let firewall = self.host.guest_firewall().map_err(OpError::new)?;
        let address: std::net::Ipv4Addr = ip
            .parse()
            .map_err(|_| OpError::new(format!("{vm} reported a non-IPv4 address {ip}")))?;
        let table = firewall.table.display().to_string();
        let table_for_record = table.clone();
        let ip_for_record = ip.to_string();
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                record["firewall_table"] = Value::String(table_for_record.clone());
                record["firewall_address"] = Value::String(ip_for_record.clone());
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        guest_firewall::add_guest(
            &firewall.table,
            guest_firewall::Guest {
                address,
                instance: self.firewall_instance(),
                vm: vm.to_string(),
            },
        )
        .map_err(|error| OpError::new(format!("guest firewall table {table}: {error}")))?;
        self.log(&format!(
            "{vm}: {ip} added to the guest firewall table {table}"
        ));
        Ok(())
    }

    /// The name this instance's entries carry in the shared table file.
    fn firewall_instance(&self) -> String {
        self.config.state_dir.display().to_string()
    }

    /// Remove a lease's entry from the firewall table it was added to.
    fn remove_from_firewall(&self, vm: &str, record: &Value) {
        let Some(table) = record.get("firewall_table").and_then(Value::as_str) else {
            return;
        };
        match guest_firewall::remove_guest(
            std::path::Path::new(table),
            &self.firewall_instance(),
            vm,
        ) {
            Ok(()) => self.log(&format!(
                "{vm}: removed from the guest firewall table {table}"
            )),
            Err(error) => self.log(&format!(
                "WARN: {vm}: could not remove it from the guest firewall table {table}: {error}; \
the next GC pass retries"
            )),
        }
    }

    /// Drop this instance's table entries whose lease is gone, so a stale
    /// address never cuts off a later guest that receives it (DC-15).
    pub fn prune_firewall_table(&self) {
        let table = self.host.firewall_table();
        if !table.exists() {
            return;
        }
        let live: std::collections::BTreeSet<String> = match self.state.read() {
            Ok(data) => data
                .get("vms")
                .and_then(Value::as_object)
                .map(|records| records.keys().cloned().collect())
                .unwrap_or_default(),
            Err(_) => return,
        };
        match guest_firewall::prune_guests(&table, &self.firewall_instance(), |vm| {
            live.contains(vm)
        }) {
            Ok(removed) if !removed.is_empty() => self.log(&format!(
                "guest firewall table {}: removed stale entries for {}",
                table.display(),
                removed.join(", ")
            )),
            Ok(_) => {}
            Err(error) => self.log(&format!(
                "WARN: guest firewall table {}: {error}",
                table.display()
            )),
        }
    }

    /// Point an `internet` guest at resolvers over DNS over TLS, show that it
    /// cannot reach this host through its default gateway, and show that it
    /// resolves a name to a public address and loads it over HTTPS
    /// (DC-14, DC-15, DC-16).
    ///
    /// Returns the lease's `isolation` report. Any failure fails the
    /// acquisition, whose rollback releases the lease.
    #[allow(clippy::too_many_arguments)]
    fn isolate_guest(
        &self,
        vm: &str,
        record: &Value,
        kind: &str,
        ip: &str,
        ssh_user: &str,
        ssh_pass: &str,
        key_dir: &std::path::Path,
    ) -> OpResult<Value> {
        let resolvers: Vec<crate::isolation::GuestResolver> = record
            .get("guest_dns")
            .and_then(Value::as_array)
            .map(|items| {
                let text = items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                crate::isolation::parse_guest_dns(Some(&text)).unwrap_or_default()
            })
            .unwrap_or_default();
        if resolvers.is_empty() {
            return Err(OpError::new(format!(
                "{vm} has no guest resolvers recorded"
            )));
        }
        let listed = resolvers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let script = if kind == "macos" {
            let forwarder = self.host.dot_forwarder()?;
            match self.host.ssh(
                ip,
                ssh_user,
                key_dir,
                &crate::isolation::forwarder_upload_command(),
                Some(forwarder),
                120,
            ) {
                Some((0, _)) => {}
                other => {
                    return Err(OpError::new(format!(
                        "the DNS-over-TLS forwarder could not be uploaded: {other:?}"
                    )))
                }
            }
            crate::isolation::macos_resolver_script(&resolvers)
        } else {
            crate::isolation::linux_resolver_script(&resolvers)
        };
        let (command, stdin) = crate::isolation::as_root(&script, ssh_pass);
        match self.host.ssh(ip, ssh_user, key_dir, &command, stdin, 180) {
            Some((0, _)) => self.log(&format!(
                "{vm}: guest resolvers set to {listed} over DNS over TLS"
            )),
            Some((rc, output)) => {
                return Err(OpError::new(format!(
                    "guest resolvers could not be set to {listed} (rc={rc}): {}",
                    output.trim()
                )))
            }
            None => {
                return Err(OpError::new(format!(
                    "guest resolvers could not be set to {listed}: SSH failed"
                )))
            }
        }

        // The isolation probe, retried while the LaunchDaemon reloads the
        // table with this guest's address.
        let firewall = self.host.guest_firewall().map_err(OpError::new)?;
        let deadline = Instant::now() + Duration::from_secs(crate::isolation::PROBE_RETRY_S);
        let mut attempts = 0u32;
        let (outcome, port) = loop {
            attempts += 1;
            let mut listener = self.host.open_probe()?;
            let port = listener.port();
            let probe = crate::isolation::probe_command(kind, port);
            let outcome = match self.host.ssh(
                ip,
                ssh_user,
                key_dir,
                &probe,
                None,
                crate::isolation::PROBE_TIMEOUT_S + 30,
            ) {
                Some((_, output)) => crate::isolation::parse_probe(&output),
                None => crate::isolation::ProbeOutcome {
                    gateway: None,
                    result: crate::isolation::ProbeResult::Error("SSH failed".to_string()),
                },
            };
            let accepted = listener.accepted();
            if crate::isolation::isolated(&outcome, accepted) {
                break (outcome, port);
            }
            if !crate::isolation::worth_retrying(&outcome, accepted) || Instant::now() >= deadline {
                return Err(OpError::new(crate::isolation::probe_failure(
                    &outcome,
                    accepted,
                    port,
                    ip,
                    &firewall.table,
                )));
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        let gateway = outcome
            .gateway
            .map(|gateway| gateway.to_string())
            .unwrap_or_default();
        self.log(&format!(
            "{vm}: isolation verified; {gateway}:{port} on this host did not answer the guest \
(attempt {attempts})"
        ));

        // The egress check: a name resolves to a public address over DNS
        // over TLS, and HTTPS to it answers.
        let mut egress = None;
        for attempt in 1..=crate::isolation::EGRESS_ATTEMPTS {
            let output = self
                .host
                .ssh(
                    ip,
                    ssh_user,
                    key_dir,
                    &crate::isolation::egress_command(kind),
                    None,
                    60,
                )
                .map(|(_, output)| output)
                .unwrap_or_default();
            egress = crate::isolation::parse_egress(&output);
            if crate::isolation::decide_egress(egress.as_ref()).is_ok() {
                break;
            }
            if attempt < crate::isolation::EGRESS_ATTEMPTS {
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        crate::isolation::decide_egress(egress.as_ref()).map_err(OpError::new)?;
        let egress = egress.expect("decided egress has a result");
        self.log(&format!(
            "{vm}: egress verified; {} resolved to {} and HTTPS answered {}",
            crate::isolation::EGRESS_HOST,
            egress.address,
            egress.https
        ));
        Ok(serde_json::json!({
            "status": "verified",
            "address": ip,
            "gateway": gateway,
            "probe_port": port,
            "probe_attempts": attempts,
            "firewall_table": firewall.table.display().to_string(),
            "guest_dns": resolvers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "dns_transport": "dns-over-tls",
            "egress": {
                "name": crate::isolation::EGRESS_HOST,
                "address": egress.address,
                "https": egress.https,
            },
            "observed_at": unix_now(),
        }))
    }

    /// Public release entry point, mirroring the Python lock decision.
    ///
    /// Release commits the `releasing` state before it takes the VM's
    /// operation lock, so a running exec, push, or pull is preempted instead
    /// of waited for; see `docs/lifecycle-fixes.md` (V4).
    pub fn release(&self, vm: &str, reason: &str, expired_only: bool) -> OpResult<Value> {
        self.get_record(vm)?;
        self.log(&format!("release requested for {vm} (reason: {reason})"));
        self.begin_release(vm, reason, expired_only)
    }

    /// Commit the releasing state, preempt a running guest operation, and
    /// tear the VM down.
    pub fn begin_release(&self, vm: &str, reason: &str, expired_only: bool) -> OpResult<Value> {
        if !self.commit_release(vm, expired_only)? {
            return Ok(lease_renewed(vm));
        }
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        self.destroy_lease(vm, reason)
    }

    /// Commit the `releasing` state under the state lock, then set the VM's
    /// release flag so a guest operation holding the operation lock kills its
    /// subprocess and returns.
    ///
    /// Returns `false`, and changes nothing, when an expired-only release
    /// finds the lease no longer due for reclamation.
    fn commit_release(&self, vm: &str, expired_only: bool) -> OpResult<bool> {
        let proceed = self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                self.state.require_lease_environment(record)?;
                if expired_only && record.get("state").and_then(Value::as_str) != Some("releasing")
                {
                    let now = unix_now();
                    let state_value = record.get("state").and_then(Value::as_str).unwrap_or("");
                    let grace = record.get("grace_until").and_then(Value::as_f64);
                    let ttl = record
                        .get("ttl_expires_at")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    if !crate::lines::ACTIVE_STATES.contains(&state_value)
                        || grace.is_none()
                        || now < ttl
                        || now < grace.unwrap_or(0.0)
                    {
                        return Ok(false);
                    }
                }
                record["state"] = Value::String("releasing".to_string());
                let record = record.as_object().expect("lease record is an object");
                self.consoles.revoke(record, "released");
                Ok(true)
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        if proceed {
            self.release_flag(vm).store(true, Ordering::SeqCst);
            // From the moment it starts releasing, no relay credential of the
            // lease is accepted (RP-10).
            self.revoke_relay(vm, expired_only);
        }
        Ok(proceed)
    }

    /// Stop, delete, and unregister a lease, then remove its credentials.
    pub fn destroy_lease(&self, vm: &str, reason: &str) -> OpResult<Value> {
        let present = self
            .state
            .read()?
            .get("vms")
            .and_then(|vms| vms.get(vm))
            .is_some();
        self.close_guest_session(vm);
        if !present {
            return Ok(serde_json::json!({"vm": vm, "released": true, "reason": reason}));
        }
        let record_before = self
            .state
            .read()?
            .get("vms")
            .and_then(|vms| vms.get(vm))
            .cloned()
            .unwrap_or(Value::Null);
        let teardown = (|| -> OpResult<()> {
            if self.host.vm_running(vm)? {
                self.log(&format!("stopping {vm} ({reason})"));
                self.host
                    .tart(&["stop".to_string(), vm.to_string()], true, 180)?;
            }
            if self.host.vm_exists(vm)? {
                self.host
                    .tart(&["delete".to_string(), vm.to_string()], true, 600)?;
            }
            if self.host.vm_exists(vm)? {
                return Err(OpError::new("Tart still reports the clone after deletion"));
            }
            lease_keys::cleanup(&self.config.state_dir, vm)
                .map_err(|error| OpError::new(error.to_string()))?;
            self.log(&format!(
                "deleted {vm} ({reason}); lease credentials removed"
            ));
            // Only once the guest is gone may its address leave the table.
            self.remove_from_firewall(vm, &record_before);
            Ok(())
        })();
        if let Err(error) = teardown {
            self.log(&format!(
                "WARN: teardown of {vm} incomplete; retaining credentials and record: {error}"
            ));
            return Err(OpError::new(format!(
                "Teardown incomplete for {vm}; lease retained for retry"
            )));
        }
        self.state.update(
            |data| {
                vms_mut(data).remove(vm);
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        self.consoles.forget(vm);
        let mut answer = serde_json::json!({"vm": vm, "released": true, "reason": reason});
        // A VM's relay agent keeps its records inside the clone, so they
        // went with it; release asks for no purge (DC-173).
        if record_before.get("link").and_then(Value::as_str) == Some(crate::relay::LINK_RELAY) {
            let skipped = "the relay agent's records were deleted with the VM";
            self.log(&format!("{vm}: relay records not purged: {skipped}"));
            answer["relay_records"] = serde_json::json!({"purged": false, "skipped": skipped});
        }
        Ok(answer)
    }

    /// Renew a lease TTL and clear the grace warning.
    pub fn heartbeat(&self, vm: &str, ttl_hours: Option<&Value>) -> OpResult<Value> {
        // A renewal must not keep a lease alive whose VM is gone. Tart is
        // asked outside the state lock; an unknown record falls through to the
        // update below, which reports it.
        let recorded_running = self
            .get_record(vm)
            .ok()
            .and_then(|record| {
                record
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|state| state == "running");
        if recorded_running {
            match self.host.vm_running(vm) {
                Ok(true) => {}
                Ok(false) => {
                    self.log(&format!(
                        "heartbeat refused for {vm}: Tart reports the VM stopped or absent"
                    ));
                    return Err(OpError::new(format!(
                        "{vm} is not running on the host (Tart reports it stopped or absent); \
release it and acquire a new lease"
                    )));
                }
                Err(error) => self.log(&format!(
                    "WARN: heartbeat {vm}: liveness check unavailable ({error}); renewing anyway"
                )),
            }
        }
        let grant = std::cell::Cell::new(None::<Instant>);
        let renewal = std::cell::Cell::new(None::<(f64, f64)>);
        self.state.update(
            |data| {
                let record = vms_mut(data)
                    .get_mut(vm)
                    .ok_or_else(|| OpError::new(format!("unknown VM: {vm}")))?;
                self.state.require_lease_environment(record)?;
                if record.get("state").and_then(Value::as_str) == Some("releasing") {
                    return Err(OpError::new(format!(
                        "{vm} is releasing; renewal cannot cancel committed teardown"
                    )));
                }
                let ttl = match ttl_hours {
                    Some(ttl_value) => {
                        let resolved =
                            acquisition_options::resolve(None, None, None, true, ttl_value, false)
                                .map_err(|error| OpError::new(error.to_string()))?;
                        resolved
                            .get("effective")
                            .and_then(|value| value.get("initial_ttl_hours"))
                            .and_then(Value::as_f64)
                            .unwrap_or(DEFAULT_TTL_HOURS)
                    }
                    // A bare heartbeat renews the lease for its own initial
                    // TTL; see `docs/lifecycle-fixes.md` (V5).
                    None => lease_initial_ttl_hours(record),
                };
                let expires = unix_now() + ttl * 3600.0;
                record["ttl_expires_at"] = number(expires);
                grant.set(Some(
                    Instant::now() + std::time::Duration::from_secs_f64(ttl * 3600.0),
                ));
                renewal.set(Some((ttl, expires)));
                record["grace_until"] = Value::Null;
                record["warned"] = Value::Bool(false);
                Ok(())
            },
            Some(|data: &Map<String, Value>| {
                if let Some(deadline) = grant.get() {
                    if let Some(record) = data
                        .get("vms")
                        .and_then(|vms| vms.get(vm))
                        .and_then(Value::as_object)
                    {
                        self.consoles.renew(record, Some(deadline));
                    }
                }
            }),
        )?;
        // The renewal moves the end of the lease's relay credentials too.
        self.relay_credentials_until(vm);
        if let Some((ttl, expires)) = renewal.get() {
            self.log(&format!(
                "heartbeat {vm}: TTL renewed for {ttl}h (until {})",
                format_gmtime(expires)
            ));
        }
        let now = unix_now();
        let mut record = self.get_record(vm)?;
        if let Some(object) = record.as_object_mut() {
            let ttl = object
                .get("ttl_expires_at")
                .and_then(Value::as_f64)
                .unwrap_or(now);
            object.insert(
                "ttl_hours_remaining".to_string(),
                number(round2((ttl - now) / 3600.0)),
            );
        }
        Ok(record)
    }

    /// Run a command or script in the guest.
    pub fn guest_exec(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.get_record(vm)?;
        let state = record.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" {
            return Err(OpError::new(format!("{vm} is not running (state={state})")));
        }
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind = record
            .get("image_kind")
            .and_then(Value::as_str)
            .unwrap_or("linux");
        let ip = record
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|ip| !ip.is_empty())
            .or_else(|| self.host.vm_ip(vm))
            .ok_or_else(|| OpError::new("VM has no IP; is it running?"))?;
        let timeout = body.get("timeout").and_then(Value::as_i64).unwrap_or(600);
        if timeout <= 0 {
            return Err(OpError::new("timeout must be positive"));
        }
        let (remote_cmd, stdin) = if let Some(script) = body.get("script").and_then(Value::as_str) {
            let shell = if kind == "macos" { "zsh -s" } else { "bash -s" };
            (shell.to_string(), Some(script.as_bytes().to_vec()))
        } else if let Some(argv) = body.get("argv").and_then(Value::as_array) {
            let command = argv
                .iter()
                .map(|value| shell_quote(&arg_text(value)))
                .collect::<Vec<_>>()
                .join(" ");
            (command, None)
        } else {
            return Err(OpError::new("need 'argv' or 'script'"));
        };
        if let Some(target) = self.relay_target(&record) {
            let script = body
                .get("script")
                .and_then(Value::as_str)
                .map(|script| script.as_bytes().to_vec());
            let command = script.is_none().then(|| remote_cmd.clone());
            return self.relay_exec(vm, &target, body, command, script, timeout as u64);
        }
        if self.config.guest_link == GuestLink::SshSession {
            let target = GuestTarget {
                ip: &ip,
                user: &ssh_user,
                key_dir: &key_dir,
                kind,
            };
            return self.session_exec(vm, &target, &remote_cmd, stdin, timeout as u64);
        }
        let flag = self.release_flag(vm);
        let outcome = crate::proc::with_cancellation(Arc::clone(&flag), || {
            self.host
                .ssh(&ip, &ssh_user, &key_dir, &remote_cmd, stdin, timeout as u64)
        });
        if outcome.is_none() && flag.load(Ordering::SeqCst) {
            self.log(&format!("{vm}: running command cancelled by release"));
            return Err(OpError::new(format!(
                "command cancelled by release of {vm}"
            )));
        }
        let (rc, text) = outcome.ok_or_else(|| OpError::new("ssh failed"))?;
        let truncated = tail_chars(&text, 64000);
        Ok(serde_json::json!({"vm": vm, "rc": rc, "output": truncated}))
    }

    /// Copy a host path into the guest.
    pub fn guest_push(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.running_record(vm)?;
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ip = self.record_ip(vm, &record)?;
        let local = body.get("local_path").and_then(Value::as_str);
        let remote = body.get("remote_path").and_then(Value::as_str);
        let (Some(local), Some(remote)) = (local, remote) else {
            return Err(OpError::new("need local_path and remote_path"));
        };
        if !std::path::Path::new(local).exists() {
            return Err(OpError::new(format!("no such local path: {local}")));
        }
        crate::packs::protect_key_material(
            &self.config.state_dir,
            std::path::Path::new(local),
            true,
        )?;
        if let Some(target) = self.relay_target(&record) {
            if let Some(answer) =
                self.relay_push(vm, &target, body, std::path::Path::new(local), remote)?
            {
                return Ok(answer);
            }
        }
        if self.config.guest_link == GuestLink::SshSession && !std::path::Path::new(local).is_dir()
        {
            let target = GuestTarget {
                ip: &ip,
                user: &ssh_user,
                key_dir: &key_dir,
                kind: record_kind(&record),
            };
            let result = self.with_guest_session(vm, &target, Op::Write, |session| {
                session.write_from_path(std::path::Path::new(local), remote, TRANSFER_TIMEOUT)
            });
            let transfer = self.session_transfer_result(vm, Op::Write, result)?;
            return Ok(serde_json::json!({
                "vm": vm, "pushed": local, "to": remote,
                "bytes": transfer.size, "sha256": transfer.sha256,
            }));
        }
        let destination = format!("{ssh_user}@{ip}:{remote}");
        let result = self.cancellable_transfer(vm, || {
            self.host
                .scp(&ip, &ssh_user, &key_dir, local, &destination, 300)
        })?;
        match result {
            Some((0, _)) => Ok(serde_json::json!({"vm": vm, "pushed": local, "to": remote})),
            other => Err(OpError::new(format!(
                "scp failed: {}",
                other.map(|(_, text)| text).unwrap_or_default()
            ))),
        }
    }

    /// Copy a guest path to the host.
    pub fn guest_pull(&self, vm: &str, body: &Value) -> OpResult<Value> {
        let lock = self.operation_lock(vm);
        let _guard = lock.lock();
        let record = self.running_record(vm)?;
        let key_dir = self.key_directory(&record)?;
        let ssh_user = record
            .get("ssh_user")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ip = self.record_ip(vm, &record)?;
        let local = body.get("local_path").and_then(Value::as_str);
        let remote = body.get("remote_path").and_then(Value::as_str);
        let (Some(local), Some(remote)) = (local, remote) else {
            return Err(OpError::new("need local_path and remote_path"));
        };
        crate::packs::protect_key_material(
            &self.config.state_dir,
            std::path::Path::new(local),
            true,
        )?;
        if let Some(parent) = std::path::Path::new(local).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Some(target) = self.relay_target(&record) {
            if let Some(answer) =
                self.relay_pull(vm, &target, body, remote, std::path::Path::new(local))?
            {
                return Ok(answer);
            }
        }
        if self.config.guest_link == GuestLink::SshSession {
            let target = GuestTarget {
                ip: &ip,
                user: &ssh_user,
                key_dir: &key_dir,
                kind: record_kind(&record),
            };
            let result = self.with_guest_session(vm, &target, Op::Read, |session| {
                session.read_to_path(remote, std::path::Path::new(local), TRANSFER_TIMEOUT)
            });
            // A guest directory is still copied by `scp -r` (GS-6).
            let directory = matches!(
                &result,
                Err(GuestError::Session(SessionError::Remote { code, .. })) if code == "is_directory"
            );
            if !directory {
                let transfer = self.session_transfer_result(vm, Op::Read, result)?;
                return Ok(serde_json::json!({
                    "vm": vm, "pulled": remote, "to": local,
                    "bytes": transfer.size, "sha256": transfer.sha256,
                }));
            }
        }
        let source = format!("{ssh_user}@{ip}:{remote}");
        let result = self.cancellable_transfer(vm, || {
            self.host.scp(&ip, &ssh_user, &key_dir, &source, local, 300)
        })?;
        match result {
            Some((0, _)) => Ok(serde_json::json!({"vm": vm, "pulled": remote, "to": local})),
            other => Err(OpError::new(format!(
                "scp failed: {}",
                other.map(|(_, text)| text).unwrap_or_default()
            ))),
        }
    }

    /// Run an exec on the VM's guest session and answer as the SSH exec does.
    fn session_exec(
        &self,
        vm: &str,
        target: &GuestTarget<'_>,
        command: &str,
        stdin: Option<Vec<u8>>,
        timeout_s: u64,
    ) -> OpResult<Value> {
        let result = self.with_guest_session(vm, target, Op::Exec, |session| {
            session.exec(command, stdin.as_deref(), Duration::from_secs(timeout_s))
        });
        if self.release_flag(vm).load(Ordering::SeqCst) {
            self.log(&format!("{vm}: running command cancelled by release"));
            return Err(OpError::new(format!(
                "command cancelled by release of {vm}"
            )));
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(GuestError::Open(error)) => return Err(error),
            Err(GuestError::Session(error)) if error.is_uncertain(Op::Exec) => {
                self.log(&format!("{vm}: exec outcome uncertain: {error}"));
                return Err(OpError::uncertain(format!(
                    "the guest session failed after the command was sent; whether it ran is unknown: {error}"
                )));
            }
            Err(GuestError::Session(error)) => {
                return Err(OpError::new(format!("ssh failed: {error}")))
            }
        };
        if outcome.timed_out {
            return Err(OpError::new(format!(
                "ssh failed: the command timed out after {timeout_s}s and was killed"
            )));
        }
        let text = String::from_utf8_lossy(&outcome.output);
        let truncated = tail_chars(&text, 64000);
        // `ssh` exits 255 when the remote command is ended by a signal.
        let rc = outcome.rc.unwrap_or(255);
        let mut answer = serde_json::json!({"vm": vm, "rc": rc, "output": truncated});
        if let Some(signal) = outcome.signal {
            answer["signal"] = serde_json::json!(signal);
        }
        Ok(answer)
    }

    /// Map a guest session transfer's result onto the service's errors.
    fn session_transfer_result(
        &self,
        vm: &str,
        op: Op,
        result: Result<Transfer, GuestError>,
    ) -> OpResult<Transfer> {
        let verb = if op == Op::Read { "pull" } else { "push" };
        if self.release_flag(vm).load(Ordering::SeqCst) {
            self.log(&format!("{vm}: running transfer cancelled by release"));
            return Err(OpError::new(format!(
                "transfer cancelled by release of {vm}"
            )));
        }
        match result {
            Ok(transfer) => Ok(transfer),
            Err(GuestError::Open(error)) => Err(error),
            Err(GuestError::Session(error)) if error.is_uncertain(op) => {
                self.log(&format!("{vm}: {verb} outcome uncertain: {error}"));
                Err(OpError::uncertain(format!(
                    "{verb} failed after its bytes were sent; whether the file is in place is unknown: {error}"
                )))
            }
            Err(GuestError::Session(SessionError::Remote { message, .. })) => {
                Err(OpError::new(format!("{verb} failed: {message}")))
            }
            Err(GuestError::Session(error)) => Err(OpError::new(format!("{verb} failed: {error}"))),
        }
    }

    /// Run one `scp` transfer under the VM's release flag. A transfer killed
    /// by a release reports that instead of a generic `scp` failure.
    fn cancellable_transfer(
        &self,
        vm: &str,
        transfer: impl FnOnce() -> Option<(i32, String)>,
    ) -> OpResult<Option<(i32, String)>> {
        let flag = self.release_flag(vm);
        let result = crate::proc::with_cancellation(Arc::clone(&flag), transfer);
        if result.is_none() && flag.load(Ordering::SeqCst) {
            self.log(&format!("{vm}: running transfer cancelled by release"));
            return Err(OpError::new(format!(
                "transfer cancelled by release of {vm}"
            )));
        }
        Ok(result)
    }

    fn running_record(&self, vm: &str) -> OpResult<Value> {
        let record = self.get_record(vm)?;
        let state = record.get("state").and_then(Value::as_str).unwrap_or("");
        if state != "running" {
            return Err(OpError::new(format!("{vm} is not running (state={state})")));
        }
        Ok(record)
    }

    fn record_ip(&self, vm: &str, record: &Value) -> OpResult<String> {
        record
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|ip| !ip.is_empty())
            .or_else(|| self.host.vm_ip(vm))
            .ok_or_else(|| OpError::new("VM has no IP; is it running?"))
    }

    /// Run the garbage collector once.
    pub fn gc_once(&self) -> OpResult<()> {
        self.prune_firewall_table();
        let now = unix_now();
        let mut actions: Vec<(String, &'static str, Option<f64>)> = Vec::new();
        // One Tart snapshot per pass, taken outside the state lock and only
        // after every record passed the environment check, so a mismatched
        // store never reaches Tart. When Tart cannot be asked, liveness is not
        // judged in this pass.
        let snapshot = self.state.read()?;
        let mut any_running = false;
        if let Some(records) = snapshot.get("vms").and_then(Value::as_object) {
            for record in records.values() {
                self.state.require_lease_environment(record)?;
                any_running |= record.get("state").and_then(Value::as_str) == Some("running");
            }
        }
        let listing = if any_running {
            Some(self.host.tart_list())
        } else {
            None
        };
        let live: Option<std::collections::HashSet<String>> = match listing {
            None => None,
            Some(Ok(rows)) => Some(
                rows.into_iter()
                    .filter(|(_, state)| state == "running")
                    .map(|(name, _)| name)
                    .collect(),
            ),
            Some(Err(error)) => {
                self.log(&format!("WARN: GC liveness check skipped: {error}"));
                None
            }
        };
        let previous_absent = self
            .absent_passes
            .lock()
            .map_err(|_| OpError::new("absent-pass counter poisoned"))?
            .clone();
        let mut absent: std::collections::HashMap<String, u32> = Default::default();
        self.state.update(
            |data| {
                for (vm, record) in vms_mut(data).iter_mut() {
                    self.state.require_lease_environment(record)?;
                    let state = record.get("state").and_then(Value::as_str).unwrap_or("");
                    if state == "releasing" {
                        actions.push((vm.clone(), "reclaim", None));
                        continue;
                    }
                    if !crate::lines::ACTIVE_STATES.contains(&state) {
                        continue;
                    }
                    if let Some(live) = &live {
                        if state == "running" && !live.contains(vm) {
                            let passes = previous_absent.get(vm).copied().unwrap_or(0) + 1;
                            absent.insert(vm.clone(), passes);
                            if passes >= ABSENT_PASSES_BEFORE_RECLAIM {
                                actions.push((vm.clone(), "reclaim-absent", None));
                                continue;
                            }
                            actions.push((vm.clone(), "absent", Some(passes as f64)));
                        }
                    }
                    let ttl = record
                        .get("ttl_expires_at")
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::INFINITY);
                    if now >= ttl {
                        let grace = record.get("grace_until").and_then(Value::as_f64);
                        match grace {
                            None => {
                                let until = now + grace_hours() * 3600.0;
                                record["grace_until"] = number(until);
                                record["warned"] = Value::Bool(true);
                                actions.push((vm.clone(), "warn", Some(until)));
                            }
                            Some(until) if now >= until => {
                                actions.push((vm.clone(), "reclaim", None));
                            }
                            Some(_) => {}
                        }
                    }
                }
                Ok(())
            },
            Option::<fn(&Map<String, Value>)>::None,
        )?;
        if live.is_some() || !any_running {
            // Records no longer absent (or gone) drop their count.
            if let Ok(mut counter) = self.absent_passes.lock() {
                *counter = absent;
            }
        }
        for (vm, action, grace) in actions {
            if action == "warn" {
                self.log(&format!(
                    "LEASE WARN: {vm} past TTL; grace until {}",
                    grace.map(format_gmtime).unwrap_or_default()
                ));
            } else if action == "absent" {
                self.log(&format!(
                    "LEASE WARN: {vm} is recorded running but Tart reports it stopped or absent \
(pass {} of {ABSENT_PASSES_BEFORE_RECLAIM})",
                    grace.unwrap_or(0.0) as u32
                ));
            } else if action == "reclaim-absent" {
                if matches!(self.host.vm_running(&vm), Ok(true)) {
                    self.log(&format!("GC: {vm} is running again; not reclaiming"));
                    if let Ok(mut counter) = self.absent_passes.lock() {
                        counter.remove(&vm);
                    }
                    continue;
                }
                self.log(&format!(
                    "GC: reclaiming {vm} (VM not running for {ABSENT_PASSES_BEFORE_RECLAIM} \
consecutive passes)"
                ));
                if let Err(error) = self.gc_release(&vm, "vm-not-running", false) {
                    self.log(&format!("WARN: GC release of {vm} failed: {error}"));
                }
                if let Ok(mut counter) = self.absent_passes.lock() {
                    counter.remove(&vm);
                }
            } else {
                self.log(&format!("GC: reclaiming {vm} (TTL + grace expired)"));
                if let Err(error) = self.gc_release(&vm, "ttl-expired", true) {
                    self.log(&format!("WARN: GC release of {vm} failed: {error}"));
                }
            }
        }
        Ok(())
    }

    /// Release a lease from GC without waiting on its operation lock.
    ///
    /// GC commits `releasing` and sets the release flag exactly as
    /// [`Service::release`] does, which preempts a running exec, push, or
    /// pull. If another thread still holds the VM's operation lock, GC logs
    /// the VM and defers its teardown: the record stays `releasing`, and the
    /// next pass reclaims it. One busy VM therefore never delays the
    /// reclamation of the others; see `docs/lifecycle-fixes.md` (V4).
    fn gc_release(&self, vm: &str, reason: &str, expired_only: bool) -> OpResult<()> {
        self.get_record(vm)?;
        self.log(&format!("release requested for {vm} (reason: {reason})"));
        if !self.commit_release(vm, expired_only)? {
            return Ok(());
        }
        let lock = self.operation_lock(vm);
        let Some(_guard) = lock.try_lock() else {
            self.log(&format!(
                "GC: {vm} is busy with another operation; teardown deferred to the next pass"
            ));
            return Ok(());
        };
        self.destroy_lease(vm, reason).map(|_| ())
    }

    /// Release every `pending` or `provisioning` record left by a previous
    /// daemon process, through the normal teardown path.
    ///
    /// Call once at startup, before serving requests. The kernel-held
    /// `daemon.lock` guarantees one daemon per state directory, so after a
    /// restart no acquisition from the previous process can still be in
    /// flight. A failed teardown keeps the record `releasing` for GC to retry.
    /// Returns the names of the reconciled records.
    pub fn reconcile_startup(&self) -> OpResult<Vec<String>> {
        let data = self.state.read()?;
        let mut interrupted: Vec<(String, String)> = Vec::new();
        if let Some(records) = data.get("vms").and_then(Value::as_object) {
            for (vm, record) in records {
                let state = record.get("state").and_then(Value::as_str).unwrap_or("");
                if state != "pending" && state != "provisioning" {
                    continue;
                }
                if let Err(error) = self.state.require_lease_environment(record) {
                    self.log(&format!(
                        "WARN: startup: not reconciling {vm} (state={state}): {error}"
                    ));
                    continue;
                }
                interrupted.push((vm.clone(), state.to_string()));
            }
        }
        let mut reconciled = Vec::new();
        for (vm, state) in interrupted {
            self.log(&format!(
                "startup: reconciling {vm} (state={state}); its acquisition did not survive \
the daemon restart"
            ));
            match self.begin_release(&vm, "acquire-interrupted-by-restart", false) {
                Ok(_) => reconciled.push(vm),
                Err(error) => self.log(&format!(
                    "WARN: startup reconciliation of {vm} incomplete: {error}"
                )),
            }
        }
        self.prune_firewall_table();
        Ok(reconciled)
    }

    /// Run the garbage collector forever.
    pub fn gc_loop(self: Arc<Self>) {
        loop {
            if let Err(error) = self.gc_once() {
                self.log(&format!("WARN: gc error: {error:?}"));
            }
            std::thread::sleep(std::time::Duration::from_secs(GC_INTERVAL_S));
        }
    }
}

/// Describe a lease holding a macOS slot for the refused-acquisition log:
/// its name, state, and grace deadline when it is past its TTL.
fn describe_holder(record: &Value) -> String {
    let vm = record.get("vm").and_then(Value::as_str).unwrap_or("?");
    let state = record.get("state").and_then(Value::as_str).unwrap_or("?");
    match record.get("grace_until").and_then(Value::as_f64) {
        Some(grace) => format!("{vm} ({state}, grace until {})", format_gmtime(grace)),
        None => format!("{vm} ({state})"),
    }
}

/// The result of an expired-only release that found the lease renewed.
fn lease_renewed(vm: &str) -> Value {
    serde_json::json!({
        "vm": vm, "released": false,
        "reason": "lease-renewed-before-reclamation"
    })
}

/// The acquisition default TTL, in hours, used when a record carries none.
const DEFAULT_TTL_HOURS: f64 = 24.0;

/// The lease's own initial TTL: its resolved `initial_ttl_hours`, or the
/// acquisition default for a legacy record without a configuration report.
pub(crate) fn lease_initial_ttl_hours(record: &Value) -> f64 {
    record
        .get("configuration")
        .and_then(|config| config.get("effective"))
        .and_then(|effective| effective.get("initial_ttl_hours"))
        .and_then(Value::as_f64)
        .filter(|ttl| ttl.is_finite() && (0.1..=720.0).contains(ttl))
        .unwrap_or(DEFAULT_TTL_HOURS)
}

fn valid_purpose(purpose: &str) -> bool {
    if purpose.is_empty() || purpose.len() > 64 {
        return false;
    }
    let mut chars = purpose.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_vm_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn arg_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn record_kind(record: &Value) -> &str {
    record
        .get("image_kind")
        .and_then(Value::as_str)
        .unwrap_or("linux")
}

fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn number_i(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

fn format_gmtime(seconds: f64) -> String {
    // SAFETY: `gmtime_r` writes into a caller-provided `tm`.
    unsafe {
        let stamp = seconds as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&stamp, &mut tm).is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}
