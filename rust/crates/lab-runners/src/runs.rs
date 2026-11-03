//! Which workflow runs the runner controller serves, and which a durable
//! Mac's controller starts a test environment for (LR-6, LR-10, LR-11). Every
//! check reads what GitHub's API reports of the run, never what the
//! repository's files say.

use crate::github::{Job, Run};
use crate::labels;

/// The lab's workflow file.
pub const WORKFLOW: &str = ".github/workflows/lab.yml";

/// The events whose runs the runner controller serves: a push to the
/// repository and a dispatch by hand (DC-263). A pull request's run, a fork's
/// above all, never gets a runner VM or a test environment.
pub const EVENTS: [&str; 2] = ["push", "workflow_dispatch"];

/// The only event whose run a durable Mac serves (DC-263, LR-10).
pub const DISPATCH: &str = "workflow_dispatch";

/// Whether the runner controller serves `run` of `repository`: the lab's
/// workflow, started by a push or a dispatch, from the repository itself.
/// Its test environment runs the run's own build on the host, so a run from
/// any other workflow, event or repository is never served.
pub fn eligible(run: &Run, repository: &str) -> Result<(), String> {
    if run.path != WORKFLOW {
        return Err(format!("run {} is not of {WORKFLOW}", run.id));
    }
    if !EVENTS.contains(&run.event.as_str()) {
        return Err(format!(
            "run {} was started by {:?}, not a push or a dispatch",
            run.id, run.event
        ));
    }
    if !run.head_repository.eq_ignore_ascii_case(repository) {
        return Err(format!("run {} is not from {repository}", run.id));
    }
    Ok(())
}

/// Whether a durable Mac serves `run`: an eligible run that `owner`, the
/// login of the controller's local configuration, dispatched by hand, and
/// whose attempt the owner started too. Without a configured owner nothing
/// is served.
pub fn owner_dispatched(run: &Run, repository: &str, owner: Option<&str>) -> Result<(), String> {
    eligible(run, repository)?;
    let owner = owner.ok_or("no owner login is configured on this Mac")?;
    if run.event != DISPATCH {
        return Err(format!(
            "run {} was started by {:?}, not dispatched by hand",
            run.id, run.event
        ));
    }
    let same = |login: &str| !login.is_empty() && login.eq_ignore_ascii_case(owner);
    if !same(&run.actor) || !same(&run.triggering_actor) {
        return Err(format!(
            "run {} was not dispatched by this Mac's owner",
            run.id
        ));
    }
    Ok(())
}

/// Whether a job still needs a durable Mac's test environment.
pub fn wants_durable(job: &Job) -> bool {
    job.active() && labels::need(&job.labels).is_some_and(|need| need.durable)
}

/// The durable run a durable Mac serves now: the oldest owner-dispatched
/// run with a durable job that has not ended, or none.
pub fn durable_run<'a>(
    runs: &'a [(Run, Vec<Job>)],
    repository: &str,
    owner: Option<&str>,
) -> Option<&'a Run> {
    runs.iter()
        .filter(|(run, _)| owner_dispatched(run, repository, owner).is_ok())
        .filter(|(_, jobs)| jobs.iter().any(wants_durable))
        .map(|(run, _)| run)
        .min_by_key(|run| run.id)
}

/// The second-instance ID of run `run`'s test environment: `r` and the
/// run's ID, at most 16 characters (DC-273), so its VMs are `si-r<run>-…`.
pub fn environment_id(run: u64) -> String {
    let digits = run.to_string();
    let tail = &digits[digits.len().saturating_sub(15)..];
    format!("r{tail}")
}

/// Whether a login is one GitHub could have issued.
pub fn valid_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && !login.starts_with('-')
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "o/r";

    fn run(id: u64, event: &str, actor: &str) -> Run {
        Run {
            id,
            event: event.into(),
            path: WORKFLOW.into(),
            head_sha: "abc".into(),
            head_repository: REPO.into(),
            actor: actor.into(),
            triggering_actor: actor.into(),
            status: "queued".into(),
        }
    }

    fn job(labels: &[&str], status: &str) -> Job {
        Job {
            id: 1,
            run_id: 1,
            labels: labels.iter().map(|label| label.to_string()).collect(),
            status: status.into(),
        }
    }

    #[test]
    fn only_the_lab_workflows_pushes_and_dispatches_from_the_repository_are_served() {
        assert!(eligible(&run(1, "push", "a"), REPO).is_ok());
        assert!(eligible(&run(1, "workflow_dispatch", "a"), REPO).is_ok());
        for event in ["pull_request", "pull_request_target", "schedule", ""] {
            assert!(eligible(&run(1, event, "a"), REPO).is_err(), "{event}");
        }
        let fork = Run {
            head_repository: "someone/r".into(),
            ..run(1, "push", "a")
        };
        assert!(eligible(&fork, REPO).is_err());
        let other = Run {
            path: ".github/workflows/ci.yml".into(),
            ..run(1, "push", "a")
        };
        assert!(eligible(&other, REPO).is_err());
    }

    #[test]
    fn a_durable_mac_serves_only_its_owners_dispatch() {
        let owner = Some("Owner");
        assert!(owner_dispatched(&run(1, "workflow_dispatch", "owner"), REPO, owner).is_ok());
        // A push, even the owner's.
        assert!(owner_dispatched(&run(1, "push", "owner"), REPO, owner).is_err());
        // Someone else's dispatch.
        assert!(owner_dispatched(&run(1, "workflow_dispatch", "other"), REPO, owner).is_err());
        // The owner's run re-run by someone else.
        let rerun = Run {
            triggering_actor: "other".into(),
            ..run(1, "workflow_dispatch", "owner")
        };
        assert!(owner_dispatched(&rerun, REPO, owner).is_err());
        // No owner configured: nothing.
        let error =
            owner_dispatched(&run(1, "workflow_dispatch", "owner"), REPO, None).unwrap_err();
        assert!(error.contains("no owner"), "{error}");
        // An empty actor never matches.
        assert!(owner_dispatched(&run(1, "workflow_dispatch", ""), REPO, Some("")).is_err());
        // A fork's dispatch cannot exist, and is refused all the same.
        let fork = Run {
            head_repository: "x/r".into(),
            ..run(1, "workflow_dispatch", "owner")
        };
        assert!(owner_dispatched(&fork, REPO, owner).is_err());
    }

    #[test]
    fn the_durable_run_is_the_oldest_owner_dispatch_with_a_durable_job_left() {
        let durable = ["self-hosted", "lab-macos", "lab-durable-target"];
        let vm = ["self-hosted", "lab-macos"];
        let runs = vec![
            (run(5, "push", "owner"), vec![job(&durable, "queued")]),
            (
                run(6, "workflow_dispatch", "other"),
                vec![job(&durable, "queued")],
            ),
            (
                run(7, "workflow_dispatch", "owner"),
                vec![job(&vm, "queued")],
            ),
            (
                run(8, "workflow_dispatch", "owner"),
                vec![job(&durable, "completed")],
            ),
            (
                run(9, "workflow_dispatch", "owner"),
                vec![job(&durable, "in_progress")],
            ),
            (
                run(10, "workflow_dispatch", "owner"),
                vec![job(&durable, "queued")],
            ),
        ];
        assert_eq!(
            durable_run(&runs, REPO, Some("owner")).map(|run| run.id),
            Some(9)
        );
        assert_eq!(durable_run(&runs, REPO, None), None);
        assert_eq!(durable_run(&runs[..4], REPO, Some("owner")), None);
    }

    #[test]
    fn a_runs_test_environment_is_named_for_the_run() {
        assert_eq!(environment_id(19283746501), "r19283746501");
        let long = environment_id(u64::MAX);
        assert_eq!(long.len(), 16);
        assert!(second_instance::valid_id(&long));
        assert!(second_instance::valid_id(&environment_id(7)));
    }

    #[test]
    fn logins_are_checked() {
        assert!(valid_login("Some-One1"));
        assert!(!valid_login(""));
        assert!(!valid_login("-x"));
        assert!(!valid_login("a b"));
        assert!(!valid_login(&"a".repeat(40)));
    }
}
