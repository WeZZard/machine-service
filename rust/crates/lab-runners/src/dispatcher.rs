//! The dispatcher: queued lab jobs to one-job runner VMs (LR-3).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::decide::{self, Ours};
use crate::github::{Job, JobSource};
use crate::instance::{self, Instance, Lease};
use crate::lab::Endpoint;
use crate::labels::{self, Kind, Need};
use crate::runner;

/// The runner archives a dispatcher pushes into its guests.
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

/// One VM host's dispatcher.
pub struct Dispatcher {
    source: SourceFactory,
    instance: Arc<dyn Instance>,
    archives: Arc<dyn Archives>,
    /// This host's tag in its runners' names.
    tag: String,
    /// `<state>/jobs`, one file per job between its start and its release.
    jobs: PathBuf,
    endpoints: Mutex<Vec<Endpoint>>,
    ours: Mutex<Vec<Ours>>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    told_missing: Mutex<Option<Instant>>,
    /// How often a job's watcher looks at its runner.
    pub watch_every: Duration,
}

impl Dispatcher {
    /// A dispatcher of `instance`'s runner VMs for the jobs `source` finds.
    pub fn new(
        source: SourceFactory,
        instance: Arc<dyn Instance>,
        archives: Arc<dyn Archives>,
        tag: &str,
        state: PathBuf,
    ) -> Arc<Dispatcher> {
        Arc::new(Dispatcher {
            source,
            instance,
            archives,
            tag: tag.to_string(),
            jobs: state.join("jobs"),
            endpoints: Mutex::new(Vec::new()),
            ours: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            told_missing: Mutex::new(None),
            watch_every: Duration::from_secs(30),
        })
    }

    /// Replace the lab endpoints that new jobs' plans name.
    pub fn set_endpoints(&self, endpoints: Vec<Endpoint>) {
        *self.endpoints.lock().expect("endpoints") = endpoints;
    }

    /// This dispatcher's jobs between start and release.
    pub fn ours(&self) -> Vec<Ours> {
        self.ours.lock().expect("ours").clone()
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
            Ok(source) => {
                *self.told_missing.lock().expect("told") = None;
                Some(source)
            }
            Err(error) => {
                let mut told = self.told_missing.lock().expect("told");
                if told.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600)) {
                    crate::log(&format!("serving no job: {error}"));
                    *told = Some(Instant::now());
                }
                None
            }
        }
    }

    /// Release what an earlier dispatcher on this host left: every lease of
    /// the lab instance whose purpose names a lab job that is not ours, and
    /// every runner its job files name (LR-3).
    pub fn recover(&self) {
        let mine: Vec<u64> = self.ours().iter().map(|job| job.job).collect();
        match self.instance.leases() {
            Ok(leases) => {
                for lease in leases {
                    if let Some(job) = decide::job_of_purpose(&lease.purpose) {
                        if !mine.contains(&job) {
                            crate::log(&format!(
                                "job {job}: releasing the runner VM {} an earlier lab-host left",
                                lease.vm
                            ));
                            if let Err(error) =
                                self.instance.release(&lease.vm, "lab-host restarted")
                            {
                                crate::log(&format!("job {job}: {error}"));
                            }
                        }
                    }
                }
            }
            Err(error) => crate::log(&format!("recovery: {error}")),
        }
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

    /// One round (LR-2): read the queued jobs, the runners and the host's
    /// capacity, and start a thread for each job [`decide::to_start`] picks.
    pub fn round(self: &Arc<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        let read = || -> Result<_, String> {
            let jobs = source.queued_jobs()?;
            let lab: Vec<Job> = jobs
                .into_iter()
                .filter(|job| labels::need(&job.labels).is_some())
                .collect();
            if lab.is_empty() {
                return Ok(None);
            }
            Ok(Some((lab, source.runners()?, self.instance.capacity()?)))
        };
        let (jobs, runners, capacity) = match read() {
            Ok(Some(read)) => read,
            Ok(None) => return,
            Err(error) => {
                crate::log(&format!("round: {error}"));
                return;
            }
        };
        let starts = {
            let mut ours = self.ours.lock().expect("ours");
            let starts = decide::to_start(&jobs, &runners, capacity, &ours);
            for (job, need) in &starts {
                ours.push(Ours {
                    job: job.id,
                    key: labels::key(&job.labels),
                    runner: None,
                    acquired: false,
                    need: *need,
                });
            }
            starts
        };
        for (job, need) in starts {
            crate::log(&format!(
                "job {} of run {}: starting a {} runner VM",
                job.id, job.run_id, need.image
            ));
            let dispatcher = Arc::clone(self);
            let source = Arc::clone(&source);
            let thread = std::thread::spawn(move || dispatcher.run_job(&job, need, source));
            self.threads.lock().expect("threads").push(thread);
        }
        self.threads
            .lock()
            .expect("threads")
            .retain(|thread| !thread.is_finished());
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
    fn run_job(&self, job: &Job, need: Need, source: Arc<dyn JobSource>) {
        let purpose = decide::purpose(job.id);
        let started = self
            .instance
            .acquire(&instance::runner_request(need.image, &purpose));
        let lease = match started {
            Ok(lease) => lease,
            Err(error) => {
                crate::log(&format!(
                    "job {}: the runner VM was not acquired: {error}",
                    job.id
                ));
                // A failed acquisition leaves nothing; its job stays queued.
                self.finish(job.id);
                return;
            }
        };
        self.update(job.id, |entry| entry.acquired = true);
        self.record(
            job.id,
            &json!({"job": job.id, "vm": lease.vm, "purpose": purpose}),
        );
        crate::log(&format!(
            "job {}: runner VM {} is running",
            job.id, lease.vm
        ));
        let mut runner_id = None;
        let outcome = self.serve(job, need, &lease, source.as_ref(), &mut runner_id);
        match &outcome {
            Ok(reason) => crate::log(&format!("job {}: {reason}", job.id)),
            Err(error) => crate::log(&format!("job {}: {error}", job.id)),
        }
        if let Err(error) = self.instance.release(&lease.vm, "lab job ended") {
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
    ) -> Result<String, String> {
        let (archive, sha) = self.archives.archive(need.kind)?;
        let path = |name: &str| runner::guest_path(name).display().to_string();
        self.instance
            .put_file(lease, &path("runner.tar.gz"), &archive)?;
        let endpoints = self.endpoints.lock().expect("endpoints").clone();
        let plan = crate::lab::plan(&endpoints, job.id, job.run_id);
        self.instance.put_file(
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
        self.instance.put_file(
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
        self.instance
            .put_file(lease, &path("jit"), jit.encoded.as_bytes())?;
        drop(jit);

        // The watcher releases the VM at the deadline or when the runner
        // took no job in time; the release ends the exec below.
        let done = Arc::new(AtomicBool::new(false));
        let why = Arc::new(Mutex::new(None::<String>));
        let watcher = {
            let done = Arc::clone(&done);
            let why = Arc::clone(&why);
            let instance = Arc::clone(&self.instance);
            let vm = lease.vm.clone();
            let every = self.watch_every;
            let name = name.clone();
            // The watcher needs the runner list; it reads it through a
            // snapshot function made from the source for this job.
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
                        let _ = instance.release(&vm, reason);
                        return;
                    }
                }
            })
        };
        let result = self.instance.exec(
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
