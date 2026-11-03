//! The controller's local configuration, `~/.config/vm-service-lab/controller.json`
//! (LR-10): what only this Mac's owner decides, so never anything the
//! repository or a workflow run could set.
//!
//! ```json
//! {"owner": "<GitHub login>"}
//! ```
//!
//! `owner` is the login whose dispatched runs a durable Mac serves. A
//! durable Mac without it serves nothing; a VM host does not read it.
//! `lab-host.sh install --owner LOGIN` writes it.

use std::path::Path;

use serde_json::Value;

/// The local configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The owner's GitHub login.
    pub owner: Option<String>,
}

impl Config {
    /// The configuration in `text`.
    pub fn parse(text: &str) -> Result<Config, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| format!("not JSON: {error}"))?;
        let object = value.as_object().ok_or("not a JSON object")?;
        if let Some(key) = object.keys().find(|key| key.as_str() != "owner") {
            return Err(format!("unknown key {key:?}"));
        }
        let owner = match object.get("owner") {
            None | Some(Value::Null) => None,
            Some(Value::String(login)) if crate::runs::valid_login(login) => Some(login.clone()),
            Some(other) => return Err(format!("owner {other} is not a GitHub login")),
        };
        Ok(Config { owner })
    }

    /// The configuration at `path`; none there is the empty configuration.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                Config::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_is_read_and_checked() {
        assert_eq!(
            Config::parse(r#"{"owner": "some-one"}"#).unwrap(),
            Config {
                owner: Some("some-one".into()),
            }
        );
        assert_eq!(Config::parse("{}").unwrap(), Config::default());
        for bad in [
            r#"{"owner": "a b"}"#,
            r#"{"owner": 3}"#,
            r#"{"owners": "x"}"#,
            "[]",
            "nope",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        let missing = Config::load(Path::new("/nonexistent/controller.json")).unwrap();
        assert_eq!(missing, Config::default());
    }
}
