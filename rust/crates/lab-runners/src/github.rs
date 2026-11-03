//! Where lab jobs come from: GitHub's documented REST API behind one
//! interface, so a runner scale-set source could replace it (LR-2,
//! DC-265).

use std::time::Duration;

use serde_json::{json, Value};

use crate::token::Token;

/// A queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The job's ID.
    pub id: u64,
    /// Its workflow run's ID.
    pub run_id: u64,
    /// Its `runs-on` labels.
    pub labels: Vec<String>,
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

/// Where queued lab jobs come from and where their runners are registered.
pub trait JobSource: Send + Sync {
    /// The repository's queued jobs.
    fn queued_jobs(&self) -> Result<Vec<Job>, String>;
    /// The repository's self-hosted runners.
    fn runners(&self) -> Result<Vec<Runner>, String>;
    /// A JIT configuration for one runner named `name` with exactly `labels`.
    fn jit_config(&self, name: &str, labels: &[String]) -> Result<JitConfig, String>;
    /// Remove a registered runner.
    fn delete_runner(&self, id: u64) -> Result<(), String>;
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
                    .header("User-Agent", "vm-service-lab-host")
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
}

/// The queued jobs of one run's `jobs` answer.
pub fn parse_jobs(run_id: u64, value: &Value) -> Vec<Job> {
    value
        .get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|job| job.get("status").and_then(Value::as_str) == Some("queued"))
        .filter_map(|job| {
            Some(Job {
                id: job.get("id")?.as_u64()?,
                run_id,
                labels: strings(job.get("labels")?),
            })
        })
        .collect()
}

/// The runs of a `runs` answer.
pub fn parse_run_ids(value: &Value) -> Vec<u64> {
    value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("id")?.as_u64())
        .collect()
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
    fn queued_jobs(&self) -> Result<Vec<Job>, String> {
        let (_, runs) = self.request(
            "GET",
            &self.url("/actions/runs?status=queued&per_page=100"),
            None,
        )?;
        let mut jobs = Vec::new();
        for run in parse_run_ids(&runs) {
            let (_, answer) = self.request(
                "GET",
                &self.url(&format!(
                    "/actions/runs/{run}/jobs?filter=latest&per_page=100"
                )),
                None,
            )?;
            jobs.extend(parse_jobs(run, &answer));
        }
        Ok(jobs)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_jobs_runs_and_runners_are_read_from_githubs_answers() {
        let runs = json!({"total_count": 2, "workflow_runs": [{"id": 11}, {"id": 12}]});
        assert_eq!(parse_run_ids(&runs), vec![11, 12]);
        let jobs = json!({"jobs": [
            {"id": 1, "status": "queued", "labels": ["self-hosted", "lab-macos"]},
            {"id": 2, "status": "in_progress", "labels": ["self-hosted", "lab-macos"]},
            {"id": 3, "status": "queued", "labels": ["ubuntu-latest"]}
        ]});
        assert_eq!(
            parse_jobs(11, &jobs),
            vec![
                Job {
                    id: 1,
                    run_id: 11,
                    labels: vec!["self-hosted".into(), "lab-macos".into()]
                },
                Job {
                    id: 3,
                    run_id: 11,
                    labels: vec!["ubuntu-latest".into()]
                },
            ]
        );
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
