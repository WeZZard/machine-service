//! The controller's decisions, as functions over values (LR-2, LR-3, LR-7).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::github::{Job, Runner};
use crate::labels::{self, Need};
use crate::production::Capacity;

/// Every lab runner's name starts with it, so the controller tells its
/// runners from any other in the repository's list.
pub const RUNNER_PREFIX: &str = "lab-";

/// A runner that has not taken a job this long after it started is
/// released: the second runner when two controllers saw one job (LR-2).
pub const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// A runner VM is released this long after its runner started, whatever
/// the job does: past GitHub's 6-hour job limit (LR-3).
pub const DEADLINE: Duration = Duration::from_secs(6 * 3600 + 30 * 60);

/// A runner VM's lease TTL, in hours: past the deadline, so a controller
/// that died leaves a VM production reclaims (LR-3).
pub const TTL_HOURS: f64 = 7.0;

/// One of this controller's jobs, between its start and its release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ours {
    /// The job it was started for.
    pub job: u64,
    /// The job's run.
    pub run: u64,
    /// The job's labels, as [`labels::key`] gives them.
    pub key: Vec<String>,
    /// Its runner's name, once its JIT config was made.
    pub runner: Option<String>,
    /// Whether its runner VM is leased, so production counts its slot.
    pub acquired: bool,
    /// What it needs.
    pub need: Need,
}

/// The name of a job's runner on host `tag`: unique per job and host.
pub fn runner_name(tag: &str, job: u64) -> String {
    format!("{RUNNER_PREFIX}{tag}-{job}")
}

/// The purpose of a job's runner VM lease, by which a restarted controller
/// finds what an earlier one left (LR-3).
pub fn purpose(job: u64) -> String {
    format!("lab-{job}")
}

/// The job a runner VM's purpose names, if it is one of the lab's.
pub fn job_of_purpose(purpose: &str) -> Option<u64> {
    purpose.strip_prefix("lab-")?.parse().ok()
}

/// The macOS slots this controller's jobs will take that production does
/// not count yet: a runner VM's until it is leased, and a target's
/// for the job's whole life, because the job leases it itself (LR-7).
pub fn reserved(ours: &[Ours]) -> u32 {
    ours.iter()
        .map(|job| {
            let target = job.need.macos_slots - job.need.runner_slots;
            let runner = if job.acquired {
                0
            } else {
                job.need.runner_slots
            };
            runner + target
        })
        .sum()
}

/// Production's capacity with the test environment's own macOS guests,
/// which production counts as another program's, taken out of the foreign
/// ones: the lab yields to an image build, not to its own targets (LR-7).
pub fn without_test_environment(capacity: Capacity, environment_running: u32) -> Capacity {
    Capacity {
        foreign: capacity.foreign.saturating_sub(environment_running),
        ..capacity
    }
}

/// The queued jobs this host starts now, oldest first (LR-2, LR-7):
///
/// - only lab jobs ([`labels::need`]) that are not already ours;
/// - per label set, no more than its queued jobs less its **pending**
///   runners: a lab runner that is registered and not busy (any host's),
///   and each of ours whose runner is not registered yet;
/// - each only while the host has its macOS slots, counting every guest the
///   host runs and the slots ours will take ([`reserved`]), and none
///   that needs a macOS slot while another program's macOS guest runs.
pub fn to_start(
    jobs: &[Job],
    runners: &[Runner],
    capacity: Capacity,
    ours: &[Ours],
) -> Vec<(Job, Need)> {
    let mut pending: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    for runner in runners {
        if runner.name.starts_with(RUNNER_PREFIX) && !runner.busy {
            *pending.entry(labels::key(&runner.labels)).or_default() += 1;
        }
    }
    for job in ours {
        let registered = job
            .runner
            .as_ref()
            .is_some_and(|name| runners.iter().any(|runner| &runner.name == name));
        if !registered {
            *pending.entry(job.key.clone()).or_default() += 1;
        }
    }
    let mut queued: Vec<&Job> = jobs
        .iter()
        .filter(|job| !ours.iter().any(|mine| mine.job == job.id))
        .collect();
    queued.sort_by_key(|job| job.id);
    let mut free = capacity
        .limit
        .saturating_sub(capacity.taken)
        .saturating_sub(reserved(ours));
    let mut starts = Vec::new();
    for job in queued {
        let Some(need) = labels::need(&job.labels) else {
            continue;
        };
        let key = labels::key(&job.labels);
        let waiting = pending.entry(key).or_default();
        if *waiting > 0 {
            // A pending runner takes this job.
            *waiting -= 1;
            continue;
        }
        if need.macos_slots > 0 && (capacity.foreign > 0 || free < need.macos_slots) {
            continue;
        }
        free -= need.macos_slots;
        starts.push((job.clone(), need));
    }
    starts
}

/// Whether a started runner VM is released now: at the deadline, or when
/// its runner has taken no job within [`IDLE_LIMIT`].
pub fn expired(started: Duration, took_job: bool) -> bool {
    started >= DEADLINE || (!took_job && started >= IDLE_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64, labels: &[&str]) -> Job {
        Job {
            id,
            run_id: 1,
            labels: labels.iter().map(|label| label.to_string()).collect(),
            status: "queued".into(),
        }
    }

    fn runner(name: &str, busy: bool, labels: &[&str]) -> Runner {
        Runner {
            id: 9,
            name: name.into(),
            busy,
            status: "online".into(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
        }
    }

    const MAC: [&str; 2] = ["self-hosted", "lab-macos"];
    const MAC_TARGET: [&str; 3] = ["self-hosted", "lab-macos", "lab-macos-target"];
    const UBUNTU: [&str; 2] = ["self-hosted", "lab-ubuntu"];

    fn room(taken: u32) -> Capacity {
        Capacity {
            limit: 2,
            taken,
            foreign: 0,
            running: 0,
        }
    }

    fn ids(starts: &[(Job, Need)]) -> Vec<u64> {
        starts.iter().map(|(job, _)| job.id).collect()
    }

    #[test]
    fn jobs_start_oldest_first_while_slots_last() {
        let jobs = [job(3, &MAC), job(1, &MAC), job(2, &MAC)];
        assert_eq!(ids(&to_start(&jobs, &[], room(0), &[])), vec![1, 2]);
        assert_eq!(ids(&to_start(&jobs, &[], room(1), &[])), vec![1]);
        assert!(to_start(&jobs, &[], room(2), &[]).is_empty());
    }

    #[test]
    fn a_target_takes_a_second_slot_and_ubuntu_runners_take_none() {
        let jobs = [
            job(1, &MAC_TARGET),
            job(2, &MAC),
            job(3, &UBUNTU),
            job(4, &UBUNTU),
        ];
        assert_eq!(ids(&to_start(&jobs, &[], room(0), &[])), vec![1, 3, 4]);
        assert_eq!(ids(&to_start(&jobs, &[], room(1), &[])), vec![2, 3, 4]);
    }

    #[test]
    fn another_programs_macos_guest_stops_every_macos_start() {
        let jobs = [job(1, &MAC), job(2, &UBUNTU)];
        let building = Capacity {
            limit: 2,
            taken: 1,
            foreign: 1,
            running: 0,
        };
        assert_eq!(ids(&to_start(&jobs, &[], building, &[])), vec![2]);
        // The test environment's own target is no other program's.
        let target = without_test_environment(building, 1);
        assert_eq!(target.foreign, 0);
        assert_eq!(ids(&to_start(&jobs, &[], target, &[])), vec![1, 2]);
        assert_eq!(without_test_environment(building, 0), building);
    }

    #[test]
    fn a_pending_runner_takes_a_job_so_none_is_started_twice() {
        let jobs = [job(1, &MAC), job(2, &MAC)];
        // Another host's runner is registered and idle: it takes one job.
        let idle = [runner("lab-other-1", false, &MAC)];
        assert_eq!(ids(&to_start(&jobs, &idle, room(0), &[])), vec![2]);
        // A busy one takes none; one that is not the lab's counts not at all.
        let busy = [
            runner("lab-other-1", true, &MAC),
            runner("mine", false, &MAC),
        ];
        assert_eq!(ids(&to_start(&jobs, &busy, room(0), &[])), vec![1, 2]);
        // An idle runner of another label set takes nothing here.
        let ubuntu = [runner("lab-other-2", false, &UBUNTU)];
        assert_eq!(ids(&to_start(&jobs, &ubuntu, room(0), &[])), vec![1, 2]);
    }

    #[test]
    fn our_jobs_count_their_slots_until_the_instance_does() {
        let need = labels::need(&MAC_TARGET.map(String::from)).unwrap();
        let starting = Ours {
            job: 7,
            run: 1,
            key: labels::key(&MAC_TARGET.map(String::from)),
            runner: None,
            acquired: false,
            need,
        };
        assert_eq!(reserved(std::slice::from_ref(&starting)), 2);
        let leased = Ours {
            acquired: true,
            ..starting.clone()
        };
        // Leased: the instance counts the runner; the target stays reserved.
        assert_eq!(reserved(std::slice::from_ref(&leased)), 1);
        // Our job is never started again, and its pending runner takes a
        // queued job of its labels.
        let jobs = [job(7, &MAC_TARGET), job(8, &MAC_TARGET)];
        assert!(to_start(
            &jobs,
            &[],
            Capacity {
                limit: 4,
                taken: 0,
                foreign: 0,
                running: 0
            },
            std::slice::from_ref(&starting)
        )
        .is_empty());
        // Once its runner is registered, the registered runner is the
        // pending one, counted once.
        let named = Ours {
            runner: Some("lab-h-7".into()),
            acquired: true,
            ..starting
        };
        let listed = [runner("lab-h-7", false, &MAC_TARGET)];
        assert!(to_start(
            &jobs,
            &listed,
            Capacity {
                limit: 4,
                taken: 1,
                foreign: 0,
                running: 0
            },
            std::slice::from_ref(&named)
        )
        .is_empty());
        let took = [runner("lab-h-7", true, &MAC_TARGET)];
        assert_eq!(
            ids(&to_start(
                &jobs,
                &took,
                Capacity {
                    limit: 4,
                    taken: 1,
                    foreign: 0,
                    running: 0
                },
                &[named]
            )),
            vec![8]
        );
    }

    #[test]
    fn a_runner_vm_ends_at_its_deadline_or_when_idle_too_long() {
        assert!(!expired(Duration::from_secs(60), false));
        assert!(expired(IDLE_LIMIT, false));
        assert!(!expired(IDLE_LIMIT, true));
        assert!(expired(DEADLINE, true));
        assert!(TTL_HOURS * 3600.0 > DEADLINE.as_secs_f64());
    }

    #[test]
    fn purposes_and_names_carry_the_job() {
        assert_eq!(purpose(42), "lab-42");
        assert_eq!(job_of_purpose("lab-42"), Some(42));
        assert_eq!(job_of_purpose("lab-x"), None);
        assert_eq!(job_of_purpose("acc-lab-42"), None);
        assert_eq!(runner_name("a1b2c3", 42), "lab-a1b2c3-42");
    }
}
