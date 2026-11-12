//! Where lab jobs come from: GitHub's documented REST API behind one
//! interface, so a runner scale-set source could replace it (LR-2,
//! DC-265), and where a run's branch build comes from (LR-6).

use std::time::Duration;

use serde_json::{json, Value};

use crate::token::Token;

/// A job of a workflow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The job's ID.
    pub id: u64,
    /// Its workflow run's ID.
    pub run_id: u64,
    /// Its `runs-on` labels.
    pub labels: Vec<String>,
    /// `queued`, `in_progress`, `completed` or another of GitHub's states.
    pub status: String,
}

impl Job {
    /// Whether the job waits for a runner.
    pub fn queued(&self) -> bool {
        self.status == "queued"
    }

    /// Whether the job has not ended.
    pub fn active(&self) -> bool {
        self.status != "completed"
    }
}

/// A workflow run, as GitHub's API reports it: what started it and from
/// which repository, never what the repository says of itself (LR-11).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    /// The run's ID.
    pub id: u64,
    /// The event that started it: `push`, `workflow_dispatch`, ...
    pub event: String,
    /// The workflow file, such as `.github/workflows/lab.yml`.
    pub path: String,
    /// The commit it runs.
    pub head_sha: String,
    /// The repository the commit came from: a fork's for a fork's pull
    /// request.
    pub head_repository: String,
    /// The account whose action started the run.
    pub actor: String,
    /// The account that started this attempt, which differs from
    /// [`Run::actor`] on a re-run.
    pub triggering_actor: String,
    /// `queued`, `in_progress`, `completed`, ...
    pub status: String,
}

impl Run {
    /// Whether the run has ended.
    pub fn completed(&self) -> bool {
        self.status == "completed"
    }
}

/// An artifact of a run, downloaded.
pub struct Artifact {
    /// The zip GitHub serves.
    pub bytes: Vec<u8>,
    /// GitHub's digest of the zip, `sha256:<hex>`, when it reports one.
    pub digest: Option<String>,
    /// The run GitHub says made it.
    pub run_id: u64,
    /// The commit GitHub says that run ran.
    pub head_sha: String,
}

/// A registered self-hosted runner of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// Its ID.
    pub id: u64,
    /// Its name.
    pub name: String,
    /// Whether it runs a job.
    pub busy: bool,
    /// `online` or `offline`.
    pub status: String,
    /// Its labels.
    pub labels: Vec<String>,
}

/// A just-in-time runner configuration: good for one runner, once.
pub struct JitConfig {
    /// The runner GitHub registered for it.
    pub runner_id: u64,
    /// The configuration the runner takes as `ACTIONS_RUNNER_INPUT_JITCONFIG`.
    pub encoded: String,
}

impl std::fmt::Debug for JitConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "JitConfig {{ runner_id: {}, encoded: [redacted] }}",
            self.runner_id
        )
    }
}

/// Where lab jobs and their runs' builds come from, and where their runners
/// are registered.
pub trait JobSource: Send + Sync {
    /// The repository's runs that have not ended: queued or in progress,
    /// since a run with one job started still has queued jobs.
    fn active_runs(&self) -> Result<Vec<Run>, String>;
    /// One run, or `None` when GitHub knows no such run.
    fn run(&self, id: u64) -> Result<Option<Run>, String>;
    /// The jobs of a run's latest attempt, whatever their state.
    fn jobs(&self, run: u64) -> Result<Vec<Job>, String>;
    /// The repository's self-hosted runners.
    fn runners(&self) -> Result<Vec<Runner>, String>;
    /// A JIT configuration for one runner named `name` with exactly `labels`.
    fn jit_config(&self, name: &str, labels: &[String]) -> Result<JitConfig, String>;
    /// Remove a registered runner.
    fn delete_runner(&self, id: u64) -> Result<(), String>;
    /// Download the artifact `name` of run `run` (Actions read).
    fn artifact(&self, run: u64, name: &str) -> Result<Artifact, String>;
}

/// The REST API of `base` (`https://api.github.com`) for `repository`,
/// authenticated by a token read fresh for each source.
pub struct Rest {
    base: String,
    repository: String,
    token: Token,
    agent: ureq::Agent,
}

/// The default API.
pub const API: &str = "https://api.github.com";

/// The largest artifact the controller downloads.
pub const ARTIFACT_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

const USER_AGENT: &str = "vm-service-lab-host";

impl Rest {
    /// A source for `repository` at `base` with `token`.
    pub fn new(base: &str, repository: &str, token: Token) -> Rest {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into();
        Rest {
            base: base.trim_end_matches('/').to_string(),
            repository: repository.to_string(),
            token,
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/repos/{}{path}", self.base, self.repository)
    }

    fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let authorization = format!("Bearer {}", self.token.reveal());
        macro_rules! with_headers {
            ($request:expr) => {
                $request
                    .header("Authorization", &authorization)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .header("User-Agent", USER_AGENT)
            };
        }
        let response = match method {
            "GET" => with_headers!(self.agent.get(url)).call(),
            "DELETE" => with_headers!(self.agent.delete(url)).call(),
            _ => with_headers!(self.agent.post(url))
                .header("Content-Type", "application/json")
                .send(body.map(Value::to_string).unwrap_or_default()),
        };
        // An error names the request's path, never its headers.
        let path = url.strip_prefix(&self.base).unwrap_or(url);
        let mut response = response.map_err(|error| format!("GitHub {method} {path}: {error}"))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(16 * 1024 * 1024)
            .read_to_string()
            .unwrap_or_default();
        let value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        if !(200..300).contains(&status) {
            let message = value.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(format!("GitHub {method} {path}: HTTP {status} {message}"));
        }
        Ok((status, value))
    }

    /// GET `url` with the token, and follow GitHub's one redirect to its
    /// storage without it: the storage's URL is signed, and the token is
    /// for GitHub alone.
    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(1800)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        let path = url.strip_prefix(&self.base).unwrap_or(url).to_string();
        let response = agent
            .get(url)
            .header("Authorization", &format!("Bearer {}", self.token.reveal()))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|error| format!("GitHub GET {path}: {error}"))?;
        let status = response.status().as_u16();
        let mut response = if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| format!("GitHub GET {path}: HTTP {status} without a location"))?
                .to_string();
            agent
                .get(&location)
                .header("User-Agent", USER_AGENT)
                .call()
                .map_err(|error| format!("the artifact's storage: {error}"))?
        } else {
            response
        };
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("artifact download {path}: HTTP {status}"));
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut response
                .body_mut()
                .with_config()
                .limit(ARTIFACT_LIMIT)
                .reader(),
            &mut bytes,
        )
        .map_err(|error| format!("artifact download {path}: {error}"))?;
        Ok(bytes)
    }
}

fn text(value: &Value, path: &[&str]) -> String {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The jobs of one run's `jobs` answer.
pub fn parse_jobs(run_id: u64, value: &Value) -> Vec<Job> {
    value
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|job| {
            Some(Job {
                id: job.get("id")?.as_u64()?,
                run_id,
                labels: strings(job.get("labels")?),
                status: text(job, &["status"]),
            })
        })
        .collect()
}

/// A run from a run object of GitHub's API.
pub fn parse_run(run: &Value) -> Option<Run> {
    Some(Run {
        id: run.get("id")?.as_u64()?,
        event: text(run, &["event"]),
        path: text(run, &["path"]),
        head_sha: text(run, &["head_sha"]),
        head_repository: text(run, &["head_repository", "full_name"]),
        actor: text(run, &["actor", "login"]),
        triggering_actor: text(run, &["triggering_actor", "login"]),
        status: text(run, &["status"]),
    })
}

/// The runs of a `runs` answer.
pub fn parse_runs(value: &Value) -> Vec<Run> {
    value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_run)
        .collect()
}

/// The artifact named `name` of a run's `artifacts` answer that has not
/// expired: its ID, GitHub's digest, and the run and commit GitHub says
/// made it.
pub fn parse_artifact(value: &Value, name: &str) -> Option<(u64, Option<String>, u64, String)> {
    value
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("name").and_then(Value::as_str) == Some(name))
        .filter(|item| item.get("expired").and_then(Value::as_bool) != Some(true))
        .find_map(|item| {
            Some((
                item.get("id")?.as_u64()?,
                item.get("digest")
                    .and_then(Value::as_str)
                    .filter(|digest| !digest.is_empty())
                    .map(str::to_string),
                item.get("workflow_run")?.get("id")?.as_u64()?,
                text(item, &["workflow_run", "head_sha"]),
            ))
        })
}

/// The runners of a `runners` answer.
pub fn parse_runners(value: &Value) -> Vec<Runner> {
    value
        .get("runners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|runner| {
            Some(Runner {
                id: runner.get("id")?.as_u64()?,
                name: runner.get("name")?.as_str()?.to_string(),
                busy: runner.get("busy").and_then(Value::as_bool).unwrap_or(false),
                status: runner
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("offline")
                    .to_string(),
                labels: runner
                    .get("labels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// The body of `generate-jitconfig`: the repository's default runner group,
/// exactly the job's labels, and the runner's work folder.
pub fn jit_body(name: &str, labels: &[String]) -> Value {
    json!({
        "name": name,
        "runner_group_id": 1,
        "labels": labels,
        "work_folder": "_work",
    })
}

impl JobSource for Rest {
    fn active_runs(&self) -> Result<Vec<Run>, String> {
        let mut runs: Vec<Run> = Vec::new();
        for status in ["queued", "in_progress"] {
            let (_, answer) = self.request(
                "GET",
                &self.url(&format!("/actions/runs?status={status}&per_page=100")),
                None,
            )?;
            for run in parse_runs(&answer) {
                if !runs.iter().any(|seen| seen.id == run.id) {
                    runs.push(run);
                }
            }
        }
        runs.sort_by_key(|run| run.id);
        Ok(runs)
    }

    fn run(&self, id: u64) -> Result<Option<Run>, String> {
        match self.request("GET", &self.url(&format!("/actions/runs/{id}")), None) {
            Ok((_, answer)) => Ok(parse_run(&answer)),
            Err(error) if error.contains("HTTP 404") => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn jobs(&self, run: u64) -> Result<Vec<Job>, String> {
        let (_, answer) = self.request(
            "GET",
            &self.url(&format!(
                "/actions/runs/{run}/jobs?filter=latest&per_page=100"
            )),
            None,
        )?;
        Ok(parse_jobs(run, &answer))
    }

    fn runners(&self) -> Result<Vec<Runner>, String> {
        let (_, answer) = self.request("GET", &self.url("/actions/runners?per_page=100"), None)?;
        Ok(parse_runners(&answer))
    }

    fn jit_config(&self, name: &str, labels: &[String]) -> Result<JitConfig, String> {
        let (_, answer) = self.request(
            "POST",
            &self.url("/actions/runners/generate-jitconfig"),
            Some(&jit_body(name, labels)),
        )?;
        let runner_id = answer
            .get("runner")
            .and_then(|runner| runner.get("id"))
            .and_then(Value::as_u64)
            .ok_or("GitHub's JIT answer names no runner")?;
        let encoded = answer
            .get("encoded_jit_config")
            .and_then(Value::as_str)
            .filter(|encoded| !encoded.is_empty())
            .ok_or("GitHub's JIT answer has no configuration")?
            .to_string();
        Ok(JitConfig { runner_id, encoded })
    }

    fn delete_runner(&self, id: u64) -> Result<(), String> {
        match self.request("DELETE", &self.url(&format!("/actions/runners/{id}")), None) {
            Ok(_) => Ok(()),
            // Gone already: a JIT runner deregisters itself after its job.
            Err(error) if error.contains("HTTP 404") => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn artifact(&self, run: u64, name: &str) -> Result<Artifact, String> {
        let (_, list) = self.request(
            "GET",
            &self.url(&format!(
                "/actions/runs/{run}/artifacts?name={name}&per_page=100"
            )),
            None,
        )?;
        let (id, digest, run_id, head_sha) = parse_artifact(&list, name)
            .ok_or_else(|| format!("run {run} has no artifact {name}"))?;
        let bytes = self.download(&self.url(&format!("/actions/artifacts/{id}/zip")))?;
        Ok(Artifact {
            bytes,
            digest,
            run_id,
            head_sha,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_jobs_and_runners_are_read_from_githubs_answers() {
        let runs = json!({"total_count": 2, "workflow_runs": [
            {"id": 11, "event": "workflow_dispatch", "path": ".github/workflows/lab.yml", "head_sha": "abc",
             "head_repository": {"full_name": "o/r"}, "actor": {"login": "a"}, "triggering_actor": {"login": "b"},
             "status": "queued"},
            {"id": 12}]});
        let parsed = parse_runs(&runs);
        assert_eq!(
            parsed.iter().map(|run| run.id).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert_eq!(
            parsed[0],
            Run {
                id: 11,
                event: "workflow_dispatch".into(),
                path: ".github/workflows/lab.yml".into(),
                head_sha: "abc".into(),
                head_repository: "o/r".into(),
                actor: "a".into(),
                triggering_actor: "b".into(),
                status: "queued".into(),
            }
        );
        assert_eq!(parsed[1].event, "");
        let jobs = json!({"jobs": [
            {"id": 1, "status": "queued", "labels": ["self-hosted", "lab-macos"]},
            {"id": 2, "status": "in_progress", "labels": ["self-hosted", "lab-macos"]},
            {"id": 3, "status": "completed", "labels": ["ubuntu-latest"]}
        ]});
        let parsed = parse_jobs(11, &jobs);
        assert_eq!(
            parsed
                .iter()
                .map(|job| (job.id, job.queued(), job.active()))
                .collect::<Vec<_>>(),
            vec![(1, true, true), (2, false, true), (3, false, false)]
        );
        assert_eq!(parsed[0].labels, vec!["self-hosted", "lab-macos"]);
        let runners = json!({"runners": [{"id": 5, "name": "lab-a-1", "status": "online", "busy": true,
            "labels": [{"id": 1, "name": "self-hosted", "type": "read-only"}, {"id": 2, "name": "lab-macos", "type": "custom"}]}]});
        assert_eq!(
            parse_runners(&runners),
            vec![Runner {
                id: 5,
                name: "lab-a-1".into(),
                busy: true,
                status: "online".into(),
                labels: vec!["self-hosted".into(), "lab-macos".into()],
            }]
        );
    }

    #[test]
    fn an_artifact_is_found_by_name_and_names_its_run() {
        let list = json!({"artifacts": [
            {"id": 1, "name": "other", "workflow_run": {"id": 5, "head_sha": "x"}},
            {"id": 2, "name": "lab-test-environment", "expired": true, "workflow_run": {"id": 5, "head_sha": "x"}},
            {"id": 3, "name": "lab-test-environment", "digest": "sha256:ab", "workflow_run": {"id": 5, "head_sha": "x"}}
        ]});
        assert_eq!(
            parse_artifact(&list, "lab-test-environment"),
            Some((3, Some("sha256:ab".into()), 5, "x".into()))
        );
        assert_eq!(parse_artifact(&list, "missing"), None);
    }

    #[test]
    fn a_jit_runner_gets_exactly_the_jobs_labels_and_its_config_never_prints() {
        let labels = vec!["self-hosted".to_string(), "lab-ubuntu".to_string()];
        assert_eq!(
            jit_body("lab-h-1", &labels),
            json!({"name": "lab-h-1", "runner_group_id": 1, "labels": ["self-hosted", "lab-ubuntu"], "work_folder": "_work"})
        );
        let config = JitConfig {
            runner_id: 3,
            encoded: "c2VjcmV0".into(),
        };
        assert!(!format!("{config:?}").contains("c2VjcmV0"));
    }
}
