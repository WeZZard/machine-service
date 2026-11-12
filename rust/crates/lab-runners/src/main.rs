//! `lab-host`: the runner controller (`docs/arch/lab-runners.md`). On a VM
//! host it serves queued lab jobs with ephemeral runner VMs that production
//! lends and runs each run's test environment; on a durable Mac it runs the
//! test environment only for a run its owner dispatched by hand.
//!
//! ```text
//! lab-host run            # the LaunchAgent's program (LR-1)
//! lab-host doctor         # what this host lacks, never a secret
//! lab-host allowlist      # write the egress allowlist once (LR-5)
//! lab-host launch-agent   # print the LaunchAgent's property list
//! ```

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lab_runners::config::Config;
use lab_runners::dispatcher::{Archives, Dispatcher, Given, Lab, Pinned, SourceFactory};
use lab_runners::dns::{Fixed, OverTls, Resolve};
use lab_runners::durable::Controller;
use lab_runners::environment::{Environments, Place, HTTP_PORT};
use lab_runners::github::{JobSource, Rest, API};
use lab_runners::lab::{self, Role};
use lab_runners::production::{Http, Production, CI_PACK, PRODUCTION};
use lab_runners::seams::Seams;
use lab_runners::{allowlist, log, token, REPOSITORY};
use serde_json::Value;

/// How often a VM host looks for queued jobs.
const ROUND_S: u64 = 15;
/// How often a durable Mac looks for its owner's dispatched runs.
const DURABLE_S: u64 = 30;

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

    /// Production's loopback API: a test build's fake, else the installed
    /// instance's port on loopback (DC-284).
    fn production(&self) -> Result<String, String> {
        match (&self.seams.production, self.test_build()) {
            (Some(url), true) => Ok(url.clone()),
            (None, true) => Err("a test build reaches production only through its seams".into()),
            (_, false) => Ok(PRODUCTION.to_string()),
        }
    }

    fn production_health(&self) -> Option<Value> {
        Http::new(&self.production().ok()?).health()
    }

    /// The host's role: a test build's seam, else what production's
    /// `/health` shows. Without production there is nothing to do.
    fn role(&self) -> Result<Role, String> {
        if self.test_build() {
            return Ok(match self.seams.role.as_deref() {
                Some("durable") => Role::DurableMac,
                _ => Role::VmHost,
            });
        }
        self.production_health()
            .map(|health| lab::role_of(&health))
            .ok_or_else(|| {
                "production machine-service does not answer on loopback; nothing to do".into()
            })
    }

    /// This host's tag in its runners' names: a hash of production's
    /// machine ID, so a public job log never names the Mac.
    fn tag(&self) -> String {
        let id = (!self.test_build())
            .then(|| self.production_health())
            .flatten()
            .and_then(|health| {
                health
                    .get("machine_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "test".to_string());
        lab_runners::runner::sha256(id.as_bytes())[..8].to_string()
    }

    fn config(&self) -> Result<Config, String> {
        if self.test_build() {
            return match &self.seams.config {
                Some(path) => Config::load(path),
                None => Ok(Config::default()),
            };
        }
        Config::load(&self.home.join(lab::CONFIG))
    }

    fn address(&self) -> Result<Ipv4Addr, String> {
        match (self.seams.address, self.test_build()) {
            (Some(address), true) => Ok(address),
            (None, true) => Ok(Ipv4Addr::LOCALHOST),
            (_, false) => lab::lan_address(),
        }
    }

    /// Where this host's test environments run.
    fn place(&self, role: Role) -> Result<Place, String> {
        let test = self.test_build();
        Ok(Place {
            bind: self.address()?,
            port: self.seams.environment_port.unwrap_or(HTTP_PORT),
            tart_home: if test {
                self.state.join("tart")
            } else {
                self.home.join(".tart")
            },
            pilot_repo: (!test).then(|| lab::pilot_repo(&self.home)).flatten(),
            durable: role == Role::DurableMac,
            fallback_launcher: self.bundle.join("second-instance"),
            // By convention, a published release in the controller's bundle,
            // which `lab-host.sh install --relay-agent-release` puts there.
            relay_agent_release: Some(self.bundle.join(service_layout::release::FOLDER))
                .filter(|release| !test && release.is_dir()),
        })
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

    /// The other lab hosts' test environment endpoints: in a test build
    /// the seams', else the port 6272 of every instance discovery lists.
    fn peers(&self, own: Ipv4Addr) -> Vec<(Ipv4Addr, u16)> {
        if self.test_build() {
            return self.seams.peers.clone();
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
        lab::listed_addresses(&listed)
            .into_iter()
            .filter(|address| *address != own)
            .map(|address| (address, HTTP_PORT))
            .collect()
    }

    fn rounds(&self) -> (Option<u64>, Option<Duration>) {
        (
            self.seams.rounds,
            self.seams.interval_s.map(Duration::from_secs),
        )
    }

    /// Whether a newer bundle is `current`, so this one exits once idle and
    /// launchd starts the new one.
    fn superseded(&self) -> bool {
        !self.test_build()
            && lab::current_bundle(&self.home).is_some_and(|current| current != self.bundle)
    }
}

fn source_factory(github: String) -> SourceFactory {
    Box::new(move || -> Result<Arc<dyn JobSource>, String> {
        let token = token::read()?;
        Ok(Arc::new(Rest::new(&github, REPOSITORY, token)) as Arc<dyn JobSource>)
    })
}

fn run() -> Result<(), String> {
    let host = Arc::new(Host::new()?);
    let role = host.role()?;
    let config = host.config()?;
    log(&format!(
        "running as the runner controller of a {}",
        role.name()
    ));
    let environments = Arc::new(Environments::new(
        host.state.join("runs"),
        host.place(role)?,
    ));
    let github = host.github()?;
    match role {
        Role::VmHost => run_vm_host(&host, github, environments),
        Role::DurableMac => run_durable(&host, github, environments, config.owner),
    }
}

fn run_vm_host(
    host: &Arc<Host>,
    github: String,
    environments: Arc<Environments>,
) -> Result<(), String> {
    let archives: Arc<dyn Archives> = if host.test_build() {
        Arc::new(Given(host.seams.runners.clone()))
    } else {
        Arc::new(Pinned(host.state.join("cache")))
    };
    let own = environments.address();
    let peers = {
        let host = Arc::clone(host);
        Box::new(move || host.peers(own))
    };
    let dispatcher = Dispatcher::new(
        source_factory(github),
        Arc::new(Http::new(&host.production()?)) as Arc<dyn Production>,
        archives,
        environments,
        Lab {
            own,
            peers,
            table: host.egress.lab.clone(),
        },
        &host.tag(),
        host.state.clone(),
    );
    dispatcher.recover();
    let (rounds, interval) = host.rounds();
    let interval = interval.unwrap_or(Duration::from_secs(ROUND_S));
    let mut last_allowlist: Option<Instant> = None;
    let mut round = 0u64;
    loop {
        if dispatcher.idle() && host.superseded() {
            log("a new lab bundle is current; exiting so launchd starts it");
            return Ok(());
        }
        if last_allowlist.is_none_or(|at| at.elapsed() >= Duration::from_secs(allowlist::REFRESH_S))
        {
            last_allowlist = Some(Instant::now());
            if let Err(error) = host.refresh_allowlist() {
                log(&format!("egress allowlist: {error}"));
            }
        }
        dispatcher.round();
        round += 1;
        if rounds.is_some_and(|rounds| round >= rounds) {
            dispatcher.join();
            // One last round retires what the ended jobs left.
            dispatcher.round();
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn run_durable(
    host: &Arc<Host>,
    github: String,
    environments: Arc<Environments>,
    owner: Option<String>,
) -> Result<(), String> {
    let controller = Controller::new(source_factory(github), environments, owner);
    controller.recover();
    let (rounds, interval) = host.rounds();
    let interval = interval.unwrap_or(Duration::from_secs(DURABLE_S));
    let mut round = 0u64;
    loop {
        if controller.idle() && host.superseded() {
            log("a new lab bundle is current; exiting so launchd starts it");
            return Ok(());
        }
        controller.round();
        round += 1;
        if rounds.is_some_and(|rounds| round >= rounds) {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

/// What this host lacks for the lab, printed without any secret, name or
/// address.
fn doctor() -> Result<(), String> {
    let host = Host::new()?;
    let mark = |ok: bool| if ok { "ok     " } else { "MISSING" };
    let role = host.role();
    println!(
        "{} production machine-service answering on loopback",
        mark(role.is_ok())
    );
    let role = role.unwrap_or(Role::VmHost);
    println!("        role: {}", role.name());
    let token = token::read();
    println!(
        "{} GitHub token in the login keychain{}",
        mark(token.is_ok()),
        match &token {
            Ok(_) => String::new(),
            Err(_) => format!(": {}", token::store_command()),
        }
    );
    let config = host.config();
    match role {
        Role::VmHost => {
            let ci = host.home.join(".config/vm-credentials").join(CI_PACK);
            println!(
                "{} production's {CI_PACK} credential pack{}",
                mark(ci.is_dir()),
                if ci.is_dir() {
                    String::new()
                } else {
                    format!(": mkdir -m 700 {}", ci.display())
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
        }
        Role::DurableMac => {
            let owner = config.as_ref().ok().and_then(|config| config.owner.clone());
            println!(
                "{} owner login in ~/{}{}",
                mark(owner.is_some()),
                lab::CONFIG,
                match &config {
                    Err(error) => format!(": {error}"),
                    Ok(_) if owner.is_none() => ": a durable Mac serves no run without it".into(),
                    Ok(_) => String::new(),
                }
            );
        }
    }
    let runs = std::fs::read_dir(host.state.join("runs"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    println!("        test environments on this Mac now: {runs}");
    let plist = host
        .home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", lab_runners::LAUNCH_AGENT));
    println!(
        "{} LaunchAgent {}",
        mark(plist.exists()),
        lab_runners::LAUNCH_AGENT
    );
    Ok(())
}
