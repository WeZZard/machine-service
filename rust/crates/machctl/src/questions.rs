//! Catalog questions asked of every instance (`docs/arch/federation.md`
//! FD-7; `docs/ux/cli/list.md` CL-4), and the choice of an instance for an
//! image acquisition (FD-7, CL-2).
//!
//! A question goes to every instance the gathering confirmed, at once, each
//! within its own deadline. Each answer is kept unchanged under its
//! instance; an instance that did not answer stays in the result as
//! unreachable, so one instance's empty catalog never reads as the
//! network's.

use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::federation::{Instance, Merged, Settings};
use crate::http;

/// One instance's answer to one question.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The instance answered, with this HTTP status and body.
    Answered { status: u16, body: Value },
    /// The instance did not answer, for this reason.
    Unreachable(String),
}

impl Answer {
    /// The body of an answer with HTTP 200.
    pub fn ok(&self) -> Option<&Value> {
        match self {
            Answer::Answered { status: 200, body } => Some(body),
            _ => None,
        }
    }

    /// The body of any answer.
    pub fn body(&self) -> Option<&Value> {
        match self {
            Answer::Answered { body, .. } => Some(body),
            Answer::Unreachable(_) => None,
        }
    }
}

/// Ask `path` of every instance in `merged`, in its order, with `method`.
/// The local instance gets the local headers (a selected environment's
/// fingerprint); an instance that did not answer the gathering is not asked
/// again and keeps its error.
pub fn ask(
    merged: &Merged,
    settings: &Settings,
    method: &str,
    path: &str,
    timeout: Duration,
) -> Vec<Answer> {
    thread::scope(|scope| {
        let handles: Vec<_> = merged
            .instances
            .iter()
            .map(|instance| {
                scope.spawn(move || {
                    if !instance.reachable {
                        return Answer::Unreachable(
                            instance
                                .error
                                .clone()
                                .unwrap_or_else(|| "no answer".to_string()),
                        );
                    }
                    let headers: &[(String, String)] = if instance.local {
                        &settings.local_headers
                    } else {
                        &[]
                    };
                    let url = format!("{}{path}", instance.address);
                    let result = if method == "POST" {
                        http::post_json(&url, timeout, headers)
                    } else {
                        http::get_json(&url, timeout, headers)
                    };
                    match result {
                        Ok((status, body)) => Answer::Answered { status, body },
                        Err(error) => Answer::Unreachable(error),
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Answer::Unreachable("the request panicked".to_string()))
            })
            .collect()
    })
}

/// The JSON of CL-4: one object per instance with its identity, how it was
/// found, whether it answered, and its answer unchanged.
pub fn to_json(merged: &Merged, answers: &[Answer]) -> Value {
    Value::Array(
        merged
            .instances
            .iter()
            .zip(answers)
            .map(|(instance, answer)| entry(instance, answer, answer.body().cloned()))
            .collect(),
    )
}

/// One instance's object, with `answer` as given.
pub fn entry(instance: &Instance, answer: &Answer, body: Option<Value>) -> Value {
    let mut object = Map::new();
    object.insert("instance".into(), json!(instance.label));
    object.insert("machine_id".into(), json!(instance.machine_id));
    object.insert("address".into(), json!(instance.address));
    object.insert("local".into(), json!(instance.local));
    object.insert("found_by".into(), json!(instance.found_by));
    match answer {
        Answer::Answered { status, .. } => {
            object.insert("reachable".into(), json!(true));
            object.insert("error".into(), Value::Null);
            object.insert("status".into(), json!(status));
            object.insert("answer".into(), body.unwrap_or(Value::Null));
        }
        Answer::Unreachable(error) => {
            object.insert("reachable".into(), json!(false));
            object.insert("error".into(), json!(error));
            object.insert("status".into(), Value::Null);
        }
    }
    Value::Object(object)
}

/// The local instance refused the connection: no service runs on this Mac
/// (CL-1, DC-93).
fn not_running(instance: &Instance, answer: &Answer) -> bool {
    instance.local
        && matches!(answer, Answer::Unreachable(error) if error.contains("connection refused"))
}

/// The `machctl images` table of CL-4: each instance's rows, its catalog
/// warning above them, and one row for an instance with no image or no
/// answer.
pub fn format_images(merged: &Merged, answers: &[Answer]) -> String {
    let width = merged
        .instances
        .iter()
        .map(|instance| instance.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(16);
    let mut out = String::new();
    for (instance, answer) in merged.instances.iter().zip(answers) {
        let label = &instance.label;
        let body = match answer {
            Answer::Unreachable(error) => {
                let (state, text) = if not_running(instance, answer) {
                    ("not running", "no machine-service on this Mac")
                } else {
                    ("unreachable", error.as_str())
                };
                out.push_str(&format!("{label:<width$} {state:<14} {text}\n"));
                continue;
            }
            Answer::Answered { body, .. } => body,
        };
        if let Some(warning) = crate::catalog_warning(body) {
            let rest = warning
                .strip_prefix("machctl: warning: ")
                .unwrap_or(&warning);
            out.push_str(&format!("machctl: warning: {label}: {rest}\n"));
        }
        let rows = crate::format_images(body);
        if body.get("images").is_none() {
            let error = crate::error_message_text(body);
            out.push_str(&format!("{label:<width$} {:<14} {error}\n", "no catalog"));
        } else if rows.is_empty() {
            out.push_str(&format!("{label:<width$} {:<14} provides no images\n", "-"));
        } else {
            for row in rows.lines() {
                out.push_str(&format!("{label:<width$} {row}\n"));
            }
        }
    }
    out
}

/// One instance an image acquisition may ask, with the line printed before
/// it is asked (FD-7, CL-2).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The instance's base URL.
    pub address: String,
    /// The instance's name.
    pub instance: String,
    /// `chose <image> (virtual, offer on <instance>)`, and why it is not
    /// acquirable when it is not.
    pub reason: String,
}

/// The instances whose offer of `image` has its base built, acquirable
/// offers first, each group in the gathering's order: the local instance
/// first, then the others by name (FD-7).
pub fn image_candidates(merged: &Merged, image: &str) -> Vec<Candidate> {
    let mut found: Vec<(bool, Candidate)> = merged
        .instances
        .iter()
        .filter(|instance| instance.reachable)
        .filter_map(|instance| {
            let offer = instance.machines().into_iter().find(|record| {
                crate::capability::is_offer(record)
                    && [record.get("image"), record.get("name")]
                        .into_iter()
                        .flatten()
                        .any(|name| name.as_str() == Some(image))
            })?;
            if offer.get("base_available") == Some(&Value::Bool(false)) {
                return None;
            }
            let acquirable = crate::capability::offer_acquirable(&offer);
            let mut why = format!("virtual, offer on {}", instance.label);
            if !acquirable {
                why.push_str(", not acquirable now");
            }
            Some((
                acquirable,
                Candidate {
                    address: instance.address.clone(),
                    instance: instance.label.clone(),
                    reason: format!("chose {image} ({why})"),
                },
            ))
        })
        .collect();
    // A stable sort keeps the gathering's order inside each group.
    found.sort_by_key(|(acquirable, _)| !*acquirable);
    found.into_iter().map(|(_, candidate)| candidate).collect()
}

/// The images each answering instance offers, for a message that says
/// where an image is not: `mac-a: macos26, ubuntu2404; mac-b: none`.
pub fn offered(merged: &Merged, answers: &[Answer]) -> String {
    merged
        .instances
        .iter()
        .zip(answers)
        .map(|(instance, answer)| {
            let images = match answer.body().and_then(|body| body.get("images")) {
                Some(Value::Object(images)) if !images.is_empty() => {
                    images.keys().cloned().collect::<Vec<String>>().join(", ")
                }
                Some(_) => "none".to_string(),
                None => match answer {
                    Answer::Unreachable(error) => format!("unreachable ({error})"),
                    Answer::Answered { body, .. } => {
                        format!("no catalog ({})", crate::error_message_text(body))
                    }
                },
            };
            format!("{}: {images}", instance.label)
        })
        .collect::<Vec<String>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(label: &str, local: bool, body: Option<Value>) -> Instance {
        Instance {
            label: label.to_string(),
            id: Some(label.to_string()),
            machine_id: Some(format!("id-{label}")),
            former_names: Vec::new(),
            address: format!("http://{label}:6240"),
            local,
            consul: None,
            found_by: if local { "local" } else { "bonjour" },
            expected_id: None,
            unaskable: None,
            reachable: body.is_some(),
            error: body.is_none().then(|| "connection refused".to_string()),
            body,
        }
    }

    fn merged(instances: Vec<Instance>) -> Merged {
        Merged {
            instances,
            consul_asked: false,
            consul_error: None,
            browsed: true,
            browse_error: None,
            notes: Vec::new(),
        }
    }

    fn offer(image: &str, acquirable: bool, built: bool) -> Value {
        json!({"name": image, "image": image, "kind": "virtual", "base_available": built,
               "concurrency": {"running": 0, "limit": 2, "acquirable": acquirable}})
    }

    /// FD-7: acquirable offers first, the local instance first within a
    /// group, and an offer whose base is not built is no candidate.
    #[test]
    fn fd7_image_candidates_follow_the_offer_order() {
        let merged = merged(vec![
            instance("laptop", true, Some(json!({"machines": {}, "offers": []}))),
            instance(
                "mac-a",
                false,
                Some(json!({"machines": {}, "offers": [offer("macos26", false, true)]})),
            ),
            instance(
                "mac-b",
                false,
                Some(json!({"machines": {}, "offers": [offer("macos26", true, true)]})),
            ),
            instance(
                "mac-c",
                false,
                Some(json!({"machines": {}, "offers": [offer("macos26", true, false)]})),
            ),
            instance("mac-d", false, None),
        ]);
        let chosen = image_candidates(&merged, "macos26");
        let names: Vec<&str> = chosen.iter().map(|c| c.instance.as_str()).collect();
        assert_eq!(names, ["mac-b", "mac-a"]);
        assert_eq!(chosen[0].reason, "chose macos26 (virtual, offer on mac-b)");
        assert!(chosen[1].reason.ends_with("not acquirable now)"));
    }

    /// CL-4: every instance keeps a row; an empty catalog and an unreachable
    /// instance are said, never left out.
    #[test]
    fn cl4_images_table_keeps_every_instance() {
        let merged = merged(vec![
            instance("laptop", true, Some(json!({}))),
            instance("mac-a", false, Some(json!({}))),
            instance("mac-b", false, None),
        ]);
        let answers = vec![
            Answer::Answered {
                status: 200,
                body: json!({"images": {}, "catalog": {"status": "current", "generation": 1}}),
            },
            Answer::Answered {
                status: 200,
                body: json!({"images": {"macos26": {
                    "kind": "macos", "base_vm": "pilot-macos26-base", "base_available": true,
                    "concurrency": {"running": 0, "limit": 2, "acquirable": true}}}}),
            },
            Answer::Unreachable("connection refused".to_string()),
        ];
        let table = format_images(&merged, &answers);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{table}");
        assert!(lines[0].starts_with("laptop ") && lines[0].ends_with("provides no images"));
        assert!(lines[1].starts_with("mac-a ") && lines[1].contains("macos26"));
        assert!(lines[2].starts_with("mac-b ") && lines[2].contains("unreachable"));
        let json = to_json(&merged, &answers);
        assert_eq!(json[0]["answer"]["images"], json!({}));
        assert_eq!(json[1]["machine_id"], "id-mac-a");
        assert_eq!(json[2]["reachable"], false);
        assert!(json[2].get("answer").is_none());
        assert_eq!(
            offered(&merged, &answers),
            "laptop: none; mac-a: macos26; mac-b: unreachable (connection refused)"
        );
    }

    /// CL-1/DC-93: the local instance that refuses the connection is not
    /// running, not unreachable.
    #[test]
    fn cl4_local_refusal_reads_as_not_running() {
        let merged = merged(vec![instance("laptop", true, None)]);
        let answers = vec![Answer::Unreachable("connection refused".to_string())];
        assert!(format_images(&merged, &answers).contains("not running"));
    }
}
