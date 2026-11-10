//! A virtual machine's capability report, read from its image
//! (`docs/arch/machines.md`, MM-3; DC-67).
//!
//! A virtual machine's capabilities come from its image, not from a probe of
//! a running guest. The report has the durable machine's sections, so a
//! client compares both kinds in one shape:
//!
//! - `os` names the line's kind; the version is not recorded by a line.
//! - `hardware` and `disk` carry the line's resource defaults.
//! - `gpu` of a macOS guest is the measured paravirtual device (DC-27):
//!   families `mac2` and `common3`, no `metal3`, argument buffers tier 1. A
//!   Linux guest has no Metal device.
//! - `software` comes from the image's validated application inventory
//!   (`GET /applications`); without one it is `null` with the reason.
//! - The facts a running guest would have to answer (display,
//!   CI runner) are `null`, each with its reason under `unknown`. The
//!   instance never guesses a fact.

use serde_json::{json, Map, Value};

use crate::durable::capabilities::SCHEMA_VERSION;

/// The paravirtual GPU's Metal families, measured in a macOS guest (DC-27).
pub const PARAVIRTUAL_FAMILIES: [&str; 2] = ["mac2", "common3"];

/// Why a fact a running guest would answer is not in an image's report.
const GUEST_FACT: &str = "a virtual machine's report comes from its image, not from a guest";

/// The capability report of a virtual machine of image `kind` with resource
/// `defaults` (`{cpu, memory_mb, disk_gb}`). `inventory` is the image's
/// validated application inventory, or the reason there is none.
pub fn report(kind: &str, defaults: &Value, inventory: Result<&Value, &str>) -> Value {
    let mut unknown = Map::new();
    let os_name = match kind {
        "macos" => json!("macOS"),
        "linux" => json!("Linux"),
        _ => Value::Null,
    };
    unknown.insert(
        "os.version".to_string(),
        json!("an image line does not record its OS version"),
    );
    let number = |name: &str| defaults.get(name).and_then(Value::as_i64);
    let memory_bytes = number("memory_mb").map(|mb| json!(mb.saturating_mul(1024 * 1024)));
    let disk_bytes = number("disk_gb").map(|gb| json!(gb.saturating_mul(1_000_000_000)));
    if disk_bytes.is_none() {
        unknown.insert(
            "disk.total_bytes".to_string(),
            json!("the clone inherits its base's disk size"),
        );
    }
    let gpu = if kind == "macos" {
        json!({
            "name": "Apple Paravirtual device",
            "metal_families": PARAVIRTUAL_FAMILIES,
            "metal3": false,
            "argument_buffers_tier": 1,
        })
    } else {
        json!({"name": null, "metal_families": [], "metal3": false, "argument_buffers_tier": null})
    };
    let software = match inventory {
        Ok(inventory) => software(inventory),
        Err(reason) => {
            unknown.insert("software".to_string(), json!(reason));
            Value::Null
        }
    };
    for section in ["display", "ci_runner", "agents"] {
        unknown.insert(section.to_string(), json!(GUEST_FACT));
    }
    json!({
        "schema_version": SCHEMA_VERSION,
        "source": "image",
        "network": "nat",
        "os": {"name": os_name, "version": null, "build": null},
        "hardware": {
            "chip": null,
            "cpu_cores": number("cpu"),
            "memory_bytes": memory_bytes,
            "hardware_uuid": null,
        },
        "disk": {"total_bytes": disk_bytes, "free_bytes": null},
        "gpu": gpu,
        "display": null,
        "software": software,
        "agents": null,
        "ci_runner": null,
        "observed_at": {},
        "unknown": unknown,
    })
}

/// The `software` section from an application inventory's `applications`:
/// the same fields as a durable machine's, each a version or `null`, and
/// Xcode, Unity and Unreal as lists of installations.
pub fn software(inventory: &Value) -> Value {
    let applications: &[Value] = inventory
        .get("applications")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let version_of = |matches: &dyn Fn(&str) -> bool| -> Value {
        applications
            .iter()
            .find(|application| application_id(application).is_some_and(matches))
            .map(|application| application.get("version").cloned().unwrap_or(Value::Null))
            // Installed without a recorded version is still installed.
            .map(|version| {
                if version.is_null() {
                    json!("installed")
                } else {
                    version
                }
            })
            .unwrap_or(Value::Null)
    };
    let installs_of = |matches: &dyn Fn(&str) -> bool| -> Value {
        Value::Array(
            applications
                .iter()
                .filter(|application| application_id(application).is_some_and(matches))
                .map(|application| {
                    json!({
                        "version": application.get("version").cloned().unwrap_or(Value::Null),
                        "path": null,
                    })
                })
                .collect(),
        )
    };
    let package = |id: &str, names: &[&str]| -> bool {
        let Some((source, name)) = id.split_once(':') else {
            return false;
        };
        matches!(
            source,
            "brew-formula" | "brew-cask" | "dpkg" | "npm" | "pip" | "snap"
        ) && names.iter().any(|wanted| {
            name == *wanted
                || name
                    .strip_prefix(wanted)
                    .is_some_and(|rest| rest.starts_with('@'))
        })
    };
    json!({
        "node": version_of(&|id| package(id, &["node", "nodejs"])),
        "cua_driver": version_of(&|id| id == "bundle:com.trycua.driver" || package(id, &["cua-driver"])),
        "claude_code": version_of(&|id| package(id, &["@anthropic-ai/claude-code", "claude-code"])),
        "codex": version_of(&|id| package(id, &["@openai/codex", "codex"])),
        "tart": version_of(&|id| package(id, &["tart"])),
        "xcode": installs_of(&|id| id == "bundle:com.apple.dt.Xcode"),
        "unity": installs_of(&|id| id.starts_with("bundle:com.unity3d.UnityEditor")),
        "unreal": installs_of(&|id| id.starts_with("bundle:com.epicgames.UnrealEditor")),
    })
}

fn application_id(application: &Value) -> Option<&str> {
    application.get("id").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> Value {
        json!({"applications": [
            {"id": "brew-formula:node@24", "name": "node@24", "aliases": [], "version": "24.19.0"},
            {"id": "bundle:com.apple.dt.Xcode", "name": "Xcode", "aliases": [], "version": "26.6"},
            {"id": "bundle:com.trycua.driver", "name": "Cua Driver", "aliases": [], "version": "0.12.6"},
            {"id": "bundle:com.unity3d.UnityEditor5.x", "name": "Unity", "aliases": [], "version": "6000.3.26f1"},
            {"id": "npm:@openai/codex", "name": "@openai/codex", "aliases": [], "version": "0.154.0"},
            {"id": "dpkg:tart-guest-agent", "name": "tart-guest-agent", "aliases": [], "version": "0.11.0"},
            {"id": "brew-formula:nodenv", "name": "nodenv", "aliases": [], "version": "1.0"},
        ]})
    }

    /// MM-3 (TL-1): a macOS image reports the measured paravirtual GPU, its
    /// resource defaults, and its software from the inventory; a guest's
    /// own facts are unknown with their reason, never guessed.
    #[test]
    fn mm3_macos_image_report() {
        let inventory = inventory();
        let report = report(
            "macos",
            &json!({"cpu": 6, "memory_mb": 16384, "disk_gb": null}),
            Ok(&inventory),
        );
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["source"], "image");
        assert_eq!(report["os"]["name"], "macOS");
        assert_eq!(report["gpu"]["metal3"], false);
        assert_eq!(report["gpu"]["metal_families"], json!(["mac2", "common3"]));
        assert_eq!(report["gpu"]["argument_buffers_tier"], 1);
        assert_eq!(report["hardware"]["cpu_cores"], 6);
        assert_eq!(report["hardware"]["memory_bytes"], 16384u64 * 1024 * 1024);
        assert_eq!(report["disk"]["total_bytes"], Value::Null);
        let software = &report["software"];
        assert_eq!(software["node"], "24.19.0");
        assert_eq!(software["cua_driver"], "0.12.6");
        assert_eq!(software["codex"], "0.154.0");
        assert_eq!(software["claude_code"], Value::Null);
        // The guest agent of Tart is not Tart.
        assert_eq!(software["tart"], Value::Null);
        assert_eq!(software["xcode"][0]["version"], "26.6");
        assert_eq!(software["unity"][0]["version"], "6000.3.26f1");
        assert_eq!(software["unreal"], json!([]));
        for section in ["display", "ci_runner"] {
            assert_eq!(report[section], Value::Null);
            assert!(report["unknown"].get(section).is_some(), "{section}");
        }
    }

    /// MM-3 (TL-1): a Linux image has no Metal device, and a missing
    /// inventory leaves the software unknown with its reason.
    #[test]
    fn mm3_linux_image_without_inventory() {
        let report = report(
            "linux",
            &json!({"cpu": 4, "memory_mb": 8192, "disk_gb": 50}),
            Err("no validated application inventory for ubuntu2404"),
        );
        assert_eq!(report["os"]["name"], "Linux");
        assert_eq!(report["gpu"]["metal3"], false);
        assert_eq!(report["gpu"]["metal_families"], json!([]));
        assert_eq!(report["disk"]["total_bytes"], 50_000_000_000u64);
        assert_eq!(report["software"], Value::Null);
        assert_eq!(
            report["unknown"]["software"],
            "no validated application inventory for ubuntu2404"
        );
    }
}
