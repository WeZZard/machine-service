//! The host's lab instance, kept running and current by `lab-host`, and the
//! lab's endpoints, which runner VMs may reach (LR-6, DC-1xx-EPH-2,
//! DC-1xx-EPH-12).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// The lab instance's second-instance ID, so its clones are `si-lab-…`.
pub const INSTANCE_ID: &str = "lab";
/// The lab instance's HTTP port.
pub const HTTP_PORT: u16 = guest_firewall::LAB_PORTS[0];
/// The lab instance's broker port.
pub const BROKER_PORT: u16 = guest_firewall::LAB_PORTS[1];
/// The lab's state folder under the account's home.
pub const STATE: &str = ".local/state/vm-service-lab";
/// The lab's bundles under the account's home; `current` names the one in use.
pub const BUNDLES: &str = ".local/libexec/vm-service-lab";
/// The durable cells a durable Mac's lab instance exposes its Mac for.
pub const DURABLE_CELLS: &str = "macos-client.durable-mac,ubuntu-client.durable-mac";
/// The installed instance's LaunchAgent, whose `PILOT_REPO` the lab takes.
pub const INSTALLED_PLIST: &str = "Library/LaunchAgents/com.wezzard.machine-service.plist";

/// What a lab host is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// It has Tart and images: it runs runner VMs and VM targets.
    VmHost,
    /// It exposes its Mac: only a target of the durable cells.
    DurableMac,
}

/// The role a host's installed instance's `/health` shows: a durable Mac
/// when it exposes its Mac, otherwise a VM host.
pub fn role_of(health: &Value) -> Role {
    let durable = health
        .get("kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "durable"));
    if durable {
        Role::DurableMac
    } else {
        Role::VmHost
    }
}

/// The account's own home.
pub fn home() -> Result<PathBuf, String> {
    service_layout::test_build::account_home()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| "no home folder".to_string())
}

/// The folder of the running `lab-host`, the bundle it came from, with its
/// links resolved.
pub fn bundle() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let exe = exe.canonicalize().unwrap_or(exe);
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "lab-host has no folder".to_string())
}

/// The bundle `current` names now, resolved.
pub fn current_bundle(home: &Path) -> Option<PathBuf> {
    home.join(BUNDLES).join("current").canonicalize().ok()
}

/// The host's LAN address: the address of the interface of the default
/// route. A host whose address changes gets a new lab instance when idle.
pub fn lan_address() -> Result<Ipv4Addr, String> {
    let route = capture("/sbin/route", &["-n", "get", "default"])?;
    let interface = route
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or("no default route")?
        .to_string();
    capture("/usr/sbin/ipconfig", &["getifaddr", &interface])?
        .trim()
        .parse()
        .map_err(|_| format!("the default route's interface {interface} has no IPv4 address"))
}

/// `PILOT_REPO` from the installed instance's LaunchAgent, read at run
/// time so nothing names the image source (DC-1xx-LAB-5).
pub fn pilot_repo(home: &Path) -> Option<String> {
    let plist = home.join(INSTALLED_PLIST);
    capture(
        "/usr/bin/plutil",
        &[
            "-extract",
            "EnvironmentVariables.PILOT_REPO",
            "raw",
            "-o",
            "-",
            &plist.display().to_string(),
        ],
    )
    .ok()
    .map(|text| text.trim().to_string())
    .filter(|text| !text.is_empty())
}

fn capture(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// How the lab instance is started (LR-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Its root.
    pub root: PathBuf,
    /// The host's LAN address, both listeners' one address.
    pub bind: Ipv4Addr,
    /// The Tart store it reads its bases from.
    pub tart_home: PathBuf,
    /// The bundle's `machine-service`.
    pub executable: PathBuf,
    /// The image source.
    pub pilot_repo: Option<String>,
    /// The host's role.
    pub role: Role,
}

/// The arguments of `second-instance start` for the lab instance.
pub fn start_args(settings: &Settings) -> Vec<String> {
    let bind = settings.bind.to_string();
    let mut args: Vec<String> = [
        "start",
        "--root",
        &settings.root.display().to_string(),
        "--id",
        INSTANCE_ID,
        "--port",
        &HTTP_PORT.to_string(),
        "--session-port",
        &BROKER_PORT.to_string(),
        "--bind",
        &bind,
        "--tart-home",
        &settings.tart_home.display().to_string(),
        "--executable",
        &settings.executable.display().to_string(),
        "--guest-link",
        "relay",
        "--env",
        &format!("MACHINE_SERVICE_RELAY_GUEST_URL=wss://{bind}:{BROKER_PORT}/relay"),
        "--env",
        "MACHINE_SERVICE_GRACE_HOURS=0",
    ]
    .iter()
    .map(|item| item.to_string())
    .collect();
    if let Some(repo) = &settings.pilot_repo {
        args.extend(["--env".to_string(), format!("PILOT_REPO={repo}")]);
    }
    if settings.role == Role::DurableMac {
        args.extend([
            "--durable".to_string(),
            "--durable-cell".to_string(),
            DURABLE_CELLS.to_string(),
        ]);
    }
    args
}

/// The lab instance's base URL on `bind`.
pub fn base(bind: Ipv4Addr) -> String {
    format!("http://{bind}:{HTTP_PORT}")
}

/// GET a JSON answer, or `None`.
pub fn get_json(url: &str, timeout: Duration) -> Option<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into();
    let mut response = agent.get(url).call().ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()
}

/// Whether a `/health` answer is a lab instance's.
pub fn is_lab(health: &Value) -> bool {
    health["second_instance"]["id"] == INSTANCE_ID
}

/// One lab instance that runner VMs may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Its instance's name, the Mac's.
    pub name: String,
    /// Its host's LAN address.
    pub address: Ipv4Addr,
    /// `virtual` and, on a durable Mac, `durable`.
    pub kinds: Vec<String>,
    /// Whether it is this host's own.
    pub own: bool,
}

/// The addresses of a `machctl list --json` answer's instances.
pub fn listed_addresses(list: &Value) -> Vec<Ipv4Addr> {
    let mut addresses: Vec<Ipv4Addr> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|instance| instance.get("address")?.as_str())
        .filter_map(|address| {
            let rest = address.split_once("://").map_or(address, |(_, rest)| rest);
            rest.rsplit_once(':')
                .map_or(rest, |(host, _)| host)
                .parse()
                .ok()
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// The lab endpoints among `addresses`, as `probe` reads each one's lab
/// `/health`; `own` is this host's address, which comes first.
pub fn endpoints(
    own: Ipv4Addr,
    addresses: &[Ipv4Addr],
    probe: &dyn Fn(Ipv4Addr) -> Option<Value>,
) -> Vec<Endpoint> {
    let mut all = vec![own];
    all.extend(addresses.iter().copied().filter(|address| *address != own));
    all.into_iter()
        .filter_map(|address| {
            let health = probe(address).filter(is_lab)?;
            Some(Endpoint {
                name: health.get("instance")?.as_str()?.to_string(),
                address,
                kinds: health
                    .get("kinds")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                own: address == own,
            })
        })
        .collect()
}

/// The plan a job reads from `LAB_PLAN` (LR-3): the lab instances it may
/// use, and its VM cells' target, the host's own.
pub fn plan(endpoints: &[Endpoint], job: u64, run: u64) -> Value {
    let item = |endpoint: &Endpoint| {
        json!({
            "name": endpoint.name,
            "address": endpoint.address.to_string(),
            "url": format!("http://{}:{HTTP_PORT}", endpoint.name),
            "address_url": format!("http://{}:{HTTP_PORT}", endpoint.address),
            "broker_port": BROKER_PORT,
            "kinds": endpoint.kinds,
        })
    };
    json!({
        "job": job,
        "run": run,
        "vm_target": endpoints.iter().find(|endpoint| endpoint.own).map(item),
        "durable_targets": endpoints
            .iter()
            .filter(|endpoint| endpoint.kinds.iter().any(|kind| kind == "durable"))
            .map(item)
            .collect::<Vec<_>>(),
        "instances": endpoints.iter().map(item).collect::<Vec<_>>(),
    })
}

/// The LaunchAgent that runs `lab-host run` from the bundle's `current`
/// link in the desktop session (LR-1, LR-6). The owner loads it.
pub fn launch_agent(home: &Path) -> String {
    let program = home.join(BUNDLES).join("current").join("lab-host");
    let log = home.join("Library/Logs/vm-service-lab.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
		<string>run</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>30</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = crate::LAUNCH_AGENT,
        program = program.display(),
        log = log.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(role: Role) -> Settings {
        Settings {
            root: PathBuf::from("/h/.local/state/vm-service-lab/instance"),
            bind: "192.0.2.3".parse().unwrap(),
            tart_home: PathBuf::from("/h/.tart"),
            executable: PathBuf::from("/h/.local/libexec/vm-service-lab/abc/machine-service"),
            pilot_repo: Some("/p".into()),
            role,
        }
    }

    #[test]
    fn the_lab_instance_listens_on_the_lan_address_with_the_lab_ports() {
        let args = start_args(&settings(Role::VmHost));
        let joined = args.join(" ");
        assert!(joined.starts_with("start --root /h/.local/state/vm-service-lab/instance --id lab --port 6272 --session-port 6273 --bind 192.0.2.3 "));
        assert!(joined.contains("--guest-link relay"));
        assert!(joined.contains("--env MACHINE_SERVICE_RELAY_GUEST_URL=wss://192.0.2.3:6273/relay"));
        assert!(joined.contains("--env PILOT_REPO=/p"));
        assert!(!joined.contains("--durable"));
        let durable = start_args(&settings(Role::DurableMac)).join(" ");
        assert!(durable.ends_with(
            "--durable --durable-cell macos-client.durable-mac,ubuntu-client.durable-mac"
        ));
    }

    #[test]
    fn a_host_is_a_durable_mac_when_its_installed_instance_exposes_it() {
        assert_eq!(role_of(&json!({"kinds": ["virtual"]})), Role::VmHost);
        assert_eq!(
            role_of(&json!({"kinds": ["durable", "virtual"]})),
            Role::DurableMac
        );
        assert_eq!(role_of(&json!({})), Role::VmHost);
    }

    #[test]
    fn endpoints_are_the_hosts_that_answer_as_a_lab_instance() {
        let list = json!([
            {"instance": "a", "address": "http://192.0.2.3:6240"},
            {"instance": "b", "address": "http://192.0.2.4:6240"},
            {"instance": "c", "address": "http://192.0.2.5:6240"},
            {"instance": "d", "address": "http://[::1]:6240"}
        ]);
        let addresses = listed_addresses(&list);
        assert_eq!(addresses.len(), 3);
        let probe = |address: Ipv4Addr| -> Option<Value> {
            match address.octets()[3] {
                3 => Some(
                    json!({"instance": "mac-a", "kinds": ["virtual"], "second_instance": {"id": "lab"}}),
                ),
                4 => Some(
                    json!({"instance": "mac-b", "kinds": ["durable"], "second_instance": {"id": "lab"}}),
                ),
                5 => Some(json!({"instance": "mac-c", "second_instance": {"id": "f00d"}})),
                _ => None,
            }
        };
        let found = endpoints("192.0.2.3".parse().unwrap(), &addresses, &probe);
        assert_eq!(
            found.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["mac-a", "mac-b"]
        );
        assert!(found[0].own && !found[1].own);
        let plan = plan(&found, 7, 9);
        assert_eq!(plan["vm_target"]["name"], "mac-a");
        assert_eq!(plan["vm_target"]["url"], "http://mac-a:6272");
        assert_eq!(plan["durable_targets"][0]["name"], "mac-b");
        assert_eq!(plan["instances"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_launch_agent_runs_the_current_bundle_in_the_desktop_session() {
        let plist = launch_agent(Path::new("/h"));
        assert!(plist.contains("<string>com.wezzard.vm-service.lab</string>"));
        assert!(
            plist.contains("<string>/h/.local/libexec/vm-service-lab/current/lab-host</string>")
        );
        assert!(plist.contains("<string>Aqua</string>"));
        if Path::new("/usr/bin/plutil").exists() {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), &plist).unwrap();
            let lint = Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(file.path())
                .output()
                .unwrap();
            assert!(
                lint.status.success(),
                "{}",
                String::from_utf8_lossy(&lint.stdout)
            );
        }
    }
}
