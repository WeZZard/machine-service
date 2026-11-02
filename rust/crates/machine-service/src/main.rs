//! The `machine-service` daemon: HTTP API, lease state, and the GC loop.

use std::io::Write;
use std::sync::Arc;

use serde_json::{json, Map, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use machine_service_core::config::{Config, GuestLink};
use machine_service_core::error::{OpError, OpResult};
use machine_service_core::service::Service;
use machine_service_core::{
    acquisition_options, application_catalog, control_only, environment, snapshot,
};

mod durable_routes;
mod files;
mod name_watch;
mod probe;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("--probe-host") {
        probe::run();
        return;
    }
    // A durable lease's command, started by the relay agent (DM-4, M4).
    if arguments.first().map(String::as_str)
        == Some(machine_service_core::durable::launch::ARGUMENT)
    {
        let request = arguments.get(1).map(String::as_str).unwrap_or_default();
        std::process::exit(machine_service_core::durable::launch::run(
            std::path::Path::new(request),
        ));
    }
    if let Err(message) = run(&arguments) {
        eprintln!("machine-service: {message}");
        std::process::exit(1);
    }
}

fn run(arguments: &[String]) -> Result<(), String> {
    // A `VM_SERVICE_*` variable is a setting this daemon would silently
    // ignore after the rename, so it stops here and names the fix (AC-36,
    // DC-37). `vars_os` keeps a non-UTF-8 value from panicking the check.
    environment::renamed::refuse_legacy_variables(
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
    )?;
    let mut environment_path: Option<String> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--environment" => {
                index += 1;
                environment_path = arguments.get(index).cloned();
            }
            other if other.starts_with("--environment=") => {
                environment_path = Some(other["--environment=".len()..].to_string());
            }
            _ => {}
        }
        index += 1;
    }

    let bundle = environment::load_environment(environment_path.as_deref())
        .map_err(|error| error.to_string())?;
    let mut config = match &bundle {
        Some(bundle) => Config::from_selected(bundle),
        None => Config::legacy(),
    };
    config.guest_link = GuestLink::from_environment()?;
    // An unknown mode stops the daemon before it serves (FD-1, DC-84).
    let discovery_mode = discovery::Mode::from_environment()?;

    if bundle.is_none() {
        // A selected startup binds a store/state; legacy startup must not
        // reinterpret it.
        let legacy_store = std::env::var("TART_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("/"))
                    .join(".tart")
            });
        let roots = [config.state_dir.clone(), legacy_store];
        if roots
            .iter()
            .any(|root| root.join(environment::MARKER).exists())
        {
            return Err("selected environment marker requires an explicit profile".to_string());
        }
    }

    let _ownership = match &bundle {
        Some(bundle) => Some(environment::ownership(bundle).map_err(|error| error.to_string())?),
        None => None,
    };

    serve(config, discovery_mode)
}

fn serve(config: Config, discovery_mode: discovery::Mode) -> Result<(), String> {
    let console_config = Service::load_console_config().map_err(|error| error.to_string())?;
    let selected = config.environment.is_some();
    let service = Arc::new(Service::new(config, console_config));
    std::fs::create_dir_all(&service.config.state_dir).map_err(|error| error.to_string())?;

    // Legacy mode takes the daemon lock explicitly; a selected environment
    // already holds it through `environment::ownership`.
    if !selected {
        let lock_path = service.config.state_dir.join("daemon.lock");
        let descriptor = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        use std::os::fd::AsRawFd;
        // SAFETY: `descriptor` is open for the process lifetime; the flock is
        // released by the kernel on exit.
        let locked =
            unsafe { libc::flock(descriptor.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            return Err("Another machine-service daemon owns this state directory".to_string());
        }
        std::mem::forget(descriptor);
    }

    // The image catalog executes `line.conf` files only from here on, in the
    // process that owns `daemon.lock` (IC-4). A legacy startup whose files
    // are broken serves with the catalog `unavailable`, and the builder has
    // already logged the failure; a selected environment refuses to start
    // (IC-5).
    if let Err(error) = service.initialize_image_catalog() {
        if selected {
            return Err(format!("selected image configuration is invalid: {error}"));
        }
    }
    // Starts rebuilds early; correctness never depends on it (IC-3, DC-38).
    let _watcher = match machine_service_core::catalog::watch::watch_catalog(&service.catalog) {
        Ok(watcher) => watcher,
        Err(error) => {
            service.log(&format!("WARN: image catalog watcher not started: {error}"));
            None
        }
    };

    // No acquisition survives a restart; release the records it left behind
    // before any request can observe or retry them.
    if let Err(error) = service.reconcile_startup() {
        service.log(&format!("WARN: startup reconciliation failed: {error}"));
    }
    durable_routes::install(&service)?;
    if service.config.guest_link == GuestLink::Relay {
        start_relay(&service)?;
    }

    let address = if service.config.host.contains(':') {
        format!("[{}]:{}", service.config.host, service.config.port)
    } else {
        format!("{}:{}", service.config.host, service.config.port)
    };
    let server = Server::http(&address).map_err(|error| error.to_string())?;
    service.log(&format!(
        "machine-service starting on {}:{} (image source: {}, guest link: {}, discovery: {})",
        service.config.host,
        service.config.port,
        service
            .config
            .pilot
            .as_ref()
            .map(|pilot| pilot.display().to_string())
            .unwrap_or_else(|| "none".to_string()),
        service.config.guest_link.name(),
        discovery_mode.name()
    ));
    // Held while the daemon serves; the system withdraws it on exit (FD-1).
    let _name_watcher = name_watch::start(service.clone(), discovery_mode)?;

    let gc_service = service.clone();
    std::thread::spawn(move || gc_service.gc_loop());

    for request in server.incoming_requests() {
        let service = service.clone();
        std::thread::spawn(move || handle(&service, request));
    }
    service.consoles.shutdown();
    Ok(())
}

/// Start the broker on the session listener and install the relay link
/// (`docs/arch/broker.md`, BK-1). A listener that cannot start stops the
/// daemon: an instance told to serve the relay link must not silently serve
/// SSH instead.
fn start_relay(service: &Arc<Service>) -> Result<(), String> {
    use machine_service_core::relay::{Relay, RelaySettings};
    let settings = RelaySettings::from_environment()?;
    let log_file = service.config.log_file.clone();
    let log = Arc::new(move |message: &str| {
        machine_service_core::config::log_to_file(&log_file, message)
    });
    let relay = Relay::start(settings, &service.config.state_dir, log)?;
    durable_routes::install_relay(&relay, &service.config.state_dir);
    service
        .install_relay(relay)
        .map_err(|error| error.to_string())?;
    service.watch_relay_agents();
    Ok(())
}

/// `virtual` needs Tart as well as a catalog: a Mac without Tart offers only
/// its durable machine (MM-1, AC-38, DC-66).
fn offers_virtual(service: &Service) -> bool {
    service.catalog.peek().status.freshness != machine_service_core::catalog::Freshness::Unavailable
        && service.host.tart_executable().is_ok()
}

/// Advertise `_machine-service._tcp` in Bonjour mode, named by the instance
/// ID, with `instance` and `kinds` in its TXT record. An instance bound to
/// loopback, or in Consul mode, advertises nothing and says why (FD-1, DC-85).
fn advertise(service: &Service, mode: discovery::Mode) -> Option<discovery::dnssd::Advertisement> {
    if mode == discovery::Mode::Consul {
        service.log("discovery: consul mode; not advertising on the network");
        return None;
    }
    if discovery::is_loopback_host(&service.config.host) {
        service.log(&format!(
            "discovery: bound to loopback ({}), so no other Mac could reach this instance; not advertising",
            service.config.host
        ));
        return None;
    }
    let (instance, machine_id, kinds) = durable_routes::identity(offers_virtual(service))?;
    let mut txt = discovery::txt_entries(&instance, &machine_id, &kinds);
    if let Some(relay) = service.relay() {
        let fields = relay.discovery();
        txt.extend(discovery::relay_txt_entries(&fields[0].1, &fields[1].1));
    }
    match discovery::dnssd::register(
        &instance,
        discovery::SERVICE_TYPE,
        service.config.port,
        &txt,
        discovery::dnssd::Scope::from_environment().ok()?,
        std::time::Duration::from_secs(5),
    ) {
        Ok(advertisement) => {
            service.log(&format!(
                "discovery: advertising {} as {:?} on port {} (kinds: {})",
                discovery::SERVICE_TYPE,
                advertisement.name,
                service.config.port,
                kinds.join(",")
            ));
            Some(advertisement)
        }
        Err(error) => {
            service.log(&format!("WARN: discovery: not advertising: {error}"));
            None
        }
    }
}

fn handle(service: &Service, mut request: Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    if let Some(body) = removed_route(&url) {
        let _ = send(request, 410, &body);
        return;
    }
    // The byte-carrying endpoints stream their bodies, so they are answered
    // before any JSON body is read (FD-4).
    if let Some(name) = files::machine_of(&url) {
        files::handle(service, request, &name);
        return;
    }
    if method == Method::Get && url == "/relay/ca.pem" {
        relay_authority(service, request);
        return;
    }
    let response = match method {
        Method::Get => handle_get(service, &mut request, &url),
        Method::Post => handle_post(service, &mut request, &url),
        _ => {
            let _ = send(request, 404, &json!({"error": "not found"}));
            return;
        }
    };
    let acquire = method == Method::Post && url == "/acquire";
    match response {
        Ok((code, value)) => {
            let delivered = send(request, code, &value);
            if let Err(error) = delivered {
                if acquire && code == 200 {
                    release_undelivered_acquire(service, &value, &error);
                }
            }
        }
        Err(error) => {
            let mut body = json!({"error": error.to_string()});
            if error.uncertain {
                body["outcome"] = json!("uncertain");
            }
            let _ = send(request, 409, &body);
        }
    }
}

/// `GET /relay/ca.pem`: the broker's authority, which a client trusts after
/// checking it against `relay_ca_sha256` (RP-1).
fn relay_authority(service: &Service, request: Request) {
    let Some(relay) = service.relay() else {
        let _ = send(
            request,
            404,
            &json!({"error": "this instance runs no broker"}),
        );
        return;
    };
    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"application/x-pem-file"[..])
        .expect("static header");
    let response = Response::from_string(relay.authority.cert_pem())
        .with_status_code(200)
        .with_header(content_type);
    let _ = request.respond(response);
}

/// The answer to a request on a removed `/vms` path: HTTP 410 Gone, naming
/// the `/machines` path that replaces it (AC-35, DC-37, MG-2).
///
/// There is no alias: a missed caller fails at once with its fix in the
/// error, and the request reaches no handler, so no operation runs. Any
/// method and any query string is answered the same way.
fn removed_route(url: &str) -> Option<Value> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let rest = path.strip_prefix("/vms")?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let replacement = format!("/machines{rest}");
    Some(json!({
        "error": format!("{path} was removed; use {replacement}"),
        "replacement": replacement,
    }))
}

/// Release a lease whose successful acquisition response could not be
/// delivered: no client knows about it, so it would otherwise hold its slot
/// until TTL + grace. See `docs/lifecycle-fixes.md` (V3).
fn release_undelivered_acquire(service: &Service, record: &Value, error: &std::io::Error) {
    let Some(vm) = record.get("vm").and_then(Value::as_str) else {
        return;
    };
    service.log(&format!(
        "WARN: acquire response for {vm} undelivered ({error}); releasing the new lease"
    ));
    if let Err(release_error) = service.release(vm, "acquire-response-undelivered", false) {
        service.log(&format!(
            "WARN: release of undelivered lease {vm} failed: {release_error}"
        ));
    }
}

fn supplied_fingerprint(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("X-VM-Environment-Fingerprint"))
        .map(|header| header.value.as_str().to_string())
}

fn environment_allowed(service: &Service, supplied: &Option<String>, mutating: bool) -> bool {
    let expected = service.config.fingerprint();
    if let Some(supplied) = supplied {
        if Some(supplied.as_str()) != expected.as_deref() {
            return false;
        }
    } else if mutating && expected.is_some() {
        return false;
    }
    true
}

fn handle_get(service: &Service, request: &mut Request, url: &str) -> OpResult<(u16, Value)> {
    let supplied = supplied_fingerprint(request);
    if !environment_allowed(service, &supplied, false) {
        return Ok((
            409,
            json!({"error": "environment fingerprint mismatch; refusing request"}),
        ));
    }
    // CPython compares the raw `BaseHTTPRequestHandler.self.path`, which still
    // carries any query string, so a path with a query matches no route and
    // falls through to the generic 404. Do not strip the query here.
    let path = url;
    if let Some(answer) = durable_routes::get(path) {
        return Ok(answer);
    }
    if path == "/" {
        return Ok((
            200,
            json!({
                "service": "machine-service",
                "endpoints": ["/health", "/images", "/applications", "/machines", "/machines/{name}"],
            }),
        ));
    }
    if path == "/health" {
        let mut body = json!({"ok": true, "service": "machine-service"});
        // A health check never waits for a rebuild; a changed file reads as
        // `stale` until the rebuild this read starts has finished (IC-5).
        let catalog = service.catalog.peek();
        body["catalog"] = catalog.status.to_json();
        if let Some(environment) = service
            .config
            .environment
            .as_ref()
            .and_then(|bundle| bundle.get("identity"))
        {
            body["environment"] = environment.clone();
        }
        durable_routes::health(&mut body, offers_virtual(service));
        if let Some(relay) = service.relay() {
            for (key, value) in relay.discovery() {
                body[key] = json!(value);
            }
        }
        return Ok((200, body));
    }
    if path == "/capabilities" {
        return Ok((200, control_only::descriptor()));
    }
    if path == "/acquisition-capabilities" {
        // The descriptor's shape is closed, so it carries no catalog status.
        let view = service.image_catalog();
        let images = view.snapshot().map(|snapshot| {
            Value::Array(
                snapshot
                    .lines
                    .iter()
                    .map(|(image, line)| json!({"image": image, "os": line.kind}))
                    .collect(),
            )
        });
        let capabilities = service.consoles.capabilities();
        // A stat of the Softnet executable Tart would run and a read of the
        // guest firewall's anchor file; no process starts.
        let internet = match service
            .host
            .softnet()
            .and_then(|_| service.host.guest_firewall().map(|_| ()))
        {
            Ok(_) => acquisition_options::InternetAvailability::Available,
            Err(reason) => acquisition_options::InternetAvailability::Unavailable(reason),
        };
        return Ok((
            200,
            acquisition_options::descriptor(&capabilities, images.as_ref(), &internet),
        ));
    }
    if path == "/images" {
        let view = service.image_catalog();
        if view.snapshot().is_none() {
            return Ok((503, catalog_unavailable(&view)));
        }
        return Ok((200, snapshot::images_snapshot(service, &view)?));
    }
    if path == "/applications" {
        // The catalog's shape is closed, so a failure answers 503 with the
        // diagnostic alone (IC-5).
        let associations = match service.application_associations() {
            Ok(associations) => associations,
            Err(error) => return Ok((503, json!({"error": error.to_string()}))),
        };
        // No image source, no images, so no applications (IC-7).
        let Some(pilot) = &service.config.pilot else {
            return Ok((200, json!({"schemaVersion": 1, "images": []})));
        };
        let log = |message: &str| service.log(message);
        match application_catalog::load_catalog(
            pilot,
            &Value::Object(associations),
            None,
            Some(&log),
        ) {
            Ok(catalog) => return Ok((200, catalog)),
            Err(error) => return Ok((503, json!({"error": error.to_string()}))),
        }
    }
    if path == "/machines" {
        let mut body = snapshot::snapshot(service)?;
        body["catalog"] = service.image_catalog().status.to_json();
        snapshot::add_offers(service, &mut body);
        durable_routes::extend_machines(&mut body);
        return Ok((200, body));
    }
    if let Some(name) = path.strip_prefix("/machines/") {
        if !name.is_empty() && !name.contains('/') {
            let mut machines = snapshot::snapshot(service)?;
            // MI-1 / DC-87 applies to individual machine records too.
            durable_routes::extend_machines(&mut machines);
            if let Some(record) = machines
                .get("machines")
                .and_then(|machines| machines.get(name))
            {
                return Ok((200, record.clone()));
            }
            return Ok((404, json!({"error": "unknown VM"})));
        }
    }
    Ok((404, json!({"error": "not found"})))
}

fn handle_post(service: &Service, request: &mut Request, url: &str) -> OpResult<(u16, Value)> {
    let supplied = supplied_fingerprint(request);
    if !environment_allowed(service, &supplied, true) {
        return Ok((
            409,
            json!({"error": "environment fingerprint mismatch; refusing request"}),
        ));
    }
    let body = match read_body(request) {
        Ok(body) => body,
        Err(error) => return Ok((409, json!({"error": error.to_string()}))),
    };
    // CPython compares the raw `BaseHTTPRequestHandler.self.path`, which still
    // carries any query string, so a path with a query matches no route and
    // falls through to the generic 404. Do not strip the query here.
    let path = url;
    let loopback = request
        .remote_addr()
        .is_some_and(|address| address.ip().is_loopback());
    if let Some(answer) = durable_routes::post(path, &body, loopback) {
        return Ok(answer);
    }

    if path == "/acquire" {
        // A Mac without Tart cannot create the virtual machine an image asks
        // for; the request is refused before anything is reserved (AC-38).
        if let Err(reason) = service.host.tart_executable() {
            return Ok((
                400,
                json!({"error": format!("this instance cannot create virtual machines: {reason}")}),
            ));
        }
        machine_service_core::network::validate_request(&body)?;
        let Some(purpose) = body.get("purpose").and_then(Value::as_str) else {
            return Ok((409, json!({"error": "'purpose' (string) is required"})));
        };
        let image =
            string_field(&body, &["image", "line"]).unwrap_or_else(|| "macos26".to_string());
        let env = string_field(&body, &["env", "pack", "lane"])
            .unwrap_or_else(|| machine_service_core::packs::DEFAULT_PACK.to_string());
        let ttl = body.get("ttl_hours").cloned().unwrap_or_else(|| json!(24));
        check_option_integers(&body)?;
        let record = service.acquire(
            purpose,
            &image,
            &env,
            &ttl,
            body.get("cpu").and_then(Value::as_i64),
            body.get("memory_mb").and_then(Value::as_i64),
            body.get("disk_gb").and_then(Value::as_i64),
            body.get("wait").and_then(Value::as_bool).unwrap_or(true),
            body.get("network").and_then(Value::as_str).unwrap_or("nat"),
            body.get("profile").and_then(Value::as_str),
            body.get("source").and_then(Value::as_str).unwrap_or("base"),
            body.get("expected_source_fingerprint"),
            body.get("vnc").and_then(Value::as_bool).unwrap_or(false),
        )?;
        return Ok((200, record));
    }

    if let Some(rest) = path.strip_prefix("/machines/") {
        let mut segments = rest.split('/');
        let vm = segments.next().unwrap_or_default().to_string();
        let action = segments.next().unwrap_or_default().to_string();
        let tail = segments.next();
        if action == "console" {
            let operation = tail.ok_or_else(|| OpError::new("not found"))?;
            return handle_console(service, &vm, operation, &body);
        }
        match action.as_str() {
            "release" => {
                let reason = body
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("requested");
                let result = service.release(&vm, reason, false)?;
                return Ok((200, result));
            }
            "heartbeat" => {
                let result = service.heartbeat(&vm, body.get("ttl_hours"))?;
                return Ok((200, result));
            }
            "exec" => return Ok((200, service.guest_exec(&vm, &body)?)),
            // A host path means nothing on another Mac, and from another
            // Mac it would name this host's files: path-based transfers
            // serve loopback clients only; others use the byte-carrying
            // endpoints (FD-4, DC-70).
            "push" | "pull" if !loopback => {
                return Ok((
                    409,
                    json!({"error": "path-based push and pull need a loopback client; a host path means nothing on another Mac (use PUT or GET /machines/<name>/files)"}),
                ));
            }
            "push" => return Ok((200, service.guest_push(&vm, &body)?)),
            "pull" => return Ok((200, service.guest_pull(&vm, &body)?)),
            _ => {}
        }
    }

    if path == "/images/reload" {
        // Rebuild even when the files did not change, and report the result
        // (IC-3).
        let view = service.catalog.reload();
        if view.snapshot().is_none() {
            return Ok((503, catalog_unavailable(&view)));
        }
        return Ok((200, json!({"catalog": view.status.to_json()})));
    }

    if path == "/gc" {
        service.gc_once()?;
        durable_routes::gc_once();
        return Ok((200, json!({"gc": "done"})));
    }
    Ok((404, json!({"error": "not found"})))
}

/// The HTTP 503 body of a catalog request while no snapshot exists (IC-5).
fn catalog_unavailable(view: &machine_service_core::catalog::CatalogView) -> Value {
    json!({"error": view.status.diagnostic(), "catalog": view.status.to_json()})
}

fn handle_console(
    service: &Service,
    vm: &str,
    operation: &str,
    body: &Value,
) -> OpResult<(u16, Value)> {
    let required: Vec<&str> = match operation {
        "resolve" => vec!["lease_id"],
        "open" | "cancel" => vec!["lease_id", "console_id", "attempt_id"],
        // The Python route regex accepts only `resolve`, `open`, and `cancel`
        // before the body is examined, so any other operation is unroutable.
        _ => return Ok((404, json!({"error": "not found"}))),
    };
    let object = match body.as_object() {
        Some(object)
            if object.len() == required.len()
                && required.iter().all(|key| object.contains_key(*key)) =>
        {
            object
        }
        _ => {
            return Err(OpError::new("Invalid console request identity or fields"));
        }
    };
    if !required.iter().all(|key| {
        object
            .get(*key)
            .map(machine_service_core::console_api::identity)
            .unwrap_or(false)
    }) {
        return Err(OpError::new("Invalid console request identity or fields"));
    }
    let record = service.get_record(vm)?;
    let record_map: Map<String, Value> = record
        .as_object()
        .cloned()
        .ok_or_else(|| OpError::new("lease record is not an object"))?;
    let lease_id = object["lease_id"].as_str().unwrap_or_default().to_string();
    let result = match operation {
        "resolve" => {
            let resolved = service
                .consoles
                .resolve(&record_map, Some(&lease_id))
                .map_err(|error| OpError::new(error.to_string()))?;
            Value::Object(resolved)
        }
        "open" => service
            .consoles
            .open(
                &record_map,
                &lease_id,
                object["console_id"].as_str().unwrap_or_default(),
                object["attempt_id"].as_str().unwrap_or_default(),
            )
            .map_err(|error| OpError::new(error.to_string()))?,
        "cancel" => service
            .consoles
            .cancel(
                &record_map,
                &lease_id,
                object["console_id"].as_str().unwrap_or_default(),
                object["attempt_id"].as_str().unwrap_or_default(),
            )
            .map_err(|error| OpError::new(error.to_string()))?,
        _ => return Ok((404, json!({"error": "not found"}))),
    };
    Ok((200, result))
}

fn check_option_integers(body: &Value) -> OpResult<()> {
    for name in ["cpu", "memory_mb", "disk_gb"] {
        match body.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {}
            Some(_) => {
                return Err(OpError::new(format!(
                    "{name} must be a positive integer or null"
                )));
            }
        }
    }
    Ok(())
}

fn string_field(body: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| body.get(*name).and_then(Value::as_str).map(str::to_string))
}

fn read_body(request: &mut Request) -> OpResult<Value> {
    let length: usize = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Content-Length"))
        .and_then(|header| header.value.as_str().parse().ok())
        .unwrap_or(0);
    if length == 0 {
        return Ok(json!({}));
    }
    let mut bytes = vec![0u8; length];
    request
        .as_reader()
        .read_exact(&mut bytes)
        .map_err(|error| OpError::new(format!("invalid JSON body: {error}")))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    let mut parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|error| OpError::new(format!("invalid JSON body: {error}")))?;
    // CPython's `json` normalizes float literals when it loads them.
    machine_service_core::python_json::normalize_numbers(&mut parsed);
    Ok(parsed)
}

/// Write one JSON response, returning whether it reached the client's socket.
///
/// `Request::respond` reports a reset or broken connection as success, so the
/// response is written through the request's raw writer instead; that is the
/// same serialization `respond` performs, without discarding the error.
fn send(request: Request, code: u16, value: &Value) -> std::io::Result<()> {
    let mut body = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    body.push('\n');
    let content_type =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(content_type);
    let version = request.http_version().clone();
    let headers = request.headers().to_vec();
    let head = request.method() == &Method::Head;
    let mut writer = request.into_writer();
    response.raw_print(&mut writer, version, &headers, head, None)?;
    writer.flush()
}
