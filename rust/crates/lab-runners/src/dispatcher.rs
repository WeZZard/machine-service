//! The runner controller of a VM host: queued lab jobs to ephemeral runner
//! VMs that production lends, each with its run's test environment (LR-3,
//! LR-6, DC-284).

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::decide::{self, Ours};
use crate::environment::Environments;
use crate::github::{Job, JobSource, Run};
use crate::lab::{self, Endpoint};
use crate::labels::{self, Kind, Need};
use crate::production::{self, Http, Lease, Production};
use crate::{runner, runs, Told, REPOSITORY};

/// The runner archives a controller pushes into its guests.
pub trait Archives: Send + Sync {
    /// The archive for a guest kind, checked against its pin, and the pin.
    fn archive(&self, kind: Kind) -> Result<(Vec<u8>, String), String>;
}

/// The pinned archives, cached under a folder (LR-8).
pub struct Pinned(pub PathBuf);

impl Archives for Pinned {
    fn archive(&self, kind: Kind) -> Result<(Vec<u8>, String), String> {
        runner::cached(&self.0, runner::platform(kind))
    }
}

/// Archives given by a test build's seams, each with its own SHA-256.
pub struct Given(pub std::collections::BTreeMap<String, (PathBuf, String)>);

impl Archives for Given {
    fn archive(&self, kind: Kind) -> Result<(Vec<u8>, String), String> {
        let (path, sha) = self
            .0
            .get(runner::platform(kind))
            .ok_or_else(|| format!("no runner archive for {}", runner::platform(kind)))?;
        Ok((runner::read_checked(path, sha)?, sha.clone()))
    }
}

/// Makes the job source for a round, reading the token afresh.
pub type SourceFactory = Box<dyn Fn() -> Result<Arc<dyn JobSource>, String> + Send + Sync>;

/// The other lab hosts' test environment endpoints to probe.
pub type Peers = Box<dyn Fn() -> Vec<(Ipv4Addr, u16)> + Send + Sync>;

/// The lab around this host: its own address and where to look for the
/// durable Macs' test environments.
pub struct Lab {
    /// This host's LAN address, where its test environment listens.
    pub own: Ipv4Addr,
    /// The other hosts' endpoints to probe for a run's durable targets.
    pub peers: Peers,
    /// The guest firewall's lab table (DC-267).
    pub table: PathBuf,
}

/// One VM host's runner controller.
pub struct Dispatcher {
    source: SourceFactory,
    production: Arc<dyn Production>,
    archives: Arc<dyn Archives>,
    environments: Arc<Environments>,
    lab: Lab,
    /// This host's tag in its runners' names.
    tag: String,
    /// `<state>/jobs`, one file per job between its start and its release.
    jobs: PathBuf,
    ours: Mutex<Vec<Ours>>,
    /// The lab addresses each of our jobs' runner VMs reaches.
    reach: Mutex<BTreeMap<u64, Vec<Ipv4Addr>>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    told: Told,
    /// How often a job's watcher looks at its runner.
    pub watch_every: Duration,
}

impl Dispatcher {
    /// A controller of the runner VMs `production` lends for the jobs
    /// `source` finds, with this host's test `environments`.
    pub fn new(
        source: SourceFactory,
        production: Arc<dyn Production>,
        archives: Arc<dyn Archives>,
        environments: Arc<Environments>,
        lab: Lab,
        tag: &str,
        state: PathBuf,
    ) -> Arc<Dispatcher> {
        Arc::new(Dispatcher {
            source,
            production,
            archives,
            environments,
            lab,
            tag: tag.to_string(),
            jobs: state.join("jobs"),
            ours: Mutex::new(Vec::new()),
            reach: Mutex::new(BTreeMap::new()),
            threads: Mutex::new(Vec::new()),
            told: Told::default(),
            watch_every: Duration::from_secs(30),
        })
    }

    /// This controller's jobs between start and release.
    pub fn ours(&self) -> Vec<Ours> {
        self.ours.lock().expect("ours").clone()
    }

    /// Whether it serves no job and runs no test environment.
    pub fn idle(&self) -> bool {
        self.ours().is_empty() && self.environments.current().is_none()
    }

    /// Wait for every job thread to end.
    pub fn join(&self) {
        let threads: Vec<_> = self.threads.lock().expect("threads").drain(..).collect();
        for thread in threads {
            let _ = thread.join();
        }
    }

    fn source(&self) -> Option<Arc<dyn JobSource>> {
        match (self.source)() {
            Ok(source) => Some(source),
            Err(error) => {
                self.told.say(
                    &format!("serving no job: {error}"),
                    Duration::from_secs(3600),
                );
                None
            }
        }
    }

    /// Release what an earlier controller on this host left: every runner
    /// VM production lent it (purpose `lab-<job>` and the `ci` pack) that is
    /// not ours, every runner its job files name, and every test
    /// environment, from its launcher's manifest (LR-3, LR-6).
    pub fn recover(&self) {
        let mine: Vec<u64> = self.ours().iter().map(|job| job.job).collect();
        match self.production.leases() {
            Ok(leases) => {
                for lease in leases {
                    let Some(job) = decide::job_of_purpose(&lease.purpose) else {
                        continue;
                    };
                    if mine.contains(&job) || production::check_ci(&lease).is_err() {
                        continue;
                    }
                    crate::log(&format!(
                        "job {job}: releasing the runner VM {} an earlier lab-host left",
                        lease.vm
                    ));
                    if let Err(error) = self.production.release(&lease.vm, "lab-host restarted") {
                        crate::log(&format!("job {job}: {error}"));
                    }
                }
            }
            Err(error) => crate::log(&format!("recovery: {error}")),
        }
        self.environments.retire(&|_| false);
        let Ok(entries) = std::fs::read_dir(&self.jobs) else {
            return;
        };
        let Some(source) = self.source() else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let record: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let job = record
                .get("job")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            if mine.contains(&job) {
                continue;
            }
            if let Some(runner) = record.get("runner_id").and_then(Value::as_u64) {
                if let Err(error) = source.delete_runner(runner) {
                    crate::log(&format!(
                        "job {job}: the runner an earlier lab-host left: {error}"
                    ));
                    continue;
                }
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }

    /// One round (LR-2): read the eligible runs' queued lab jobs, retire
    /// the test environment of a run that ended, and start a thread for
    /// each job [`decide::to_start`] picks whose target is up.
    pub fn round(self: &Arc<Self>) {
        let Some(source) = self.source() else {
            self.environments.retire(&|_| true);
            return;
        };
        let read = || -> Result<(Vec<Run>, Vec<Job>), String> {
            let runs: Vec<Run> = source
                .active_runs()?
                .into_iter()
                .filter(|run| runs::eligible(run, REPOSITORY).is_ok())
                .collect();
            let mut jobs = Vec::new();
            for run in &runs {
                jobs.extend(
                    source
                        .jobs(run.id)?
                        .into_iter()
                        .filter(|job| job.queued() && labels::need(&job.labels).is_some()),
                );
            }
            Ok((runs, jobs))
        };
        let (runs, jobs) = match read() {
            Ok(read) => read,
            Err(error) => {
                crate::log(&format!("round: {error}"));
                // Without GitHub, a test environment lives out its lifetime.
                self.environments.retire(&|_| true);
                return;
            }
        };
        let busy: BTreeSet<u64> = self.ours().iter().map(|job| job.run).collect();
        let active: BTreeSet<u64> = runs.iter().map(|run| run.id).collect();
        self.environments
            .retire(&|run| busy.contains(&run) || active.contains(&run));
        if jobs.is_empty() {
            return;
        }
        let capacity = source.runners().and_then(|runners| {
            let capacity = self.production.capacity()?;
            Ok((
                runners,
                decide::without_test_environment(capacity, self.environments.running_macos()),
            ))
        });
        let (runners, capacity) = match capacity {
            Ok(read) => read,
            Err(error) => {
                crate::log(&format!("round: {error}"));
                return;
            }
        };
        let picked = decide::to_start(&jobs, &runners, capacity, &self.ours());
        let mut starts = Vec::new();
        for (job, need) in picked {
            let Some(run) = runs.iter().find(|run| run.id == job.run_id) else {
                continue;
            };
            match self.targets(run, need, source.as_ref()) {
                Ok(endpoints) => starts.push((job, need, endpoints)),
                Err(why) => self.told.say(
                    &format!("job {}: waiting: {why}", job.id),
                    Duration::from_secs(600),
                ),
            }
        }
        if starts.is_empty() {
            return;
        }
        {
            let mut ours = self.ours.lock().expect("ours");
            let mut reach = self.reach.lock().expect("reach");
            for (job, need, endpoints) in &starts {
                ours.push(Ours {
                    job: job.id,
                    run: job.run_id,
                    key: labels::key(&job.labels),
                    runner: None,
                    acquired: false,
                    need: *need,
                });
                reach.insert(
                    job.id,
                    endpoints.iter().map(|endpoint| endpoint.address).collect(),
                );
            }
        }
        // Before any acquisition: production passes the lab table to
        // Softnet when it starts a runner VM (DC-267).
        self.write_table();
        for (job, need, endpoints) in starts {
            crate::log(&format!(
                "job {} of run {}: starting a {} runner VM",
                job.id, job.run_id, need.image
            ));
            let dispatcher = Arc::clone(self);
            let source = Arc::clone(&source);
            let thread =
                std::thread::spawn(move || dispatcher.run_job(&job, need, source, &endpoints));
            self.threads.lock().expect("threads").push(thread);
        }
        self.threads
            .lock()
            .expect("threads")
            .retain(|thread| !thread.is_finished());
    }

    /// The test environments a job of `run` reaches: this host's own,
    /// started for the run when it is not up yet, or for a durable cell the
    /// run's durable Macs' (LR-6, LR-10).
    fn targets(
        &self,
        run: &Run,
        need: Need,
        source: &dyn JobSource,
    ) -> Result<Vec<Endpoint>, String> {
        if need.durable {
            let probe = |address: Ipv4Addr, port: u16| {
                lab::get_json(
                    &format!("http://{address}:{port}/health"),
                    Duration::from_secs(5),
                )
            };
            let found = lab::durable_endpoints(run.id, &(self.lab.peers)(), &probe);
            if found.is_empty() {
                return Err(format!(
                    "no durable Mac runs a test environment for run {}",
                    run.id
                ));
            }
            return Ok(found);
        }
        let current = self.environments.ensure(run, source)?;
        let health = Http::new(&current.base)
            .health()
            .ok_or_else(|| format!("run {}'s test environment does not answer", run.id))?;
        let own = lab::endpoint_of(
            &health,
            run.id,
            self.environments.address(),
            self.environments.port(),
            true,
        )
        .ok_or_else(|| format!("run {}'s test environment answers as another", run.id))?;
        Ok(vec![own])
    }

    /// Write the lab table: this host's address and every lab address one
    /// of our runner VMs reaches.
    fn write_table(&self) {
        let mut entries: Vec<String> = self
            .reach
            .lock()
            .expect("reach")
            .values()
            .flatten()
            .chain(std::iter::once(&self.lab.own))
            .map(|address| address.to_string())
            .collect();
        entries.sort();
        entries.dedup();
        let comments = [
            "the test environments an egress guest may reach on the lab ports (DC-267); written by lab-host"
                .to_string(),
        ];
        if let Err(error) = guest_firewall::write_addresses(&self.lab.table, &comments, &entries) {
            crate::log(&format!("lab table: {error}"));
        }
    }

    fn update(&self, job: u64, change: impl FnOnce(&mut Ours)) {
        if let Some(entry) = self
            .ours
            .lock()
            .expect("ours")
            .iter_mut()
            .find(|entry| entry.job == job)
        {
            change(entry);
        }
    }

    fn record(&self, job: u64, record: &Value) {
        let _ = std::fs::create_dir_all(&self.jobs);
        let _ = std::fs::write(self.jobs.join(format!("{job}.json")), record.to_string());
    }

    /// A job's whole life on this host, from the acquisition to the release
    /// (LR-3). Every path out releases the VM and removes the runner.
    fn run_job(&self, job: &Job, need: Need, source: Arc<dyn JobSource>, endpoints: &[Endpoint]) {
        let purpose = decide::purpose(job.id);
        let started = self
            .production
            .acquire(&production::runner_request(need.image, &purpose));
        let lease = match started {
            Ok(lease) => lease,
            Err(error) => {
                crate::log(&format!(
                    "job {}: production lent no runner VM: {error}",
                    job.id
                ));
                // A refused acquisition leaves nothing; its job stays queued.
                self.finish(job.id);
                return;
            }
        };
        self.update(job.id, |entry| entry.acquired = true);
        self.record(
            job.id,
            &json!({"job": job.id, "vm": lease.vm, "purpose": purpose}),
        );
        let mut runner_id = None;
        let outcome = production::check_ci(&lease).and_then(|()| {
            crate::log(&format!(
                "job {}: runner VM {} is running with the ci pack",
                job.id, lease.vm
            ));
            self.serve(
                job,
                need,
                &lease,
                source.as_ref(),
                &mut runner_id,
                endpoints,
            )
        });
        match &outcome {
            Ok(reason) => crate::log(&format!("job {}: {reason}", job.id)),
            Err(error) => crate::log(&format!("job {}: {error}", job.id)),
        }
        if let Err(error) = self.production.release(&lease.vm, "lab job ended") {
            crate::log(&format!("job {}: release of {}: {error}", job.id, lease.vm));
        } else {
            crate::log(&format!("job {}: runner VM {} released", job.id, lease.vm));
        }
        if let Some(id) = runner_id {
            if let Err(error) = source.delete_runner(id) {
                crate::log(&format!(
                    "job {}: runner {id} was not removed: {error}",
                    job.id
                ));
                self.finish(job.id);
                return;
            }
        }
        let _ = std::fs::remove_file(self.jobs.join(format!("{}.json", job.id)));
        self.finish(job.id);
    }

    fn finish(&self, job: u64) {
        self.ours
            .lock()
            .expect("ours")
            .retain(|entry| entry.job != job);
        self.reach.lock().expect("reach").remove(&job);
        self.write_table();
    }

    /// Install the runner, hand it its plan and its JIT config, and run it
    /// until it exits, idles too long or reaches its deadline.
    fn serve(
        &self,
        job: &Job,
        need: Need,
        lease: &Lease,
        source: &dyn JobSource,
        runner_id: &mut Option<u64>,
        endpoints: &[Endpoint],
    ) -> Result<String, String> {
        let (archive, sha) = self.archives.archive(need.kind)?;
        let path = |name: &str| runner::guest_path(name).display().to_string();
        self.production
            .put_file(lease, &path("runner.tar.gz"), &archive)?;
        let plan = lab::plan(endpoints, job.id, job.run_id);
        self.production.put_file(
            lease,
            &path("lab-plan.json"),
            serde_json::to_string_pretty(&plan)
                .unwrap_or_default()
                .as_bytes(),
        )?;
        let hosts: Vec<(String, Vec<String>)> = endpoints
            .iter()
            .map(|endpoint| (endpoint.address.to_string(), vec![endpoint.name.clone()]))
            .collect();
        self.production.put_file(
            lease,
            &path("hosts"),
            runner::hosts_lines(&hosts).as_bytes(),
        )?;
        let name = decide::runner_name(&self.tag, job.id);
        let jit = source.jit_config(&name, &job.labels)?;
        *runner_id = Some(jit.runner_id);
        self.update(job.id, |entry| entry.runner = Some(name.clone()));
        self.record(
            job.id,
            &json!({"job": job.id, "vm": lease.vm, "purpose": lease.purpose, "runner_id": jit.runner_id}),
        );
        self.production
            .put_file(lease, &path("jit"), jit.encoded.as_bytes())?;
        drop(jit);

        // The watcher releases the VM at the deadline or when the runner
        // took no job in time; the release ends the exec below.
        let done = Arc::new(AtomicBool::new(false));
        let why = Arc::new(Mutex::new(None::<String>));
        let watcher = {
            let done = Arc::clone(&done);
            let why = Arc::clone(&why);
            let production = Arc::clone(&self.production);
            let vm = lease.vm.clone();
            let every = self.watch_every;
            let mut runners = self.runner_probe(name);
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut took = false;
                while !done.load(Ordering::SeqCst) {
                    std::thread::sleep(every.min(Duration::from_millis(250)));
                    if !took {
                        took = runners().unwrap_or(false);
                    }
                    if decide::expired(start.elapsed(), took) {
                        let reason = if took {
                            "deadline"
                        } else {
                            "runner took no job"
                        };
                        *why.lock().expect("why") = Some(reason.to_string());
                        let _ = production.release(&vm, reason);
                        return;
                    }
                }
            })
        };
        let result = self.production.exec(
            lease,
            &runner::start_script(need.kind, &sha),
            decide::DEADLINE.as_secs(),
        );
        done.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        if let Some(reason) = why.lock().expect("why").clone() {
            return Err(format!("released early: {reason}"));
        }
        match result {
            Ok((0, _)) => Ok("the runner finished its job".to_string()),
            Ok((rc, output)) => Err(format!(
                "the runner exited with {rc}: {}",
                output
                    .lines()
                    .rev()
                    .take(5)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(" | ")
            )),
            Err(error) => Err(error),
        }
    }

    /// Whether the runner `name` has taken a job, read at most every
    /// `watch_every`.
    fn runner_probe(&self, name: String) -> Box<dyn FnMut() -> Option<bool> + Send> {
        let every = self.watch_every;
        let source = (self.source)().ok();
        let mut last: Option<Instant> = None;
        Box::new(move || {
            if last.is_some_and(|at| at.elapsed() < every) {
                return None;
            }
            last = Some(Instant::now());
            let runners = source.as_ref()?.runners().ok()?;
            Some(
                runners
                    .iter()
                    .any(|runner| runner.name == name && runner.busy),
            )
        })
    }
}
