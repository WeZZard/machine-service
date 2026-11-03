//! The lab's Macs as the runner controller sees them: this host's role and
//! LAN address, the test environments a run's runner VMs may reach, and the
//! plan a job reads (LR-1, LR-3, LR-6).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// The controller's state folder under the account's home.
pub const STATE: &str = ".local/state/vm-service-lab";
/// The controller's bundles under the account's home; `current` names the
/// one in use.
pub const BUNDLES: &str = ".local/libexec/vm-service-lab";
/// The controller's local configuration under the account's home (LR-10).
pub const CONFIG: &str = ".config/vm-service-lab/controller.json";
/// Production's LaunchAgent, whose `PILOT_REPO` a test environment takes.
pub const INSTALLED_PLIST: &str = "Library/LaunchAgents/com.wezzard.machine-service.plist";

/// What a lab host is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// It has Tart and images: it runs ephemeral runners and the VM cells'
    /// test environments.
    VmHost,
    /// It exposes its Mac: a durable cell's target, only during a run its
    /// owner dispatched (LR-10).
    DurableMac,
}

impl Role {
    /// The role's name in the log and the configuration.
    pub fn name(self) -> &'static str {
        match self {
            Role::VmHost => "VM host",
            Role::DurableMac => "durable Mac",
        }
    }
}

/// The role production's `/health` shows: a durable Mac when it exposes
/// its Mac, otherwise a VM host.
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
/// route, which a test environment binds.
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

/// `PILOT_REPO` from production's LaunchAgent, read at run time so nothing
/// names the image source (DC-1xx-LAB-5).
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

/// One test environment that a run's runner VMs may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Its instance's name, the Mac's.
    pub name: String,
    /// Its host's LAN address.
    pub address: Ipv4Addr,
    /// Its HTTP port; the broker's is the next.
    pub port: u16,
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

/// The endpoint a `/health` answer describes, when it is run `run`'s test
/// environment.
pub fn endpoint_of(
    health: &Value,
    run: u64,
    address: Ipv4Addr,
    port: u16,
    own: bool,
) -> Option<Endpoint> {
    if !crate::environment::is_run(health, run) {
        return None;
    }
    Some(Endpoint {
        name: health.get("instance")?.as_str()?.to_string(),
        address,
        port,
        kinds: health
            .get("kinds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        own,
    })
}

/// The durable Macs' test environments of run `run` among `candidates`, as
/// `probe` reads each one's `/health`.
pub fn durable_endpoints(
    run: u64,
    candidates: &[(Ipv4Addr, u16)],
    probe: &dyn Fn(Ipv4Addr, u16) -> Option<Value>,
) -> Vec<Endpoint> {
    candidates
        .iter()
        .filter_map(|&(address, port)| {
            endpoint_of(&probe(address, port)?, run, address, port, false)
        })
        .filter(|endpoint| endpoint.kinds.iter().any(|kind| kind == "durable"))
        .collect()
}

/// The plan a job reads from `LAB_PLAN` (LR-3): the test environments its
/// run may use, its VM cells' target, the host's own, and its durable
/// cells' targets. It reaches the job only inside its runner VM, never a
/// log.
pub fn plan(endpoints: &[Endpoint], job: u64, run: u64) -> Value {
    let item = |endpoint: &Endpoint| {
        json!({
            "name": endpoint.name,
            "address": endpoint.address.to_string(),
            "url": format!("http://{}:{}", endpoint.name, endpoint.port),
            "address_url": format!("http://{}:{}", endpoint.address, endpoint.port),
            "broker_port": endpoint.port + 1,
            "kinds": endpoint.kinds,
        })
    };
    json!({
        "job": job,
        "run": run,
        "vm_target": endpoints.iter().find(|endpoint| endpoint.own).map(item),
        "durable_targets": endpoints
            .iter()
            .filter(|endpoint| !endpoint.own && endpoint.kinds.iter().any(|kind| kind == "durable"))
            .map(item)
            .collect::<Vec<_>>(),
        "instances": endpoints.iter().map(item).collect::<Vec<_>>(),
    })
}

/// The LaunchAgent that runs `lab-host run` from the bundle's `current`
/// link in the desktop session (LR-1). The owner loads it.
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

    #[test]
    fn a_host_is_a_durable_mac_when_production_exposes_it() {
        assert_eq!(role_of(&json!({"kinds": ["virtual"]})), Role::VmHost);
        assert_eq!(
            role_of(&json!({"kinds": ["durable", "virtual"]})),
            Role::DurableMac
        );
        assert_eq!(role_of(&json!({})), Role::VmHost);
    }

    #[test]
    fn a_runs_durable_targets_are_the_durable_test_environments_of_that_run() {
        let list = json!([
            {"instance": "a", "address": "http://192.0.2.3:6240"},
            {"instance": "b", "address": "http://192.0.2.4:6240"},
            {"instance": "c", "address": "http://192.0.2.5:6240"},
            {"instance": "d", "address": "http://[::1]:6240"}
        ]);
        let addresses = listed_addresses(&list);
        assert_eq!(addresses.len(), 3);
        let probe = |address: Ipv4Addr, _port: u16| -> Option<Value> {
            match address.octets()[3] {
                3 => Some(
                    json!({"instance": "mac-a", "kinds": ["virtual"], "second_instance": {"id": "r9"}}),
                ),
                4 => Some(
                    json!({"instance": "mac-b", "kinds": ["durable"], "second_instance": {"id": "r9"}}),
                ),
                5 => Some(
                    json!({"instance": "mac-c", "kinds": ["durable"], "second_instance": {"id": "r8"}}),
                ),
                _ => None,
            }
        };
        let candidates: Vec<(Ipv4Addr, u16)> =
            addresses.iter().map(|address| (*address, 6272)).collect();
        let durable = durable_endpoints(9, &candidates, &probe);
        assert_eq!(
            durable.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["mac-b"]
        );
        let own = endpoint_of(
            &probe("192.0.2.3".parse().unwrap(), 6272).unwrap(),
            9,
            "192.0.2.3".parse().unwrap(),
            6272,
            true,
        )
        .unwrap();
        let mut all = vec![own];
        all.extend(durable);
        let plan = plan(&all, 7, 9);
        assert_eq!(plan["run"], 9);
        assert_eq!(plan["vm_target"]["name"], "mac-a");
        assert_eq!(plan["vm_target"]["url"], "http://mac-a:6272");
        assert_eq!(plan["vm_target"]["broker_port"], 6273);
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
