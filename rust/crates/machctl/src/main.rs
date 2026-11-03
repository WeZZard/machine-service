//! `machctl` — CLI client for `machine-service` (same code path as the HTTP API).
//!
//! This is the Rust port of `bin/vmctl`. It preserves the observable CLI
//! contract: subcommands, flags and defaults, human-readable formatting,
//! `--json` output, exit codes, and the `machctl: ...` diagnostics on stderr.
//! Agents should prefer the HTTP API (localhost:6240); this CLI exists for
//! humans and for shell-based agents.

mod acquire;
mod argparse_error;
mod argparse_help;
mod capability;
mod consul;
mod federation;
mod files;
mod http;
mod leases;
mod listing;
mod textwrap;

use std::fs;
use std::io::Write;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::{Map, Value};

/// Default loopback port used when `MACHINE_SERVICE_PORT` is unset.
const DEFAULT_PORT: &str = "6240";

/// Default per-request timeout in seconds, matching Python `call(timeout=3600)`.
const DEFAULT_TIMEOUT_S: u64 = 3600;

// `argparse` has no `help` subcommand: `machctl help` is an invalid choice that
// exits 2, so clap's built-in `help` subcommand must stay off to keep the
// command surface identical.
#[derive(Parser, Debug)]
#[command(
    name = "machctl",
    about = "client for machine-service (leases of Tart VMs and durable Macs)",
    disable_help_subcommand = true,
    infer_long_args = true
)]
struct Cli {
    /// absolute selected-environment profile path
    #[arg(long, value_name = "ENVIRONMENT")]
    environment: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// resolve environment offline without writing files
    Environment {
        #[arg(long)]
        json: bool,
    },
    /// lease a machine: a fresh VM clone, or a durable Mac
    Acquire(Box<AcquireArgs>),
    /// block until a lease is running (useful after --no-wait)
    Wait(WaitArgs),
    /// list every machine from every instance
    List {
        /// machine-readable output
        #[arg(long)]
        json: bool,
        /// list only the local instance; do not ask Consul
        #[arg(long)]
        local: bool,
        /// list only this instance (ID or address)
        #[arg(long)]
        instance: Option<String>,
        /// list only machines of this kind
        #[arg(long, value_parser = ["durable", "virtual"])]
        kind: Option<String>,
        /// list only machines with this capability (repeatable)
        #[arg(long)]
        require: Vec<String>,
    },
    /// query available images and capacity
    Images {
        /// machine-readable output
        #[arg(long)]
        json: bool,
        /// rebuild the image catalog first; exit 1 unless it is current
        #[arg(long)]
        reload: bool,
    },
    /// show one image's full record (JSON)
    #[command(name = "images-show")]
    ImagesShow { image: String },
    /// show one lease
    Status { vm: String },
    /// run a command in the guest
    Exec(ExecArgs),
    /// copy host -> guest
    Push {
        vm: String,
        local: String,
        remote: String,
        /// send the bytes over HTTP even to an instance on loopback
        #[arg(long)]
        bytes: bool,
        /// the lease to act under (default: the one machctl remembered)
        #[arg(long)]
        lease_id: Option<String>,
    },
    /// copy guest -> host
    Pull {
        vm: String,
        remote: String,
        local: String,
        /// send the bytes over HTTP even to an instance on loopback
        #[arg(long)]
        bytes: bool,
        /// the lease to act under (default: the one machctl remembered)
        #[arg(long)]
        lease_id: Option<String>,
    },
    /// reset TTL; keeps a leased machine alive
    Heartbeat {
        vm: String,
        #[arg(long)]
        ttl_hours: Option<f64>,
        /// the lease to act under (default: the one machctl remembered)
        #[arg(long)]
        lease_id: Option<String>,
    },
    /// end a lease; a VM is stopped, deleted and unregistered
    Release {
        vm: String,
        /// the lease to act under (default: the one machctl remembered)
        #[arg(long)]
        lease_id: Option<String>,
    },
    /// show supported and unavailable network capabilities
    Capabilities,
    /// describe acquisition and console prerequisites without allocating a VM
    AcquisitionCapabilities,
    /// resolve the exact lease console; viewer is on the service host
    #[command(name = "console-resolve")]
    ConsoleResolve(ConsoleResolveArgs),
    /// open the exact lease console; viewer is on the service host
    #[command(name = "console-open")]
    ConsoleOpen(ConsoleArgs),
    /// cancel the exact lease console; viewer is on the service host
    #[command(name = "console-cancel")]
    ConsoleCancel(ConsoleArgs),
    /// run TTL reclaim now
    Gc,
}

#[derive(Args, Debug)]
struct AcquireArgs {
    /// task name; becomes part of the VM name
    #[arg(long)]
    purpose: String,
    /// image to clone (see: machctl images; default: macos26)
    #[arg(long, alias = "line")]
    image: Option<String>,
    /// durable machine to lease, by name
    #[arg(long)]
    machine: Option<String>,
    /// lease any machine with this capability (repeatable)
    #[arg(long)]
    require: Vec<String>,
    /// instance to acquire the image from, or to choose among (ID or address)
    #[arg(long)]
    instance: Option<String>,
    /// default | none | <env-pack-name> (credential env vars)
    #[arg(long, default_value = "default", alias = "pack")]
    env: String,
    #[arg(long, value_parser = ["nat", "internet", "control-only"], default_value = "nat")]
    network: String,
    /// explicit control-only helper profile
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    ttl_hours: Option<f64>,
    #[arg(long)]
    cpu: Option<i64>,
    #[arg(long)]
    memory_mb: Option<i64>,
    #[arg(long)]
    disk_gb: Option<i64>,
    /// use shorter synchronous SSH readiness checks; transfers are still verified
    #[arg(long)]
    no_wait: bool,
    /// prepare guest sharing without opening a viewer; requires configured console support
    #[arg(long)]
    vnc: bool,
    /// work is explicitly for fresh-clone acceptance of configured WORK_VM; requires --env none
    #[arg(long, value_parser = ["base", "work"], default_value = "base")]
    source: String,
    /// JSON fingerprint file to require before cloning a work image
    #[arg(long)]
    expected_source_fingerprint: Option<String>,
}

#[derive(Args, Debug)]
struct WaitArgs {
    vm: String,
    /// give up after N seconds (default: 900)
    #[arg(long, default_value_t = 900)]
    timeout_s: i64,
}

#[derive(Args, Debug)]
struct ExecArgs {
    vm: String,
    /// run a local script file via guest shell stdin
    #[arg(long)]
    script: Option<String>,
    /// guest command timeout in seconds; transport allows an additional 30 seconds
    #[arg(long, default_value_t = 600)]
    timeout: i64,
    /// the lease to act under (default: the one machctl remembered)
    #[arg(long)]
    lease_id: Option<String>,
    /// command to run (use -- to separate)
    #[arg(value_name = "ARGV", num_args = 0..)]
    argv: Vec<String>,
}

#[derive(Args, Debug)]
struct ConsoleResolveArgs {
    vm: String,
    #[arg(long)]
    lease_id: String,
}

#[derive(Args, Debug)]
struct ConsoleArgs {
    vm: String,
    #[arg(long)]
    lease_id: String,
    #[arg(long)]
    console_id: String,
    #[arg(long)]
    attempt_id: String,
}

/// HTTP JSON client for the loopback service.
struct Client<'a> {
    base: String,
    environment: Option<&'a Value>,
}

impl<'a> Client<'a> {
    fn new(base: String, environment: Option<&'a Value>) -> Self {
        Self { base, environment }
    }

    /// The selected-environment fingerprint, when one is selected.
    fn fingerprint(&self) -> Option<&str> {
        self.environment
            .and_then(|env| env.get("identity"))
            .and_then(|identity| identity.get("fingerprint"))
            .and_then(Value::as_str)
    }

    /// Perform one request and return `(status, parsed_body)`.
    ///
    /// Mirrors Python `call()`: mutations first pre-check the service identity
    /// through `GET /health`, transport failures exit 2 with the restart hint,
    /// and any HTTP status (including 4xx/5xx) returns the parsed body.
    fn call(&self, method: &str, path: &str, body: Option<&Value>, timeout_s: u64) -> (u16, Value) {
        if self.environment.is_some() && method != "GET" {
            let (status, health) = self.call("GET", "/health", None, 10);
            let actual = health
                .get("environment")
                .and_then(|environment| environment.get("fingerprint"))
                .and_then(Value::as_str);
            if status != 200 || actual.is_none() || actual != self.fingerprint() {
                eprintln!("machctl: selected environment identity mismatch; refusing mutation");
                std::process::exit(2);
            }
        }

        let url = format!("{}{}", self.base, path);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(timeout_s)))
            .http_status_as_error(false)
            .build()
            .into();

        let response = if method == "GET" {
            let mut request = agent
                .get(url.as_str())
                .header("Content-Type", "application/json");
            if let Some(fingerprint) = self.fingerprint() {
                request = request.header("X-VM-Environment-Fingerprint", fingerprint);
            }
            request.call()
        } else {
            let mut request = agent
                .post(url.as_str())
                .header("Content-Type", "application/json");
            if let Some(fingerprint) = self.fingerprint() {
                request = request.header("X-VM-Environment-Fingerprint", fingerprint);
            }
            let data = match body {
                Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                None => String::new(),
            };
            request.send(data)
        };

        match response {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().unwrap_or_default();
                (status, parse_response_body(&text))
            }
            Err(error) => {
                eprintln!(
                    "machctl: cannot reach machine-service at {} ({})",
                    self.base,
                    error_reason(&error)
                );
                eprintln!(
                    "start it with: machine-service-install install (launchd) or: machine-service"
                );
                std::process::exit(2);
            }
        }
    }
}

fn main() {
    // A test build keeps its marker and watches its test's lifeline
    // (DC-256); a release does nothing.
    service_layout::test_build::start();
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // `argparse` renders `--help` itself, with a different layout from clap's.
    // Intercept every help request before clap parses and emit the argparse
    // rendering, leaving clap in charge of parsing and of all error paths.
    let command = <Cli as clap::CommandFactory>::command();
    let root = argparse_help::model_from_command(&command);
    if let Some(help) = argparse_help::intercept_help(&root, &arguments) {
        print!("{help}");
        std::process::exit(0);
    }

    let (arguments, command_argv) = split_exec_arguments(arguments);

    let cli = match Cli::try_parse_from(
        std::iter::once("machctl".to_string()).chain(arguments.clone()),
    ) {
        Ok(cli) => cli,
        Err(error) => argparse_error::exit_with(&root, &arguments, &error),
    };
    let mut cli = cli;
    if let (Some(argv), Cmd::Exec(exec)) = (command_argv, &mut cli.cmd) {
        exec.argv = argv;
    }

    let environment = match environment::load_environment(cli.environment.as_deref()) {
        Ok(environment) => environment,
        Err(error) => usage_error(&error.to_string()),
    };

    if cli.environment.is_none() {
        if let Ok(bound) = std::env::var("VM_ENVIRONMENT_FINGERPRINT") {
            let unchanged = environment
                .as_ref()
                .and_then(|env| env.get("identity"))
                .and_then(|identity| identity.get("fingerprint"))
                .and_then(Value::as_str)
                .map(|fingerprint| fingerprint == bound)
                .unwrap_or(false);
            if !unchanged {
                usage_error("selected environment changed during an active command sequence");
            }
        }
    }

    let port = std::env::var("MACHINE_SERVICE_PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
    // A test build never reaches the installed service or its remembered
    // leases (DC-251).
    if let Ok(number) = port.parse::<u16>() {
        if let Err(refusal) = service_layout::test_build::owned_port("MACHINE_SERVICE_PORT", number)
        {
            usage_error(&refusal);
        }
    }
    if let Some(file) = leases::path() {
        if let Err(refusal) = service_layout::test_build::owned_path("the remembered leases", &file)
        {
            usage_error(&refusal);
        }
    }
    let mut base = format!("http://127.0.0.1:{port}");
    if let Some(url) = environment
        .as_ref()
        .and_then(|env| env.get("profile"))
        .and_then(|profile| profile.get("vmServiceUrl"))
        .and_then(Value::as_str)
    {
        base = url.to_string();
    }

    // Federation (FD-2, FD-3, CL-3). A selected environment pins its own
    // instance, so it never discovers others (DC-64).
    let consul_address = consul::base_address(std::env::var("CONSUL_HTTP_ADDR").ok().as_deref());
    // Bonjour by default, Consul by choice, for the CLI as for the daemon (DC-84).
    let discovery_mode =
        discovery::Mode::from_environment().unwrap_or_else(|error| usage_error(&error));
    let local_base = base.trim_end_matches('/').to_string();
    let federation_settings = |discover: bool| federation::Settings {
        local_base: local_base.clone(),
        consul: (discover && environment.is_none() && discovery_mode == discovery::Mode::Consul)
            .then(|| consul_address.clone()),
        browse: (discover && environment.is_none() && discovery_mode == discovery::Mode::Bonjour)
            .then_some(federation::bonjour as federation::Browser),
        local_headers: environment
            .as_ref()
            .and_then(|env| env.get("identity"))
            .and_then(|identity| identity.get("fingerprint"))
            .and_then(Value::as_str)
            .map(|fingerprint| {
                vec![(
                    "X-VM-Environment-Fingerprint".to_string(),
                    fingerprint.to_string(),
                )]
            })
            .unwrap_or_default(),
        timeout: federation::INSTANCE_TIMEOUT,
        consul_timeout: federation::CONSUL_TIMEOUT,
    };
    // Whether the machine's owner is this Mac's own instance; push and pull
    // to any other send the bytes (FD-4, DC-70).
    let mut owner_is_local = true;
    let requested_lease = lease_argument(&cli.cmd);
    let mut remembered_lease = None;
    let mut routed = false;
    if let Some(name) = machine_argument(&mut cli.cmd) {
        if environment.is_some() {
            if federation::split_name(name).1.is_some() {
                usage_error(
                    "a selected environment pins its own instance; name the machine without @<instance>",
                );
            }
        } else {
            let hint = requested_lease.as_ref().and_then(|(_, given)| {
                leases::path().and_then(|path| {
                    leases::resolve(
                        &path,
                        federation::split_name(name).0,
                        &base,
                        None,
                        given.as_deref(),
                    )
                })
            });
            match federation::route_lease(&federation_settings(true), name, hint.as_ref()) {
                Ok(route) => {
                    owner_is_local = route.local;
                    routed = true;
                    if let Some((_, given)) = &requested_lease {
                        remembered_lease = leases::path().and_then(|path| {
                            leases::resolve(
                                &path,
                                federation::split_name(name).0,
                                &route.base,
                                route.machine_id.as_deref(),
                                given.as_deref().or(route.lease_id.as_deref()),
                            )
                        });
                    }
                    *name = remembered_lease
                        .as_ref()
                        .and_then(|lease| lease.granted_name.clone())
                        .unwrap_or(route.machine);
                    base = route.base;
                }
                Err(error) => {
                    eprintln!("machctl: {error}");
                    std::process::exit(1);
                }
            }
        }
    }

    let client = Client::new(base.clone(), environment.as_ref());
    // The lease a lease command acts under: `--lease-id`, or the one this
    // CLI remembered for the machine on its owner (CL-3, DC-72).
    let lease_id = lease_argument(&cli.cmd).map(|(vm, given)| {
        given
            .or_else(|| {
                remembered_lease
                    .as_ref()
                    .map(|lease| lease.lease_id.clone())
            })
            .or_else(|| {
                (!routed)
                    .then(|| leases::path().and_then(|path| leases::find(&path, &vm, &base)))
                    .flatten()
            })
    });
    let lease_id: Option<String> = lease_id.flatten();

    match cli.cmd {
        Cmd::Environment { .. } => match &environment {
            Some(bundle) => println!("{}", pretty_unicode(bundle)),
            None => {
                let mut bundle = Map::new();
                bundle.insert("profile".to_string(), Value::Null);
                bundle.insert("identity".to_string(), Value::Null);
                bundle.insert("environment".to_string(), Value::Object(Map::new()));
                println!("{}", pretty(&Value::Object(bundle)));
            }
        },

        Cmd::Capabilities => {
            let (status, resp) = client.call("GET", "/capabilities", None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("{}", pretty(&resp));
        }

        Cmd::AcquisitionCapabilities => {
            let (status, resp) =
                client.call("GET", "/acquisition-capabilities", None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("{}", pretty(&resp));
        }

        Cmd::ConsoleResolve(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            run_console(&client, &args.vm, "resolve", body);
        }

        Cmd::ConsoleOpen(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            body.insert("console_id".to_string(), Value::String(args.console_id));
            body.insert("attempt_id".to_string(), Value::String(args.attempt_id));
            run_console(&client, &args.vm, "open", body);
        }

        Cmd::ConsoleCancel(args) => {
            let mut body = Map::new();
            body.insert("lease_id".to_string(), Value::String(args.lease_id));
            body.insert("console_id".to_string(), Value::String(args.console_id));
            body.insert("attempt_id".to_string(), Value::String(args.attempt_id));
            run_console(&client, &args.vm, "cancel", body);
        }

        Cmd::Acquire(args) => {
            let flags = acquire::VirtualFlags {
                network: &args.network,
                profile: args.profile.is_some(),
                cpu: args.cpu.is_some(),
                memory_mb: args.memory_mb.is_some(),
                disk_gb: args.disk_gb.is_some(),
                no_wait: args.no_wait,
                vnc: args.vnc,
                source: &args.source,
                expected_source_fingerprint: args
                    .expected_source_fingerprint
                    .as_deref()
                    .is_some_and(|path| !path.is_empty()),
            };
            let target = match acquire::target(
                args.image.clone(),
                args.machine.clone(),
                args.require.clone(),
                args.instance.clone(),
                &flags,
            ) {
                Ok(target) => target,
                Err(error) => usage_error(&error),
            };
            let settings = federation_settings(true);
            let pinned = environment.is_some();
            let (status, resp, owner) = match target {
                acquire::Target::Image { image, instance } => {
                    let body = virtual_body(&args, &image);
                    let base = match instance {
                        None => base.clone(),
                        Some(_) if pinned => usage_error(
                            "a selected environment pins its own instance; --instance cannot be used",
                        ),
                        Some(wanted) => instance_base(&settings, &wanted),
                    };
                    let client = Client::new(base, environment.as_ref());
                    let (status, resp) = client.call(
                        "POST",
                        "/acquire",
                        Some(&Value::Object(body)),
                        DEFAULT_TIMEOUT_S,
                    );
                    (status, resp, client.base)
                }
                acquire::Target::Machine(name) => {
                    let (machine, base) = if pinned {
                        if federation::split_name(&name).1.is_some() {
                            usage_error(
                                "a selected environment pins its own instance; name the machine without @<instance>",
                            );
                        }
                        (name.clone(), base.clone())
                    } else {
                        match federation::route(&settings, &name) {
                            Ok(route) => (route.machine, route.base),
                            Err(error) => {
                                eprintln!("machctl: {error}");
                                std::process::exit(1);
                            }
                        }
                    };
                    let client = Client::new(base, environment.as_ref());
                    let body = durable_body(&args, &machine);
                    let (status, resp) = client.call(
                        "POST",
                        "/acquire",
                        Some(&Value::Object(body)),
                        DEFAULT_TIMEOUT_S,
                    );
                    (status, resp, client.base)
                }
                acquire::Target::Require { names, instance } => {
                    if pinned && instance.is_some() {
                        usage_error(
                            "a selected environment pins its own instance; --instance cannot be used",
                        );
                    }
                    acquire_by_capability(
                        &args,
                        &names,
                        instance.as_deref(),
                        &settings,
                        &consul_address,
                        environment.as_ref(),
                    )
                }
            };
            if status != 200 {
                die_from(status, &resp, None);
            }
            remember_lease(&resp, &owner);
            println!("{}", pretty(&resp));
        }

        Cmd::Wait(args) => {
            let deadline = Instant::now()
                + Duration::from_secs(if args.timeout_s > 0 {
                    args.timeout_s as u64
                } else {
                    0
                });
            loop {
                let (status, resp) = client.call(
                    "GET",
                    &format!("/machines/{}", args.vm),
                    None,
                    DEFAULT_TIMEOUT_S,
                );
                if status == 200 && resp.get("state").and_then(Value::as_str) == Some("running") {
                    println!("{}", pretty(&resp));
                    return;
                }
                if status == 404 {
                    die_from(status, &resp, Some(&args.vm));
                }
                if Instant::now() >= deadline {
                    let state = if status == 200 {
                        match resp.get("state") {
                            Some(state) => python_str(state),
                            None => "?".to_string(),
                        }
                    } else {
                        format!("HTTP {status}")
                    };
                    eprintln!(
                        "machctl: {} not running after {}s (state: {state}); check ~/.local/state/machine-service/service.log and /tmp/tart-run-{}.log",
                        args.vm, args.timeout_s, args.vm
                    );
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }

        Cmd::Images { json, reload } => {
            // A reload rebuilds even unchanged files and reports the result
            // in the listing that follows (IC-3).
            let mut reload_failed = false;
            if reload {
                let (status, resp) = client.call("POST", "/images/reload", None, DEFAULT_TIMEOUT_S);
                if status != 200 && status != 503 {
                    die_from(status, &resp, None);
                }
                reload_failed = resp
                    .get("catalog")
                    .and_then(|catalog| catalog.get("status"))
                    .and_then(Value::as_str)
                    != Some("current");
            }
            let (_status, resp) = client.call("GET", "/images", None, DEFAULT_TIMEOUT_S);
            if json {
                println!("{}", pretty(&resp));
            } else {
                if let Some(warning) = catalog_warning(&resp) {
                    println!("{warning}");
                }
                print!("{}", format_images(&resp));
            }
            if reload_failed || resp.get("images").is_none() {
                std::process::exit(1);
            }
        }

        Cmd::ImagesShow { image } => {
            let (_status, resp) = client.call("GET", "/images", None, DEFAULT_TIMEOUT_S);
            if resp.get("images").is_none() {
                // No catalog to look in: say why instead of "unknown image".
                eprintln!("machctl: {}", error_message_text(&resp));
                std::process::exit(1);
            }
            let found = resp
                .get("images")
                .and_then(Value::as_object)
                .and_then(|images| images.get(&image))
                .filter(|record| !is_falsy(record));
            match found {
                Some(record) => println!("{}", pretty(record)),
                None => {
                    eprintln!("{}", format_unknown_image(&image, &resp));
                    std::process::exit(1);
                }
            }
        }

        Cmd::List {
            json,
            local,
            instance,
            kind,
            require,
        } => {
            if let Err(error) = capability::validate(&require) {
                usage_error(&error);
            }
            if local && instance.is_some() {
                usage_error("--local and --instance cannot be combined");
            }
            let settings = federation_settings(!local);
            let merged = match federation::gather(&settings, instance.as_deref()) {
                Ok(merged) => merged,
                Err(error) => {
                    eprintln!("machctl: {error}");
                    std::process::exit(1);
                }
            };
            // A registry that did not answer is said, never shown as an
            // empty intranet (FD-3).
            if let Some(error) = &merged.consul_error {
                eprintln!(
                    "machctl: warning: Consul at {consul_address} did not answer ({error}); listing only the local instance"
                );
            }
            browse_notes(&merged, "listing only the local instance");
            for note in &merged.notes {
                eprintln!("machctl: note: {note}");
            }
            let filters = listing::Filters { kind, require };
            if json {
                println!("{}", pretty(&listing::to_json(&merged, &filters)));
            } else {
                print!("{}", listing::format_table(&merged, &filters));
            }
            if merged.none_reachable() {
                eprintln!("machctl: no machine-service instance answered");
                if merged.instances.iter().any(|instance| instance.local) {
                    eprintln!(
                        "start it with: machine-service-install install (launchd) or: machine-service"
                    );
                }
                std::process::exit(2);
            }
        }

        Cmd::Status { vm } => {
            let (status, resp) =
                client.call("GET", &format!("/machines/{vm}"), None, DEFAULT_TIMEOUT_S);
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("{}", pretty(&resp));
        }

        Cmd::Exec(args) => {
            let mut body = Map::new();
            if let Some(script) = args.script.as_deref().filter(|script| !script.is_empty()) {
                let contents = match fs::read_to_string(script) {
                    Ok(contents) => contents,
                    Err(error) => {
                        eprintln!("machctl: {error}");
                        std::process::exit(1);
                    }
                };
                body.insert("script".to_string(), Value::String(contents));
            } else if !args.argv.is_empty() {
                body.insert(
                    "argv".to_string(),
                    Value::Array(args.argv.iter().cloned().map(Value::String).collect()),
                );
            } else {
                usage_error("exec needs a command after -- or --script FILE");
            }
            if args.timeout <= 0 {
                usage_error("--timeout must be positive");
            }
            body.insert("timeout".to_string(), json_int(args.timeout));
            insert_lease(&mut body, &lease_id);
            let transport_timeout = if args.timeout > 0 {
                (args.timeout + 30) as u64
            } else {
                30
            };
            let (status, resp) = client.call(
                "POST",
                &format!("/machines/{}/exec", args.vm),
                Some(&Value::Object(body)),
                transport_timeout,
            );
            if status != 200 {
                die_from(status, &resp, Some(&args.vm));
            }
            let output = resp.get("output").and_then(Value::as_str).unwrap_or("");
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            let _ = stdout.write_all(output.as_bytes());
            let _ = stdout.flush();
            let rc = resp.get("rc").and_then(Value::as_i64).unwrap_or(0);
            std::process::exit(rc as i32);
        }

        Cmd::Push {
            vm,
            local,
            remote,
            bytes,
            ..
        } => {
            if bytes || !owner_is_local || !files::is_loopback(&base) {
                match files::push(
                    &base,
                    client.fingerprint(),
                    &vm,
                    std::path::Path::new(&local),
                    &remote,
                    lease_id.as_deref(),
                ) {
                    Ok(_) => println!("pushed {local} -> {vm}:{remote}"),
                    Err(error) => {
                        eprintln!("machctl: push to {vm} failed: {error}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let mut body = Map::new();
            insert_lease(&mut body, &lease_id);
            body.insert("local_path".to_string(), Value::String(local.clone()));
            body.insert("remote_path".to_string(), Value::String(remote.clone()));
            let (status, resp) = client.call(
                "POST",
                &format!("/machines/{vm}/push"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("pushed {local} -> {vm}:{remote}");
        }

        Cmd::Pull {
            vm,
            remote,
            local,
            bytes,
            ..
        } => {
            if bytes || !owner_is_local || !files::is_loopback(&base) {
                match files::pull(
                    &base,
                    client.fingerprint(),
                    &vm,
                    &remote,
                    std::path::Path::new(&local),
                    lease_id.as_deref(),
                ) {
                    Ok(target) => println!("pulled {vm}:{remote} -> {}", target.display()),
                    Err(error) => {
                        eprintln!("machctl: pull from {vm} failed: {error}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let mut body = Map::new();
            insert_lease(&mut body, &lease_id);
            body.insert("remote_path".to_string(), Value::String(remote.clone()));
            body.insert("local_path".to_string(), Value::String(local.clone()));
            let (status, resp) = client.call(
                "POST",
                &format!("/machines/{vm}/pull"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!("pulled {vm}:{remote} -> {local}");
        }

        Cmd::Heartbeat { vm, ttl_hours, .. } => {
            let mut body = Map::new();
            insert_lease(&mut body, &lease_id);
            if let Some(ttl_hours) = ttl_hours {
                if ttl_hours != 0.0 {
                    body.insert("ttl_hours".to_string(), json_float(ttl_hours));
                }
            }
            let (status, resp) = client.call(
                "POST",
                &format!("/machines/{vm}/heartbeat"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            println!(
                "heartbeat ok; ttl_hours_remaining={}",
                python_str(resp.get("ttl_hours_remaining").unwrap_or(&Value::Null))
            );
        }

        Cmd::Release { vm, .. } => {
            let mut body = Map::new();
            insert_lease(&mut body, &lease_id);
            let (status, resp) = client.call(
                "POST",
                &format!("/machines/{vm}/release"),
                Some(&Value::Object(body)),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, Some(&vm));
            }
            if let (Some(path), Some(lease_id)) = (leases::path(), lease_id.as_deref()) {
                if let Err(error) = leases::forget(&path, lease_id) {
                    eprintln!("machctl: warning: could not forget lease {lease_id}: {error}");
                }
            }
            println!("released {vm}");
        }

        Cmd::Gc => {
            let (status, resp) = client.call(
                "POST",
                "/gc",
                Some(&Value::Object(Map::new())),
                DEFAULT_TIMEOUT_S,
            );
            if status != 200 {
                die_from(status, &resp, None);
            }
            println!("gc done");
        }
    }
}

/// The `POST /acquire` body for a virtual machine of `image`: the flags
/// `vmctl acquire` had, checked as it checked them.
fn virtual_body(args: &AcquireArgs, image: &str) -> Map<String, Value> {
    if args.network == "control-only" && args.env != "none" {
        usage_error("control-only requires --env none");
    }
    let mut body = Map::new();
    body.insert("purpose".to_string(), Value::String(args.purpose.clone()));
    body.insert("image".to_string(), Value::String(image.to_string()));
    body.insert("env".to_string(), Value::String(args.env.clone()));
    body.insert("ttl_hours".to_string(), acquire_ttl_hours(args.ttl_hours));
    body.insert("network".to_string(), Value::String(args.network.clone()));
    if args.vnc {
        body.insert("vnc".to_string(), Value::Bool(true));
    }
    if args.source != "base" {
        if args.env != "none" {
            usage_error("--source work requires --env none");
        }
        body.insert("source".to_string(), Value::String(args.source.clone()));
    }
    if let Some(path) = args
        .expected_source_fingerprint
        .as_deref()
        .filter(|path| !path.is_empty())
    {
        if !body
            .get("source")
            .and_then(Value::as_str)
            .map(|source| source == "work")
            .unwrap_or(false)
        {
            usage_error("--expected-source-fingerprint requires --source work");
        }
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("machctl: {error}");
                std::process::exit(1);
            }
        };
        let fingerprint = match serde_json::from_str::<Value>(&text) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                eprintln!("machctl: {error}");
                std::process::exit(1);
            }
        };
        body.insert("expected_source_fingerprint".to_string(), fingerprint);
    }
    if let Some(profile) = &args.profile {
        body.insert("profile".to_string(), Value::String(profile.clone()));
    }
    for (key, value) in [
        ("cpu", args.cpu),
        ("memory_mb", args.memory_mb),
        ("disk_gb", args.disk_gb),
    ] {
        if let Some(value) = value {
            body.insert(key.to_string(), json_int(value));
        }
    }
    if args.no_wait {
        body.insert("wait".to_string(), Value::Bool(false));
    }
    body
}

/// The `POST /acquire` body for the durable machine `machine` (DM-2): no
/// image and none of a virtual machine's shaping fields.
fn durable_body(args: &AcquireArgs, machine: &str) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("purpose".to_string(), Value::String(args.purpose.clone()));
    body.insert("machine".to_string(), Value::String(machine.to_string()));
    body.insert("env".to_string(), Value::String(args.env.clone()));
    body.insert("ttl_hours".to_string(), acquire_ttl_hours(args.ttl_hours));
    body
}

/// The base URL of the instance `wanted` names (`--instance`, FD-2).
fn instance_base(settings: &federation::Settings, wanted: &str) -> String {
    let merged = federation::gather(settings, Some(wanted)).unwrap_or_else(|error| {
        eprintln!("machctl: {error}");
        std::process::exit(1);
    });
    for note in &merged.notes {
        eprintln!("machctl: note: {note}");
    }
    let owner = &merged.instances[0];
    if let Some(reason) = owner.unaskable.as_ref().or(owner.error.as_ref()) {
        eprintln!(
            "machctl: instance {} cannot be reached: {reason}",
            owner.label
        );
        std::process::exit(1);
    }
    owner.address.clone()
}

/// `machctl acquire --require` (CL-2, DC-69): list every instance, keep the
/// machines and offers with every capability, and lease the best one. The
/// choice is printed on standard error before the lease, and a candidate
/// refused for capacity passes to the next.
fn acquire_by_capability(
    args: &AcquireArgs,
    names: &[String],
    instance: Option<&str>,
    settings: &federation::Settings,
    consul_address: &str,
    environment: Option<&Value>,
) -> (u16, Value, String) {
    let merged = match federation::gather(settings, instance) {
        Ok(merged) => merged,
        Err(error) => {
            eprintln!("machctl: {error}");
            std::process::exit(1);
        }
    };
    if let Some(error) = &merged.consul_error {
        eprintln!(
            "machctl: warning: Consul at {consul_address} did not answer ({error}); choosing among the local instance only"
        );
    }
    browse_notes(&merged, "choosing among the local instance only");
    for note in &merged.notes {
        eprintln!("machctl: note: {note}");
    }
    let listing: Vec<(String, Vec<Value>)> = merged
        .instances
        .iter()
        .filter(|instance| instance.reachable)
        .map(|instance| (instance.label.clone(), instance.machines()))
        .collect();
    let candidates = capability::choose(&listing, names);
    if candidates.is_empty() {
        if merged.none_reachable() {
            eprintln!("machctl: no machine-service instance answered");
            std::process::exit(2);
        }
        eprintln!(
            "machctl: no machine is available with {} (see: machctl list --require ...)",
            names.join(", ")
        );
        std::process::exit(1);
    }
    let mut refusals = Vec::new();
    for candidate in &candidates {
        let Some(owner) = merged
            .instances
            .iter()
            .find(|instance| instance.label == candidate.instance)
        else {
            continue;
        };
        eprintln!("{}", candidate.reason);
        let body = if candidate.tier == 2 {
            virtual_body(args, &candidate.name)
        } else {
            durable_body(args, &candidate.name)
        };
        let client = Client::new(owner.address.clone(), environment);
        let (status, resp) = client.call(
            "POST",
            "/acquire",
            Some(&Value::Object(body)),
            DEFAULT_TIMEOUT_S,
        );
        if status == 200 {
            return (status, resp, owner.address.clone());
        }
        let error = error_message_text(&resp);
        if !acquire::capacity_refusal(status, &error) {
            return (status, resp, owner.address.clone());
        }
        eprintln!(
            "machctl: {} refused: {error}; trying the next candidate",
            candidate.name
        );
        refusals.push(format!("{}: {error}", candidate.name));
    }
    eprintln!(
        "machctl: every machine with {} refused for capacity ({})",
        names.join(", "),
        refusals.join("; ")
    );
    std::process::exit(1);
}

fn run_console(client: &Client<'_>, vm: &str, action: &str, body: Map<String, Value>) -> ! {
    let (status, resp) = client.call(
        "POST",
        &format!("/machines/{vm}/console/{action}"),
        Some(&Value::Object(body)),
        DEFAULT_TIMEOUT_S,
    );
    if status != 200 {
        die_from(status, &resp, Some(vm));
    }
    println!("{}", pretty(&resp));
    std::process::exit(0);
}

/// The machine a command acting under a lease names, with its
/// `--lease-id` (CL-3).
fn lease_argument(cmd: &Cmd) -> Option<(String, Option<String>)> {
    match cmd {
        Cmd::Exec(args) => Some((args.vm.clone(), args.lease_id.clone())),
        Cmd::Push { vm, lease_id, .. }
        | Cmd::Pull { vm, lease_id, .. }
        | Cmd::Heartbeat { vm, lease_id, .. }
        | Cmd::Release { vm, lease_id } => Some((vm.clone(), lease_id.clone())),
        _ => None,
    }
}

/// Add `lease_id` to a request body when one is known. A virtual machine
/// accepts it and does not require it; a durable machine requires it (MM-2).
fn insert_lease(body: &mut Map<String, Value>, lease_id: &Option<String>) {
    if let Some(lease_id) = lease_id {
        body.insert("lease_id".to_string(), Value::String(lease_id.clone()));
    }
}

/// Remember the lease an acquisition answered, for the commands that
/// follow (CL-3). A file that cannot be written is a warning: the lease
/// exists and `--lease-id` still reaches it.
fn remember_lease(resp: &Value, owner: &str) {
    let machine = resp
        .get("name")
        .or_else(|| resp.get("vm"))
        .and_then(Value::as_str);
    let lease_id = resp.get("lease_id").and_then(Value::as_str);
    let (Some(machine), Some(lease_id), Some(path)) = (machine, lease_id, leases::path()) else {
        return;
    };
    let lease = leases::Lease {
        machine: machine.to_string(),
        instance: owner.to_string(),
        lease_id: lease_id.to_string(),
        // MI-5/DC-90: older virtual acquisition replies omit the owner ID;
        // health supplies it without writing an operator label into the key.
        machine_id: resp
            .get("machine_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .or_else(|| {
                http::get_json(
                    &format!("{owner}/health"),
                    federation::INSTANCE_TIMEOUT,
                    &[],
                )
                .ok()
                .filter(|(status, _)| *status == 200)
                .and_then(|(_, health)| {
                    health
                        .get("machine_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                })
            }),
        granted_name: resp
            .get("granted_name")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    if let Err(error) = leases::remember(&path, lease) {
        eprintln!("machctl: warning: could not remember lease {lease_id}: {error}");
    }
}

/// The machine a lease command names (CL-3). Console commands stay with
/// the local instance: their viewer runs on the service host.
fn machine_argument(cmd: &mut Cmd) -> Option<&mut String> {
    match cmd {
        Cmd::Status { vm }
        | Cmd::Heartbeat { vm, .. }
        | Cmd::Release { vm, .. }
        | Cmd::Push { vm, .. }
        | Cmd::Pull { vm, .. } => Some(vm),
        Cmd::Wait(args) => Some(&mut args.vm),
        Cmd::Exec(args) => Some(&mut args.vm),
        _ => None,
    }
}

/// Reproduce Python's pre-argparse `exec` handling: split at the first `--`
/// only when `exec` is the subcommand (after an optional global
/// `--environment` prefix), so the guest command is passed through untouched.
fn split_exec_arguments(mut arguments: Vec<String>) -> (Vec<String>, Option<Vec<String>>) {
    let prefix = if arguments
        .first()
        .map(|a| a == "--environment")
        .unwrap_or(false)
    {
        2
    } else if arguments
        .first()
        .map(|a| a.starts_with("--environment="))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    if arguments.get(prefix).map(|a| a == "exec").unwrap_or(false) {
        if let Some(boundary) = arguments.iter().position(|a| a == "--") {
            let command = arguments[boundary + 1..].to_vec();
            arguments.truncate(boundary);
            return (arguments, Some(command));
        }
    }
    (arguments, None)
}

/// Print an `argparse`-style usage error and exit 2.
///
/// Every `p.error(...)` call in `bin/vmctl` uses the top-level parser, so the
/// preamble is always this exact usage line.
/// What a Bonjour browse leaves unsaid: a failed browse, or one that found
/// no other instance, is said on standard error with how many instances
/// answered, so an empty list never reads as a network without machines
/// (FD-3, DC-84).
fn browse_notes(merged: &federation::Merged, fallback: &str) {
    if let Some(error) = &merged.browse_error {
        eprintln!("machctl: warning: the Bonjour browse failed ({error}); {fallback}");
    } else if merged.browsed && merged.instances.iter().all(|instance| instance.local) {
        let answered = merged.instances.iter().filter(|i| i.reachable).count();
        eprintln!(
            "machctl: note: Bonjour found no other machine-service instance on the network within {} s; {answered} instance{} answered",
            federation::BROWSE_WINDOW.as_secs(),
            if answered == 1 { "" } else { "s" }
        );
    }
}

fn usage_error(message: &str) -> ! {
    eprintln!("usage: machctl [-h] [--environment ENVIRONMENT]");
    eprintln!(
        "             {{environment,acquire,wait,list,images,images-show,status,exec,push,pull,heartbeat,release,capabilities,acquisition-capabilities,console-resolve,console-open,console-cancel,gc}} ..."
    );
    eprintln!("machctl: error: {message}");
    std::process::exit(2);
}

/// Map service errors to actionable messages, matching `die_from`.
fn die_from(status: u16, resp: &Value, vm: Option<&str>) -> ! {
    eprintln!("{}", die_message(status, resp, vm));
    std::process::exit(1);
}

fn die_message(status: u16, resp: &Value, vm: Option<&str>) -> String {
    let error = error_message_text(resp);
    let mut msg = format!("machctl: {error}");
    if status == 409 && error.contains("already leased") {
        msg.push_str(
            " — release the existing lease (machctl list, machctl release <vm>) or pick another --purpose",
        );
    } else if status == 409 && error.contains("limit reached") {
        msg.push_str(" — free capacity: machctl list, then machctl release <vm>");
    } else if (status == 409 && error.contains("BASE-MISSING")) || error.contains("not built yet") {
        msg.push_str(" — build the golden base first (pilot-images host/build-base.zsh <image>)");
    } else if status == 404 && vm.is_some() {
        msg.push_str(" — check spelling: machctl list");
    }
    msg
}

/// `resp["error"] or resp`, rendered the way Python's f-string would.
fn error_message_text(resp: &Value) -> String {
    let error = match resp {
        Value::Object(map) => map.get("error").cloned(),
        other => Some(other.clone()),
    };
    let chosen = match &error {
        Some(value) if !is_falsy(value) => value.clone(),
        _ => resp.clone(),
    };
    python_str(&chosen)
}

/// Format a transport failure the way Python's `str(e.reason)` renders it.
fn error_reason(error: &ureq::Error) -> String {
    match error {
        ureq::Error::Io(io) => match io.raw_os_error() {
            Some(code) => {
                let text = io.to_string();
                let suffix = format!(" (os error {code})");
                let message = text.strip_suffix(&suffix).unwrap_or(&text);
                format!("[Errno {code}] {message}")
            }
            None => io.to_string(),
        },
        ureq::Error::Timeout(_) => "timed out".to_string(),
        other => other.to_string(),
    }
}

fn parse_response_body(text: &str) -> Value {
    if text.is_empty() {
        return Value::Object(Map::new());
    }
    match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("machctl: invalid JSON response: {error}");
            std::process::exit(1);
        }
    }
}

/// 2-space indented JSON with Python's default `ensure_ascii=True` escaping,
/// matching `json.dumps(obj, indent=2)`.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value)
        .map(|text| ensure_ascii(&text))
        .unwrap_or_else(|_| "null".to_string())
}

/// 2-space indented JSON preserving non-ASCII, matching
/// `json.dumps(obj, indent=2, ensure_ascii=False)` used by `machctl environment`.
fn pretty_unicode(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
}

/// Escape every non-ASCII code point as Python's `ensure_ascii=True` does.
///
/// JSON syntax is pure ASCII, so non-ASCII only ever appears inside string
/// literals and a flat scan is safe.
fn ensure_ascii(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        let code = character as u32;
        if code <= 0x7e {
            out.push(character);
        } else if code <= 0xFFFF {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            let offset = code - 0x10000;
            let high = 0xD800 + (offset >> 10);
            let low = 0xDC00 + (offset & 0x3FF);
            out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
        }
    }
    out
}

/// Render a JSON value the way Python `str()` would (`None`, `True`, `False`).
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python truthiness for the value kinds the CLI reads.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64().map(|n| n == 0.0).unwrap_or(false),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// JSON integer, matching a Python `int` field.
fn json_int(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

/// JSON float, matching a Python `float` field such as `--ttl-hours`.
fn json_float(value: f64) -> Value {
    Value::Number(
        serde_json::Number::from_f64(value).unwrap_or_else(|| serde_json::Number::from(0)),
    )
}

/// `machctl acquire --ttl-hours` value: `argparse` leaves the untyped default
/// `24` as a Python `int`, while an explicit flag is type-converted to `float`.
fn acquire_ttl_hours(value: Option<f64>) -> Value {
    match value {
        Some(value) => json_float(value),
        None => json_int(24),
    }
}

/// Human-readable `machctl images` table.
fn format_images(resp: &Value) -> String {
    let mut out = String::new();
    let Some(images) = resp.get("images").and_then(Value::as_object) else {
        return out;
    };
    for (name, image) in images {
        let concurrency = image.get("concurrency");
        let running = concurrency
            .and_then(|c| c.get("running"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let limit = concurrency
            .and_then(|c| c.get("limit"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let ok = if image
            .get("base_available")
            .map(|value| !is_falsy(value))
            .unwrap_or(false)
        {
            "ok"
        } else {
            "BASE-MISSING"
        };
        let cap = if limit != 0 {
            format!("{running}/{limit}")
        } else {
            format!("{running}")
        };
        let kind = image.get("kind").and_then(Value::as_str).unwrap_or("");
        let base_vm = image.get("base_vm").and_then(Value::as_str).unwrap_or("");
        let acquirable = concurrency
            .and_then(|c| c.get("acquirable"))
            .map(|value| python_str(value).to_lowercase())
            .unwrap_or_else(|| "none".to_string());
        out.push_str(&format!(
            "{name:<14} {kind:<6} {base_vm:<22} {ok:<13} running={cap:<5} acquirable={acquirable}\n"
        ));
    }
    out
}

/// The warning line printed above a listing while the instance's image
/// catalog is `stale` or `unavailable` (CL-1, IC-5): the failing file, the
/// error and the time of the failure, or that a rebuild is still running.
fn catalog_warning(resp: &Value) -> Option<String> {
    let catalog = resp.get("catalog")?;
    let status = catalog.get("status").and_then(Value::as_str)?;
    if status == "current" {
        return None;
    }
    let mut line = format!("machctl: warning: image catalog {status}");
    let error = catalog.get("error").and_then(Value::as_str);
    match error {
        Some(error) => {
            if let Some(failed_at) = catalog.get("failed_at").and_then(Value::as_f64) {
                line.push_str(&format!(" since {}", utc_timestamp(failed_at)));
            }
            match catalog.get("file").and_then(Value::as_str) {
                Some(file) => line.push_str(&format!(": {file}: {error}")),
                None => line.push_str(&format!(": {error}")),
            }
        }
        None if status == "stale" => {
            line.push_str(": the image files changed and the rebuild has not finished")
        }
        None => {}
    }
    if let Some(generation) = catalog.get("generation").and_then(Value::as_u64) {
        line.push_str(&format!(" (serving generation {generation})"));
    }
    Some(line)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_timestamp(seconds: f64) -> String {
    let time = seconds.floor() as libc::time_t;
    // SAFETY: `gmtime_r` writes into a caller-provided `tm` and reads only
    // `time`; both live for the call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&time, &mut tm).is_null() {
            return format!("{seconds}");
        }
        tm
    };
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

/// `machctl images-show` unknown-image diagnostic.
fn format_unknown_image(image: &str, resp: &Value) -> String {
    let available = resp
        .get("images")
        .and_then(Value::as_object)
        .map(|images| images.keys().cloned().collect::<Vec<String>>().join(", "))
        .unwrap_or_default();
    format!("machctl: unknown image '{image}' (available: {available})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn acquire_ttl_hours_matches_argparse_default_typing() {
        assert_eq!(
            serde_json::to_string(&acquire_ttl_hours(None)).unwrap(),
            "24"
        );
        assert_eq!(
            serde_json::to_string(&acquire_ttl_hours(Some(6.0))).unwrap(),
            "6.0"
        );
    }

    #[test]
    fn split_exec_arguments_honors_plain_prefix() {
        let (remaining, command) =
            split_exec_arguments(strings(&["exec", "vm", "--", "echo", "hi"]));
        assert_eq!(remaining, strings(&["exec", "vm"]));
        assert_eq!(command, Some(strings(&["echo", "hi"])));
    }

    #[test]
    fn split_exec_arguments_honors_environment_prefix() {
        let (remaining, command) = split_exec_arguments(strings(&[
            "--environment",
            "profile.json",
            "exec",
            "vm",
            "--timeout",
            "5",
            "--",
            "echo",
            "hi",
        ]));
        assert_eq!(
            remaining,
            strings(&[
                "--environment",
                "profile.json",
                "exec",
                "vm",
                "--timeout",
                "5"
            ])
        );
        assert_eq!(command, Some(strings(&["echo", "hi"])));
    }

    #[test]
    fn split_exec_arguments_honors_inline_environment_prefix() {
        let (remaining, command) = split_exec_arguments(strings(&[
            "--environment=profile.json",
            "exec",
            "vm",
            "--",
            "ls",
        ]));
        assert_eq!(
            remaining,
            strings(&["--environment=profile.json", "exec", "vm"])
        );
        assert_eq!(command, Some(strings(&["ls"])));
    }

    #[test]
    fn split_exec_arguments_leaves_non_exec_untouched() {
        let arguments = strings(&["acquire", "--purpose", "x", "--", "trailing"]);
        let (remaining, command) = split_exec_arguments(arguments.clone());
        assert_eq!(remaining, arguments);
        assert_eq!(command, None);
    }

    #[test]
    fn acquire_defaults_match_python() {
        let cli = Cli::try_parse_from(["machctl", "acquire", "--purpose", "demo"]).unwrap();
        match cli.cmd {
            Cmd::Acquire(args) => {
                assert_eq!(args.purpose, "demo");
                assert_eq!(args.image, None);
                assert_eq!(args.machine, None);
                assert!(args.require.is_empty());
                assert_eq!(args.env, "default");
                assert_eq!(args.network, "nat");
                assert_eq!(args.profile, None);
                assert_eq!(args.ttl_hours, None);
                assert_eq!(args.source, "base");
                assert!(!args.no_wait);
                assert!(!args.vnc);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn acquire_deprecated_aliases_parse() {
        let cli = Cli::try_parse_from([
            "machctl",
            "acquire",
            "--purpose",
            "demo",
            "--line",
            "ubuntu2404",
            "--pack",
            "none",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Acquire(args) => {
                assert_eq!(args.image.as_deref(), Some("ubuntu2404"));
                assert_eq!(args.env, "none");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn exec_defaults_and_separator() {
        let cli = Cli::try_parse_from(["machctl", "exec", "vm1", "--", "echo", "hi"]).unwrap();
        match cli.cmd {
            Cmd::Exec(args) => {
                assert_eq!(args.vm, "vm1");
                assert_eq!(args.script, None);
                assert_eq!(args.timeout, 600);
                assert_eq!(args.argv, strings(&["echo", "hi"]));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn wait_timeout_default_and_heartbeat_optional() {
        let cli = Cli::try_parse_from(["machctl", "wait", "vm1"]).unwrap();
        match cli.cmd {
            Cmd::Wait(args) => assert_eq!(args.timeout_s, 900),
            other => panic!("unexpected command: {other:?}"),
        }
        let cli = Cli::try_parse_from(["machctl", "heartbeat", "vm1"]).unwrap();
        match cli.cmd {
            Cmd::Heartbeat { ttl_hours, .. } => assert_eq!(ttl_hours, None),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn console_actions_require_lease_and_ids() {
        let missing = Cli::try_parse_from(["machctl", "console-open", "vm1", "--lease-id", "l"]);
        assert!(missing.is_err());
        let cli = Cli::try_parse_from([
            "machctl",
            "console-open",
            "vm1",
            "--lease-id",
            "l",
            "--console-id",
            "c",
            "--attempt-id",
            "a",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::ConsoleOpen(args) => {
                assert_eq!(args.vm, "vm1");
                assert_eq!(args.lease_id, "l");
                assert_eq!(args.console_id, "c");
                assert_eq!(args.attempt_id, "a");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn pretty_escapes_non_ascii_like_python() {
        assert_eq!(pretty(&json!("caf\u{00e9}")), "\"caf\\u00e9\"");
        assert_eq!(pretty(&json!("\u{1f600}")), "\"\\ud83d\\ude00\"");
        assert_eq!(pretty_unicode(&json!("caf\u{00e9}")), "\"caf\u{00e9}\"");
    }

    #[test]
    fn pretty_preserves_insertion_order() {
        let value = json!({"b": 1, "a": 2});
        assert_eq!(pretty(&value), "{\n  \"b\": 1,\n  \"a\": 2\n}");
    }

    #[test]
    fn pretty_matches_python_empty_container_shapes() {
        assert_eq!(pretty(&json!({})), "{}");
        assert_eq!(pretty(&json!([])), "[]");
    }

    #[test]
    fn images_table_matches_python_format() {
        let resp = json!({
            "images": {
                "macos26": {
                    "kind": "macos",
                    "base_vm": "pilot-base",
                    "base_available": true,
                    "concurrency": {"running": 1, "limit": 2, "acquirable": true}
                }
            }
        });
        assert_eq!(
            format_images(&resp),
            "macos26        macos  pilot-base             ok            running=1/2   acquirable=true\n"
        );
    }

    #[test]
    fn images_table_uses_running_only_without_limit() {
        let resp = json!({
            "images": {
                "ubuntu2404": {
                    "kind": "linux",
                    "base_vm": "pilot-ubuntu-base",
                    "base_available": false,
                    "concurrency": {"running": 3, "limit": 0, "acquirable": false}
                }
            }
        });
        let line = format_images(&resp);
        assert!(line.contains("BASE-MISSING"));
        assert!(line.contains("running=3    "));
        assert!(line.contains("acquirable=false"));
    }

    /// CL-1, AC-41: a stale catalog's warning names the file, the error,
    /// the failure time and the generation still served.
    #[test]
    fn ac41_catalog_warning_names_file_error_and_time() {
        let current = json!({"catalog": {"generation": 3, "built_at": 1.0, "status": "current"}});
        assert_eq!(catalog_warning(&current), None);
        assert_eq!(catalog_warning(&json!({"machines": {}})), None);

        let stale = json!({"catalog": {
            "generation": 7, "built_at": 1.0, "status": "stale",
            "error": "line configuration parse failed (zsh exit status 1)",
            "failed_at": 1_792_540_800.5,
            "file": "/pilot/images/macos26/line.conf",
        }});
        assert_eq!(
            catalog_warning(&stale).expect("warning"),
            "machctl: warning: image catalog stale since 2026-10-21T00:00:00Z: \
             /pilot/images/macos26/line.conf: line configuration parse failed \
             (zsh exit status 1) (serving generation 7)"
        );

        let rebuilding = json!({"catalog": {"generation": 2, "built_at": 1.0, "status": "stale"}});
        assert_eq!(
            catalog_warning(&rebuilding).expect("warning"),
            "machctl: warning: image catalog stale: the image files changed and the \
             rebuild has not finished (serving generation 2)"
        );

        let unavailable = json!({"catalog": {
            "generation": null, "built_at": null, "status": "unavailable",
            "error": "no configured image lines", "failed_at": 0.0, "file": "/pilot/images",
        }});
        assert_eq!(
            catalog_warning(&unavailable).expect("warning"),
            "machctl: warning: image catalog unavailable since 1970-01-01T00:00:00Z: \
             /pilot/images: no configured image lines"
        );
    }

    #[test]
    fn unknown_image_message_lists_available_keys() {
        let resp = json!({"images": {"macos26": {}, "ubuntu2404": {}}});
        assert_eq!(
            format_unknown_image("nope", &resp),
            "machctl: unknown image 'nope' (available: macos26, ubuntu2404)"
        );
    }

    #[test]
    fn die_messages_add_actionable_hints() {
        let already = die_message(
            409,
            &json!({"error": "purpose 'x' already leased on image y"}),
            None,
        );
        assert!(already.contains("already leased"));
        assert!(already.contains("machctl release"));

        let limit = die_message(
            409,
            &json!({"error": "macOS VM limit reached (2 active)"}),
            None,
        );
        assert!(limit.contains("limit reached"));
        assert!(limit.contains("free capacity"));

        let missing = die_message(409, &json!({"error": "BASE-MISSING"}), None);
        assert!(missing.contains("build the golden base first"));

        let not_built = die_message(500, &json!({"error": "base not built yet"}), None);
        assert!(not_built.contains("build the golden base first"));

        let unknown = die_message(404, &json!({"error": "unknown VM"}), Some("ghost"));
        assert!(unknown.contains("machctl list"));

        let plain = die_message(500, &json!({"error": "boom"}), None);
        assert_eq!(plain, "machctl: boom");
    }

    #[test]
    fn python_strings_match_python_spelling() {
        assert_eq!(python_str(&Value::Null), "None");
        assert_eq!(python_str(&json!(true)), "True");
        assert_eq!(python_str(&json!(false)), "False");
        assert_eq!(python_str(&json!(24)), "24");
        assert_eq!(python_str(&json!(1.5)), "1.5");
        assert_eq!(python_str(&json!("text")), "text");
    }

    #[test]
    fn json_helpers_keep_python_integer_and_float_distinction() {
        assert_eq!(json_int(6), json!(6));
        assert_eq!(json_float(24.0), json!(24.0));
        assert_eq!(json_float(1.5), json!(1.5));
        assert_eq!(serde_json::to_string(&json_float(24.0)).unwrap(), "24.0");
        assert_eq!(serde_json::to_string(&json_int(24)).unwrap(), "24");
    }
}
