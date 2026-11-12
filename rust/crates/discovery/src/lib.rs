//! How instances find one another (`docs/arch/federation.md` FD-1, FD-2;
//! DC-84, DC-85), shared by the daemon and `machctl`.
//!
//! `MACHINE_SERVICE_DISCOVERY` chooses the mode for the daemon and every
//! client alike: `bonjour`, the default, or `consul`. In Bonjour mode each
//! instance advertises `_machine-service._tcp` through the Mac's own DNS-SD
//! service, named by its instance ID, with `txtvers=1`, `instance` and
//! `machine_id` and `kinds` in its TXT record (MI-1/DC-87; DNS-SD, RFC 6763, over Multicast DNS, RFC
//! 6762), and clients browse for it. In Consul mode the daemon advertises
//! nothing and clients ask the Consul agent.

pub mod dnssd;

/// The variable that chooses the mode (DC-84).
pub const MODE_VARIABLE: &str = "MACHINE_SERVICE_DISCOVERY";
/// The DNS-SD service type every instance advertises (DC-85).
pub const SERVICE_TYPE: &str = "_machine-service._tcp";
/// The version of the TXT record's keys (RFC 6763 §6.7).
pub const TXT_VERSION: &str = "1";

/// How instances are found (DC-84).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Each instance advertises itself; clients browse the network.
    #[default]
    Bonjour,
    /// The service-catalog registrar registers each instance in Consul;
    /// clients ask the Consul agent, and the daemon advertises nothing.
    Consul,
}

impl Mode {
    /// The accepted values.
    pub const NAMES: [&'static str; 2] = ["bonjour", "consul"];

    /// Parse a configuration value; empty selects the default.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "" | "bonjour" => Ok(Mode::Bonjour),
            "consul" => Ok(Mode::Consul),
            other => Err(format!(
                "{MODE_VARIABLE} must be one of {}; got {other:?}",
                Self::NAMES.join(", ")
            )),
        }
    }

    /// Read `MACHINE_SERVICE_DISCOVERY`; unset or empty selects Bonjour.
    pub fn from_environment() -> Result<Self, String> {
        match std::env::var(MODE_VARIABLE) {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Mode::Bonjour),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(format!("{MODE_VARIABLE} is not valid UTF-8"))
            }
        }
    }

    /// The configuration value naming this mode.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Bonjour => "bonjour",
            Mode::Consul => "consul",
        }
    }
}

/// Whether the daemon's bind address reaches only its own Mac, in which
/// case it advertises nothing: no other Mac could connect (FD-1).
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// The TXT record an instance advertises: a stable machine ID, its current
/// name, and its kinds joined by commas (MI-1/DC-87). Adding an optional key
/// keeps `txtvers=1`, so older browsers continue to read the record.
pub fn txt_entries(instance: &str, machine_id: &str, kinds: &[&str]) -> Vec<(String, String)> {
    vec![
        ("txtvers".to_string(), TXT_VERSION.to_string()),
        ("instance".to_string(), instance.to_string()),
        ("machine_id".to_string(), machine_id.to_string()),
        ("kinds".to_string(), kinds.join(",")),
    ]
}

/// The TXT keys of an instance's broker (BK-4, RP-11), added to
/// [`txt_entries`] when the instance runs one.
pub fn relay_txt_entries(relay_url: &str, relay_ca_sha256: &str) -> Vec<(String, String)> {
    vec![
        ("relay_url".to_string(), relay_url.to_string()),
        ("relay_ca_sha256".to_string(), relay_ca_sha256.to_string()),
    ]
}

/// Encode `key=value` pairs as a DNS TXT record's character-strings, each
/// prefixed by its length (RFC 6763 §6). A pair longer than 255 bytes is
/// refused.
pub fn encode_txt(entries: &[(String, String)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (key, value) in entries {
        let pair = format!("{key}={value}");
        let length = u8::try_from(pair.len())
            .map_err(|_| format!("TXT entry {key} is longer than 255 bytes"))?;
        out.push(length);
        out.extend_from_slice(pair.as_bytes());
    }
    Ok(out)
}

/// Decode a TXT record into `key=value` pairs. A string without `=` is a key
/// with an empty value; a malformed length ends the record (RFC 6763 §6.4).
pub fn decode_txt(record: &[u8]) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut rest = record;
    while let Some((&length, tail)) = rest.split_first() {
        let length = usize::from(length);
        if length > tail.len() {
            break;
        }
        let (string, after) = tail.split_at(length);
        rest = after;
        if string.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(string);
        let (key, value) = text.split_once('=').unwrap_or((&text, ""));
        entries.push((key.to_ascii_lowercase(), value.to_string()));
    }
    entries
}

/// One instance found by browsing: what its advertisement says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The service instance name, which is the instance ID unless the
    /// network renamed a duplicate.
    pub name: String,
    /// The host the advertisement names, such as `Mac-A.local.`.
    pub host: String,
    /// An IPv4 address of that host, when one resolved.
    pub address: Option<std::net::Ipv4Addr>,
    /// The service port.
    pub port: u16,
    /// The TXT record's pairs.
    pub txt: Vec<(String, String)>,
}

impl Found {
    /// One TXT value.
    pub fn txt_value(&self, key: &str) -> Option<&str> {
        self.txt
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The instance ID it advertises, else its service name.
    pub fn instance(&self) -> &str {
        self.txt_value("instance")
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.name)
    }

    /// The permanent machine identity, absent on older advertisements
    /// (MI-1/DC-87).
    pub fn machine_id(&self) -> Option<&str> {
        self.txt_value("machine_id").filter(|id| !id.is_empty())
    }

    /// The kinds it advertises.
    /// The instance's broker URL, when it advertises one (BK-4).
    pub fn relay_url(&self) -> Option<&str> {
        self.txt_value("relay_url").filter(|url| !url.is_empty())
    }

    /// The fingerprint of the instance's broker authority (RP-1).
    pub fn relay_ca_sha256(&self) -> Option<&str> {
        self.txt_value("relay_ca_sha256")
            .filter(|hash| !hash.is_empty())
    }

    pub fn kinds(&self) -> Vec<String> {
        self.txt_value("kinds")
            .unwrap_or_default()
            .split(',')
            .filter(|kind| !kind.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The base URL to reach it at: its IPv4 address, else its host name.
    pub fn base_url(&self) -> String {
        let host = match self.address {
            Some(address) => address.to_string(),
            None => self.host.trim_end_matches('.').to_string(),
        };
        format!("http://{host}:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MI-1/DC-87: identity travels beside the mutable label without
    /// changing the TXT protocol version.
    #[test]
    fn mi1_txt_contains_machine_identity_and_keeps_version_one() {
        let machine_id = "00000000-0000-4000-8000-000000000001";
        let entries = txt_entries("Mac-A", machine_id, &["durable"]);
        let found = Found {
            name: "Mac-A".into(),
            host: "Mac-A.local.".into(),
            address: None,
            port: 6240,
            txt: decode_txt(&encode_txt(&entries).unwrap()),
        };
        assert_eq!(found.machine_id(), Some(machine_id));
        assert_eq!(found.txt_value("txtvers"), Some("1"));
        assert_eq!(found.instance(), "Mac-A");
        assert_eq!(found.kinds(), ["durable"]);
        assert_eq!(found.relay_url(), None);
        let mut with_relay = entries.clone();
        with_relay.extend(relay_txt_entries(
            "wss://mac-a.local:6241/relay",
            &"a".repeat(64),
        ));
        let relayed = Found {
            txt: decode_txt(&encode_txt(&with_relay).unwrap()),
            ..found.clone()
        };
        assert_eq!(relayed.relay_url(), Some("wss://mac-a.local:6241/relay"));
        assert_eq!(relayed.relay_ca_sha256(), Some("a".repeat(64).as_str()));
        assert_eq!(relayed.machine_id(), Some(machine_id));
        assert_eq!(
            Found {
                txt: Vec::new(),
                ..found
            }
            .machine_id(),
            None
        );
    }

    #[test]
    fn mode_defaults_to_bonjour_and_refuses_anything_else() {
        assert_eq!(Mode::parse("").unwrap(), Mode::Bonjour);
        assert_eq!(Mode::parse("bonjour").unwrap(), Mode::Bonjour);
        assert_eq!(Mode::parse("consul").unwrap(), Mode::Consul);
        let error = Mode::parse("mdns").unwrap_err();
        assert!(error.contains("MACHINE_SERVICE_DISCOVERY must be one of bonjour, consul"));
        assert_eq!(Mode::default(), Mode::Bonjour);
    }

    #[test]
    fn loopback_binds_advertise_nothing() {
        for host in ["127.0.0.1", "127.0.0.2", "::1", "[::1]", "localhost"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "192.0.2.4"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn the_txt_record_round_trips_and_says_nothing_else() {
        let entries = txt_entries("Mac-A", "host-id", &["durable", "virtual"]);
        let record = encode_txt(&entries).unwrap();
        assert_eq!(
            record,
            b"\x09txtvers=1\x0einstance=Mac-A\x12machine_id=host-id\x15kinds=durable,virtual"
                .to_vec()
        );
        assert_eq!(decode_txt(&record), entries);
        assert!(encode_txt(&[("k".into(), "v".repeat(300))]).is_err());
    }

    #[test]
    fn a_malformed_txt_record_keeps_what_parsed() {
        assert_eq!(
            decode_txt(b"\x09txtvers=1\x00\x04Flag\x30cut"),
            vec![
                ("txtvers".to_string(), "1".to_string()),
                ("flag".to_string(), String::new())
            ]
        );
    }

    #[test]
    fn a_found_instance_names_itself_by_its_txt_record() {
        let found = Found {
            name: "Mac-A (2)".into(),
            host: "Mac-A.local.".into(),
            address: Some("192.0.2.4".parse().unwrap()),
            port: 6240,
            txt: txt_entries("Mac-A", "host-id", &["durable"]),
        };
        assert_eq!(found.instance(), "Mac-A");
        assert_eq!(found.kinds(), vec!["durable".to_string()]);
        assert_eq!(found.base_url(), "http://192.0.2.4:6240");
        let unresolved = Found {
            address: None,
            txt: Vec::new(),
            ..found
        };
        assert_eq!(unresolved.instance(), "Mac-A (2)");
        assert_eq!(unresolved.base_url(), "http://Mac-A.local:6240");
    }
}
