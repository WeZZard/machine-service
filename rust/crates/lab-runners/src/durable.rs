//! The durable Mac's controller (LR-10): nothing but a poll of GitHub's API
//! that starts this Mac's test environment for the duration of a run its
//! owner dispatched by hand, and stops it after. It listens on nothing; the
//! test environment's two ports are the only door, and only while such a run
//! has a durable cell left.

use std::sync::Arc;
use std::time::Duration;

use crate::dispatcher::SourceFactory;
use crate::environment::Environments;
use crate::github::{JobSource, Run};
use crate::{runs, Told, REPOSITORY};

/// A durable Mac's controller.
pub struct Controller {
    source: SourceFactory,
    environments: Arc<Environments>,
    owner: Option<String>,
    told: Told,
}

impl Controller {
    /// A controller that serves `owner`'s dispatched runs with
    /// `environments`. Without an owner it serves nothing.
    pub fn new(
        source: SourceFactory,
        environments: Arc<Environments>,
        owner: Option<String>,
    ) -> Controller {
        Controller {
            source,
            environments,
            owner,
            told: Told::default(),
        }
    }

    /// Whether it runs no test environment.
    pub fn idle(&self) -> bool {
        self.environments.current().is_none()
    }

    /// Stop every test environment an earlier controller left (LR-6).
    pub fn recover(&self) {
        self.environments.retire(&|_| false);
    }

    /// The run this Mac serves now, as GitHub reports the runs.
    fn wanted(&self, source: &dyn JobSource, owner: &str) -> Result<Option<Run>, String> {
        let mut candidates = Vec::new();
        for run in source.active_runs()? {
            if runs::owner_dispatched(&run, REPOSITORY, Some(owner)).is_err() {
                continue;
            }
            let jobs = source.jobs(run.id)?;
            candidates.push((run, jobs));
        }
        Ok(runs::durable_run(&candidates, REPOSITORY, Some(owner)).cloned())
    }

    /// One round: start the test environment of the run this Mac serves,
    /// and stop any other.
    pub fn round(&self) {
        let Some(owner) = self.owner.as_deref() else {
            self.told.say(
                &format!(
                    "serving no run: no owner login in {}; a durable Mac serves only its owner's dispatched runs",
                    crate::lab::CONFIG
                ),
                Duration::from_secs(3600),
            );
            self.environments.retire(&|_| false);
            return;
        };
        let source = match (self.source)() {
            Ok(source) => source,
            Err(error) => {
                self.told.say(
                    &format!("serving no run: {error}"),
                    Duration::from_secs(3600),
                );
                // Without GitHub, a test environment lives out its lifetime.
                self.environments.retire(&|_| true);
                return;
            }
        };
        let wanted = match self.wanted(source.as_ref(), owner) {
            Ok(wanted) => wanted,
            Err(error) => {
                crate::log(&format!("round: {error}"));
                self.environments.retire(&|_| true);
                return;
            }
        };
        let wanted_id = wanted.as_ref().map(|run| run.id);
        self.environments.retire(&|run| Some(run) == wanted_id);
        if let Some(run) = wanted {
            if self.environments.current().map(|current| current.run) != Some(run.id) {
                crate::log(&format!(
                    "run {}: its owner dispatched it with durable cells; starting this Mac's test environment",
                    run.id
                ));
                if let Err(error) = self.environments.ensure(&run, source.as_ref()) {
                    crate::log(&format!("run {}: {error}", run.id));
                }
            }
        }
    }
}
