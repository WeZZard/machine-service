//! Capabilities a client can require of a machine, read from its capability
//! report (`docs/ux/cli/list.md` CL-2, `docs/arch/machines.md` MM-3).
//!
//! A fact the instance could not observe is `null` in the report, and a
//! requirement over an unknown fact never matches: the instance never
//! guesses a fact (MM-3), and neither does the CLI.

use serde_json::Value;

/// The capability names `--require` accepts (CL-2).
pub const NAMES: [&str; 10] = [
    "metal3",
    "durable",
    "virtual",
    "unity",
    "unreal",
    "xcode",
    "cua-driver",
    "claude-code",
    "codex",
    "ci-idle",
];

/// Refuse a name `--require` does not know, listing the ones it does.
pub fn validate(names: &[String]) -> Result<(), String> {
    for name in names {
        if !NAMES.contains(&name.as_str()) {
            return Err(format!(
                "unknown capability {name:?} (choose from {})",
                NAMES.join(", ")
            ));
        }
    }
    Ok(())
}

/// The record's kind: `durable` or `virtual`. A record without a kind is a
/// virtual machine, as every record of an instance before durable machines.
pub fn kind(record: &Value) -> &str {
    record
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("virtual")
}

/// Whether the record is a virtual machine offer rather than a machine.
pub fn is_offer(record: &Value) -> bool {
    record.get("state").and_then(Value::as_str) == Some("offer")
}

fn section<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    record
        .get("capabilities")
        .and_then(|capabilities| capabilities.get(name))
        .filter(|value| !value.is_null())
}

/// A software entry that is a version string or a non-empty list of
/// installations.
fn installed(record: &Value, name: &str) -> bool {
    match section(record, "software").and_then(|software| software.get(name)) {
        Some(Value::String(version)) => !version.is_empty(),
        Some(Value::Array(installs)) => !installs.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        _ => false,
    }
}

/// An agent CLI is usable when it is installed and, where the record
/// reports agent keys (a durable machine, AK-1, AK-2), both its key and its
/// gateway endpoint are present.
fn agent_ready(record: &Value, software: &str, agent: &str) -> bool {
    if !installed(record, software) {
        return false;
    }
    match section(record, "agents").and_then(|agents| agents.get(agent)) {
        None => true,
        Some(state) => {
            state.get("key").and_then(Value::as_str) == Some("present")
                && state.get("endpoint").and_then(Value::as_str) == Some("present")
        }
    }
}

/// Whether the record has the capability `name`.
pub fn has(record: &Value, name: &str) -> bool {
    match name {
        "durable" => kind(record) == "durable",
        "virtual" => kind(record) == "virtual",
        "metal3" => {
            section(record, "gpu")
                .and_then(|gpu| gpu.get("metal3"))
                .and_then(Value::as_bool)
                == Some(true)
        }
        "unity" => installed(record, "unity"),
        "unreal" => installed(record, "unreal"),
        "xcode" => installed(record, "xcode"),
        "cua-driver" => installed(record, "cua_driver"),
        "claude-code" => agent_ready(record, "claude_code", "claude_code"),
        "codex" => agent_ready(record, "codex", "codex"),
        "ci-idle" => ci_idle(record),
        _ => false,
    }
}

/// A Mac without a CI runner is idle; one with a runner is idle when the
/// runner is not busy. A record that reports nothing is not known to be
/// idle.
fn ci_idle(record: &Value) -> bool {
    let Some(runner) = section(record, "ci_runner") else {
        return false;
    };
    match runner.get("present").and_then(Value::as_bool) {
        Some(false) => true,
        Some(true) => runner.get("busy").and_then(Value::as_bool) == Some(false),
        None => false,
    }
}

/// Whether the record has every capability in `names`.
pub fn has_all(record: &Value, names: &[String]) -> bool {
    names.iter().all(|name| has(record, name))
}

/// One machine `machctl acquire --require` could lease, with why.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The instance that owns it.
    pub instance: String,
    /// The machine's name, or the image an offer clones.
    pub name: String,
    /// The preference tier: 0 is best.
    pub tier: u8,
    /// The line printed before the lease, as CL-2 shows it.
    pub reason: String,
}

/// The machines that match `names`, best first, in CL-2's order of
/// preference: an available durable machine whose CI runner is idle, then
/// any available durable machine, then an acquirable virtual machine
/// offer. Ties keep the listing's order: the local instance first.
///
/// `machctl acquire --require` leases the first candidate and passes to the
/// next on a capacity refusal (DC-69).
pub fn choose(instances: &[(String, Vec<Value>)], names: &[String]) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    for (instance, records) in instances {
        for record in records {
            if !has_all(record, names) {
                continue;
            }
            let state = record.get("state").and_then(Value::as_str);
            let tier = if kind(record) == "durable" && state == Some("available") {
                if ci_idle(record) {
                    0
                } else {
                    1
                }
            } else if is_offer(record) && offer_acquirable(record) {
                2
            } else {
                continue;
            };
            let name = record
                .get("name")
                .or_else(|| record.get("image"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut why = vec![kind(record).to_string()];
            why.extend(
                names
                    .iter()
                    .filter(|name| name.as_str() != "durable" && name.as_str() != "virtual")
                    .filter(|name| name.as_str() != "ci-idle")
                    .cloned(),
            );
            if tier == 0 {
                why.push("CI idle".to_string());
            }
            if tier == 2 {
                why.push(format!("offer on {instance}"));
            }
            let reason = format!("chose {name} ({})", why.join(", "));
            candidates.push(Candidate {
                instance: instance.clone(),
                name,
                tier,
                reason,
            });
        }
    }
    // A stable sort keeps the listing's order inside each tier.
    candidates.sort_by_key(|candidate| candidate.tier);
    candidates
}

/// Whether an offer can be acquired now, by its `acquirable` figure.
pub fn offer_acquirable(record: &Value) -> bool {
    record
        .get("concurrency")
        .and_then(|concurrency| concurrency.get("acquirable"))
        .or_else(|| record.get("acquirable"))
        .and_then(Value::as_bool)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn durable(name: &str, state: &str, busy: Option<bool>, metal3: bool) -> Value {
        json!({
            "name": name, "kind": "durable", "state": state,
            "capabilities": {
                "schema_version": 1,
                "gpu": {"metal3": metal3, "metal_families": ["apple9", "metal3"]},
                "software": {"unreal": [{"version": "5.6", "path": "/Users/Shared/Epic"}],
                             "unity": [], "xcode": [{"version": "26.0"}],
                             "cua_driver": "0.19.3", "claude_code": "2.1.0", "codex": null},
                "agents": {"claude_code": {"key": "present", "endpoint": "present"},
                           "codex": {"key": "absent", "endpoint": "present"}},
                "ci_runner": match busy {
                    None => json!({"present": false, "busy": null, "name": null}),
                    Some(busy) => json!({"present": true, "busy": busy, "name": "runner"}),
                },
            }
        })
    }

    fn offer(image: &str, acquirable: bool) -> Value {
        json!({
            "name": image, "image": image, "kind": "virtual", "state": "offer",
            "concurrency": {"running": 1, "limit": 2, "acquirable": acquirable},
            "capabilities": {"gpu": {"metal3": false, "metal_families": ["mac2", "common3"]},
                             "software": {"xcode": [{"version": "26.0"}]}}
        })
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    /// CL-2: every capability name reads its fact from the report.
    #[test]
    fn cl2_capabilities_read_the_report() {
        let mac = durable("mac-b", "available", Some(false), true);
        for name in [
            "durable",
            "metal3",
            "unreal",
            "xcode",
            "cua-driver",
            "claude-code",
            "ci-idle",
        ] {
            assert!(has(&mac, name), "{name}");
        }
        // Unity has no installation; Codex has no key.
        for name in ["virtual", "unity", "codex"] {
            assert!(!has(&mac, name), "{name}");
        }
        let vm = offer("macos26", true);
        assert!(has(&vm, "virtual") && has(&vm, "xcode"));
        assert!(!has(&vm, "metal3") && !has(&vm, "ci-idle"));
        // A Mac without a runner is idle; a busy runner is not.
        assert!(has(&durable("m", "available", None, true), "ci-idle"));
        assert!(!has(
            &durable("m", "available", Some(true), true),
            "ci-idle"
        ));
    }

    /// MM-3: an unknown fact never matches.
    #[test]
    fn cl2_unknown_facts_never_match() {
        let unknown = json!({"name": "m", "kind": "durable", "state": "available",
            "capabilities": {"gpu": null, "software": null,
                             "ci_runner": null, "unknown": ["gpu: helper failed"]}});
        for name in NAMES {
            if name != "durable" {
                assert!(!has(&unknown, name), "{name}");
            }
        }
        // A record from an instance without capability reports.
        assert!(!has(
            &json!({"name": "pilot-mac-x", "state": "running"}),
            "metal3"
        ));
        assert!(has(
            &json!({"name": "pilot-mac-x", "state": "running"}),
            "virtual"
        ));
    }

    #[test]
    fn cl2_validate_names_the_choices() {
        assert!(validate(&strings(&["metal3", "ci-idle"])).is_ok());
        let error = validate(&strings(&["metal"])).unwrap_err();
        assert!(error.contains("\"metal\"") && error.contains("ci-idle"));
        // The report holds no privacy permissions (DC-309).
        assert!(validate(&strings(&["screen-recording"])).is_err());
    }

    /// CL-2: an idle durable machine first, then any available durable
    /// machine, then an acquirable offer; leased machines and full offers
    /// are never candidates.
    #[test]
    fn cl2_preference_order() {
        let listing = vec![
            (
                "mac-a".to_string(),
                vec![offer("macos26", true), offer("macos26-games", false)],
            ),
            (
                "mac-b".to_string(),
                vec![durable("mac-b", "available", Some(true), true)],
            ),
            (
                "mac-c".to_string(),
                vec![durable("mac-c", "available", Some(false), true)],
            ),
            (
                "mac-d".to_string(),
                vec![durable("mac-d", "leased", Some(false), true)],
            ),
        ];
        let chosen = choose(&listing, &[]);
        let names: Vec<&str> = chosen.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["mac-c", "mac-b", "macos26"]);
        assert_eq!(chosen[0].reason, "chose mac-c (durable, CI idle)");

        let metal = choose(&listing, &strings(&["metal3", "unreal"]));
        assert_eq!(
            metal[0].reason,
            "chose mac-c (durable, metal3, unreal, CI idle)"
        );
        assert!(metal.iter().all(|candidate| candidate.instance != "mac-a"));

        let xcode_vm = choose(&listing, &strings(&["virtual", "xcode"]));
        assert_eq!(xcode_vm.len(), 1);
        assert_eq!(
            xcode_vm[0].reason,
            "chose macos26 (virtual, xcode, offer on mac-a)"
        );
    }
}
