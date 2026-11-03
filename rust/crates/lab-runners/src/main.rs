//! `lab-host`: keep this host's lab instance running and, on a VM host,
//! serve queued lab jobs with one-job runner VMs (`docs/arch/lab-runners.md`).
//!
//! ```text
//! lab-host run            # the LaunchAgent's program (LR-1)
//! lab-host doctor         # what this host lacks, never a secret
//! lab-host allowlist      # write the egress allowlist once (LR-5)
//! lab-host launch-agent   # print the LaunchAgent's property list
//! ```

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lab_runners::dispatcher::{Archives, Dispatcher, Given, Pinned};
use lab_runners::dns::{Fixed, OverTls, Resolve};
use lab_runners::github::{JobSource, Rest, API};
use lab_runners::instance::{Http, Instance};
use lab_runners::lab::{self, Endpoint, Role, Settings};
use lab_runners::seams::Seams;
use lab_runners::{allowlist, log, token, REPOSITORY};
use serde_json::Value;

/// How often the dispatcher looks for queued jobs.
const ROUND_S: u64 = 15;
/// How often the lab instance and the lab table are checked.
const LAB_S: u64 = 60;

fn main() {
    service_layout::test_build::start();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run(),
        Some("doctor") => doctor(),
        Some("allowlist") => Host::new().and_then(|host| host.refresh_allowlist()),
        Some("launch-agent") => lab::home().map(|home| print!("{}", lab::launch_agent(&home))),
        _ => Err("usage: lab-host run | doctor | allowlist | launch-agent".into()),
    };
    if let Err(error) = result {
        log(&error);
        std::process::exit(1);
    }
}

/// Everything `lab-host` reaches, the world's or a test build's seams.
struct Host {
    seams: Seams,
    home: PathBuf,
    state: PathBuf,
    bundle: PathBuf,
    egress: guest_firewall::EgressFiles,
}

impl Host {
    fn new() -> Result<Host, String> {
        let seams = Seams::load()?;
        let home = lab::home()?;
        let state = seams.state.clone().unwrap_or_else(|| home.join(lab::STATE));
        let table = seams
            .egress
            .as_ref()
            .map(|folder| folder.join("internet-guests"))
            .unwrap_or_else(|| PathBuf::from(guest_firewall::DEFAULT_TABLE_FILE));
        Ok(Host {
            egress: guest_firewall::EgressFiles::beside(&table),
            bundle: lab::bundle()?,
            seams,
            home,
            state,
        })
    }

    fn test_build(&self) -> bool {
        service_layout::test_build::is_test_build()
    }

    /// The installed instance's `/health`, read only, for the host's role
    /// and its machine ID. A test build never reads it.
    fn installed_health(&self) -> Option<Value> {
        if self.test_build() {
            return None;
        }
        lab::get_json("http://127.0.0.1:6240/health", Duration::from_secs(5))
    }

    fn role(&self) -> Role {
        self.installed_health()
            .map(|health| lab::role_of(&health))
            .unwrap_or(Role::VmHost)
    }

    /// This host's tag in its runners' names: a hash of its machine ID, so
    /// a public job log never names the Mac.
    fn tag(&self) -> String {
        let id = self
            .installed_health()
            .and_then(|health| {
                health
                    .get("machine_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "test".to_string());
        lab_runners::runner::sha256(id.as_bytes())[..8].to_string()
    }

    fn resolver(&self) -> Result<Box<dyn Resolve>, String> {
        if self.test_build() {
            return Ok(Box::new(Fixed(self.seams.dns.clone())));
        }
        let (_, upstreams) = allowlist::resolvers();
        Ok(Box::new(OverTls::new(&upstreams)?))
    }

    fn github(&self) -> Result<String, String> {
        match (&self.seams.github, self.test_build()) {
            (Some(url), true) => Ok(url.clone()),
            (None, true) => Err("a test build reaches GitHub only through its seams".into()),
            (_, false) => Ok(API.to_string()),
        }
    }

    /// Write the egress allowlist from GitHub's `meta` (LR-5); a failed
    /// refresh keeps the last list.
    fn refresh_allowlist(&self) -> Result<(), String> {
        let url = format!("{}/meta", self.github()?);
        let meta =
            lab::get_json(&url, Duration::from_secs(30)).ok_or("GitHub's meta did not answer")?;
        let (ranges, names) = allowlist::from_meta(&meta)?;
        let (resolvers, _) = allowlist::resolvers();
        let entries = allowlist::assemble(&ranges, &names, &resolvers, self.resolver()?.as_ref())?;
        allowlist::write(&self.egress.allow, &entries, names.len(), ranges.len())
            .map_err(|error| format!("{}: {error}", self.egress.allow.display()))?;
        log(&format!(
            "egress allowlist written: {} entries",
            entries.len()
        ));
        Ok(())
    }

    /// The lab endpoints: in a test build the seams', else every listed
    /// instance's host that answers as a lab instance.
    fn endpoints(&self, own: Ipv4Addr) -> Vec<Endpoint> {
        if let Some(value) = &self.seams.endpoints {
            return value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| {
                    Some(Endpoint {
                        name: item.get("name")?.as_str()?.to_string(),
                        address: item.get("address")?.as_str()?.parse().ok()?,
                        kinds: vec!["virtual".into()],
                        own: item.get("own").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect();
        }
        let listed = Command::new(self.bundle.join("machctl"))
            .args(["list", "--json"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &self.home)
            .output()
            .ok()
            .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
            .unwrap_or(Value::Null);
        let probe = |address: Ipv4Addr| {
            lab::get_json(
                &format!("{}/health", lab::base(address)),
                Duration::from_secs(5),
            )
        };
        lab::endpoints(own, &lab::listed_addresses(&listed), &probe)
    }

    /// Write the lab table: the endpoints' addresses (LR-5).
    fn write_lab_table(&self, endpoints: &[Endpoint]) {
        let mut entries: Vec<String> = endpoints
            .iter()
            .map(|endpoint| endpoint.address.to_string())
            .collect();
        entries.sort();
        entries.dedup();
        let comments = [
            "lab hosts an egress guest may reach on the lab ports (DC-267); written by lab-host"
                .to_string(),
        ];
        if let Err(error) = guest_firewall::write_addresses(&self.egress.lab, &comments, &entries) {
            log(&format!("lab table: {error}"));
        }
    }

    /// Keep the lab instance running and current (LR-6). Answers its base
    /// URL once it answers as the lab instance.
    fn ensure_lab(&self, role: Role, idle: bool) -> Result<String, String> {
        let bind = lab::lan_address()?;
        let settings = Settings {
            root: self.state.join("instance"),
            bind,
            tart_home: self.home.join(".tart"),
            executable: self.bundle.join("machine-service"),
            pilot_repo: lab::pilot_repo(&self.home),
            role,
        };
        let base = lab::base(bind);
        let healthy = lab::get_json(&format!("{base}/health"), Duration::from_secs(5))
            .is_some_and(|health| lab::is_lab(&health));
        let manifest = second_instance::Manifest::read(&settings.root)?;
        let current = manifest.as_ref().is_some_and(|manifest| {
            manifest.bind == bind.to_string() && manifest.executable == settings.executable
        });
        let launcher = self.bundle.join("second-instance");
        if healthy && current {
            return Ok(base);
        }
        if manifest.is_some() && !current {
            let leases = Http::new(&base)
                .leases()
                .map(|leases| leases.len())
                .unwrap_or(0);
            if !idle || leases > 0 {
                return Err("the lab instance needs a restart for a new bundle or address; waiting until it is idle".into());
            }
            log("restarting the lab instance for a new bundle or address");
            launch(
                &launcher,
                &[
                    "stop".into(),
                    "--root".into(),
                    settings.root.display().to_string(),
                ],
            )?;
        }
        launch(&launcher, &lab::start_args(&settings))?;
        log("lab instance started");
        Ok(base)
    }
}

fn launch(launcher: &Path, args: &[String]) -> Result<(), String> {
    let output = Command::new(launcher)
        .args(args)
        .output()
        .map_err(|error| format!("{}: {error}", launcher.display()))?;
    if !output.status.success() {
        return Err(format!(
            "second-instance {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let host = Arc::new(Host::new()?);
    let role = host.role();
    log(&format!(
        "running as a {} lab host",
        if role == Role::VmHost {
            "VM"
        } else {
            "durable Mac"
        }
    ));
    let instance_url = match &host.seams.instance {
        Some(url) => Some(url.clone()),
        None if host.test_build() => {
            return Err("a test build reaches an instance only through its seams".into())
        }
        None => None,
    };
    let github = host.github()?;
    let source = Box::new(move || -> Result<Arc<dyn JobSource>, String> {
        let token = token::read()?;
        Ok(Arc::new(Rest::new(&github, REPOSITORY, token)) as Arc<dyn JobSource>)
    });
    let archives: Arc<dyn Archives> = if host.test_build() {
        Arc::new(Given(host.seams.runners.clone()))
    } else {
        Arc::new(Pinned(host.state.join("cache")))
    };
    let mut dispatcher: Option<Arc<Dispatcher>> = None;
    let mut last_lab: Option<Instant> = None;
    let mut last_allowlist: Option<Instant> = None;
    let rounds = host.seams.rounds;
    let interval = Duration::from_secs(host.seams.interval_s.unwrap_or(ROUND_S));
    let mut round = 0u64;
    loop {
        let idle = dispatcher
            .as_ref()
            .is_none_or(|dispatcher| dispatcher.ours().is_empty());
        // A new bundle in `current`: exit once idle, and launchd starts it.
        if !host.test_build() && idle {
            if let Some(current) = lab::current_bundle(&host.home) {
                if current != host.bundle {
                    log("a new lab bundle is current; exiting so launchd starts it");
                    return Ok(());
                }
            }
        }
        if last_lab.is_none_or(|at| at.elapsed() >= Duration::from_secs(LAB_S)) {
            last_lab = Some(Instant::now());
            let base = match &instance_url {
                Some(url) => Ok(url.clone()),
                None => host.ensure_lab(role, idle),
            };
            match base {
                Ok(base) if role == Role::VmHost => {
                    let own = if host.test_build() {
                        Ipv4Addr::LOCALHOST
                    } else {
                        lab::lan_address().unwrap_or(Ipv4Addr::LOCALHOST)
                    };
                    let endpoints = host.endpoints(own);
                    host.write_lab_table(&endpoints);
                    let dispatcher = dispatcher.get_or_insert_with(|| {
                        let created = Dispatcher::new(
                            source.clone_box(),
                            Arc::new(Http::new(&base)) as Arc<dyn Instance>,
                            Arc::clone(&archives),
                            &host.tag(),
                            host.state.clone(),
                        );
                        created.recover();
                        created
                    });
                    dispatcher.set_endpoints(endpoints);
                }
                Ok(_) => {}
                Err(error) => log(&format!("lab instance: {error}")),
            }
        }
        if role == Role::VmHost
            && last_allowlist
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(allowlist::REFRESH_S))
        {
            last_allowlist = Some(Instant::now());
            if let Err(error) = host.refresh_allowlist() {
                log(&format!("egress allowlist: {error}"));
            }
        }
        if let Some(dispatcher) = &dispatcher {
            dispatcher.round();
        }
        round += 1;
        if rounds.is_some_and(|rounds| round >= rounds) {
            if let Some(dispatcher) = &dispatcher {
                dispatcher.join();
            }
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

/// A source factory that can be handed to a dispatcher more than once.
trait CloneBox {
    fn clone_box(&self) -> lab_runners::dispatcher::SourceFactory;
}

impl<F> CloneBox for Box<F>
where
    F: Fn() -> Result<Arc<dyn JobSource>, String> + Send + Sync + Clone + 'static,
{
    fn clone_box(&self) -> lab_runners::dispatcher::SourceFactory {
        Box::new((**self).clone())
    }
}

/// What this host lacks for the lab, printed without any secret, name or
/// address.
fn doctor() -> Result<(), String> {
    let host = Host::new()?;
    let mark = |ok: bool| if ok { "ok     " } else { "MISSING" };
    let token = token::read();
    println!(
        "{} GitHub token in the login keychain{}",
        mark(token.is_ok()),
        match &token {
            Ok(_) => String::new(),
            Err(_) => format!(": {}", token::store_command()),
        }
    );
    let anchor = std::fs::read_to_string(guest_firewall::ANCHOR_FILE).unwrap_or_default();
    let egress = guest_firewall::parse_anchor_egress(&anchor);
    println!(
        "{} guest firewall egress section{}",
        mark(egress.is_some()),
        if egress.is_some() {
            String::new()
        } else {
            format!(
                ": sudo {} install",
                host.bundle.join(guest_firewall::INSTALLER).display()
            )
        }
    );
    let allow = std::fs::read_to_string(&host.egress.allow).unwrap_or_default();
    println!(
        "{} egress allowlist ({} entries)",
        mark(guest_firewall::parse_check_name(&allow).is_some()),
        guest_firewall::parse_addresses(&allow).len()
    );
    let lab_ok = lab::lan_address()
        .ok()
        .and_then(|bind| {
            lab::get_json(
                &format!("{}/health", lab::base(bind)),
                Duration::from_secs(5),
            )
        })
        .is_some_and(|health| lab::is_lab(&health));
    println!(
        "{} lab instance answering on port {}",
        mark(lab_ok),
        lab::HTTP_PORT
    );
    let plist = host
        .home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", lab_runners::LAUNCH_AGENT));
    println!(
        "{} LaunchAgent {}",
        mark(plist.exists()),
        lab_runners::LAUNCH_AGENT
    );
    println!(
        "        role: {}",
        if host.role() == Role::VmHost {
            "VM host"
        } else {
            "durable Mac"
        }
    );
    Ok(())
}
