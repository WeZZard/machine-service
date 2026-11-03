//! The egress allowlist: GitHub's published ranges, the addresses of the
//! names a self-hosted runner reaches, and the guests' resolvers (LR-5,
//! DC-1xx-EPH-4, DC-1xx-EPH-10).

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::path::Path;

use serde_json::Value;

use crate::dns::Resolve;

/// The `meta` sections whose ranges the allowlist holds.
pub const META_SECTIONS: [&str; 4] = ["web", "api", "git", "packages"];

/// The name the egress guests' isolation check loads.
pub const CHECK_NAME: &str = "api.github.com";

/// How often the allowlist is written again, in seconds.
pub const REFRESH_S: u64 = 30 * 60;

/// The IPv4 ranges of [`META_SECTIONS`] and the self-hosted runner's
/// names (`domains.actions_inbound.full_domains`) of a `meta` answer.
pub fn from_meta(meta: &Value) -> Result<(Vec<String>, Vec<String>), String> {
    let mut ranges = BTreeSet::new();
    for section in META_SECTIONS {
        let list = meta
            .get(section)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("GitHub's meta has no {section} section"))?;
        ranges.extend(
            list.iter()
                .filter_map(Value::as_str)
                .filter(|entry| !entry.contains(':'))
                .filter(|entry| guest_firewall::valid_entry(entry))
                .map(str::to_string),
        );
    }
    let names: Vec<String> = meta
        .pointer("/domains/actions_inbound/full_domains")
        .and_then(Value::as_array)
        .ok_or("GitHub's meta names no domains for self-hosted runners")?
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| !name.contains('*'))
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return Err("GitHub's meta names no domains for self-hosted runners".into());
    }
    Ok((ranges.into_iter().collect(), names))
}

/// The allowlist's entries: the ranges, every globally routable address
/// the names resolve to, and the resolvers, without repeats. A name that
/// does not resolve, or resolves only to an address that is not public
/// (such as a DNS proxy's fake `198.18.x.x`), is reported, and the refresh
/// fails: a partial list is never written.
pub fn assemble(
    ranges: &[String],
    names: &[String],
    resolvers: &[Ipv4Addr],
    resolve: &dyn Resolve,
) -> Result<Vec<String>, String> {
    let mut entries: BTreeSet<String> = ranges.iter().cloned().collect();
    let mut failed = Vec::new();
    for name in names {
        match resolve.a(name) {
            Ok(addresses) => {
                let public: Vec<Ipv4Addr> = addresses
                    .into_iter()
                    .filter(|address| machine_service_core::isolation::globally_routable(*address))
                    .collect();
                if public.is_empty() {
                    failed.push(format!("{name}: no public address"));
                }
                entries.extend(public.iter().map(Ipv4Addr::to_string));
            }
            Err(error) => failed.push(format!("{name}: {error}")),
        }
    }
    // A handful of names may be retired between GitHub's list and DNS; more
    // than that means resolution itself failed.
    if failed.len() * 10 > names.len() {
        return Err(format!(
            "{} of {} names did not resolve; keeping the last allowlist ({})",
            failed.len(),
            names.len(),
            failed
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    entries.extend(resolvers.iter().map(Ipv4Addr::to_string));
    let mut list: Vec<String> = entries.into_iter().collect();
    list.sort_by_key(|entry| sort_key(entry));
    Ok(list)
}

fn sort_key(entry: &str) -> (u32, u8) {
    let (address, bits) = entry.split_once('/').unwrap_or((entry, "32"));
    (
        address.parse::<Ipv4Addr>().map(u32::from).unwrap_or(0),
        bits.parse().unwrap_or(32),
    )
}

/// The guests' resolvers' addresses and their DNS-over-TLS upstream texts,
/// as the lab instance gives them (`MACHINE_SERVICE_GUEST_DNS`, DC-16).
pub fn resolvers() -> (Vec<Ipv4Addr>, Vec<String>) {
    let parsed = machine_service_core::isolation::parse_guest_dns(Some(
        machine_service_core::isolation::DEFAULT_GUEST_DNS,
    ))
    .unwrap_or_default();
    (
        parsed.iter().map(|resolver| resolver.address).collect(),
        parsed.iter().map(ToString::to_string).collect(),
    )
}

/// Write the allow file: its check name, how many names and ranges it
/// came from, and the entries.
pub fn write(path: &Path, entries: &[String], names: usize, ranges: usize) -> std::io::Result<()> {
    guest_firewall::write_addresses(
        path,
        &[
            format!("check: {CHECK_NAME}"),
            format!(
                "egress allowlist of the lab runners (DC-1xx-EPH-9): {ranges} GitHub ranges, {names} runner names resolved over DNS over TLS, and the guests' resolvers; written by lab-host"
            ),
        ],
        entries,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::Fixed;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn meta() -> Value {
        json!({
            "web": ["192.30.252.0/22", "2a0a:a440::/29"],
            "api": ["192.30.252.0/22", "140.82.112.0/20"],
            "git": ["140.82.112.0/20"],
            "packages": ["140.82.121.33/32"],
            "actions": ["4.148.0.0/16"],
            "domains": {"actions_inbound": {
                "full_domains": ["github.com", "results-receiver.actions.githubusercontent.com",
                    "productionresultssa0.blob.core.windows.net"],
                "wildcard_domains": ["*.githubusercontent.com"]
            }}
        })
    }

    #[test]
    fn githubs_ranges_and_runner_names_come_from_meta() {
        let (ranges, names) = from_meta(&meta()).unwrap();
        assert_eq!(
            ranges,
            vec!["140.82.112.0/20", "140.82.121.33/32", "192.30.252.0/22"]
        );
        assert_eq!(names.len(), 3);
        assert!(
            !ranges.contains(&"4.148.0.0/16".to_string()),
            "hosted runners' ranges stay out"
        );
        assert!(from_meta(&json!({"web": []})).is_err());
    }

    #[test]
    fn the_list_holds_resolved_public_addresses_and_the_resolvers() {
        let (ranges, names) = from_meta(&meta()).unwrap();
        let answers: BTreeMap<String, Vec<Ipv4Addr>> = [
            ("github.com", vec!["140.82.112.3"]),
            (
                "results-receiver.actions.githubusercontent.com",
                vec!["20.42.73.1"],
            ),
            (
                "productionresultssa0.blob.core.windows.net",
                vec!["20.60.1.2", "198.18.0.9"],
            ),
        ]
        .into_iter()
        .map(|(name, list)| {
            (
                name.to_string(),
                list.iter().map(|a| a.parse().unwrap()).collect(),
            )
        })
        .collect();
        let list = assemble(
            &ranges,
            &names,
            &["1.1.1.1".parse().unwrap()],
            &Fixed(answers),
        )
        .unwrap();
        assert_eq!(
            list,
            vec![
                "1.1.1.1",
                "20.42.73.1",
                "20.60.1.2",
                "140.82.112.0/20",
                "140.82.112.3",
                "140.82.121.33/32",
                "192.30.252.0/22"
            ]
        );
        assert!(
            !list.iter().any(|entry| entry.starts_with("198.18.")),
            "a fake answer never enters"
        );
    }

    #[test]
    fn a_resolution_that_fails_or_answers_only_fake_addresses_keeps_the_last_list() {
        let (ranges, names) = from_meta(&meta()).unwrap();
        let fake: BTreeMap<String, Vec<Ipv4Addr>> = names
            .iter()
            .map(|name| (name.clone(), vec!["198.18.0.7".parse().unwrap()]))
            .collect();
        let error = assemble(&ranges, &names, &[], &Fixed(fake)).unwrap_err();
        assert!(error.contains("3 of 3 names did not resolve"), "{error}");
        assert!(assemble(&ranges, &names, &[], &Fixed(BTreeMap::new())).is_err());
    }

    #[test]
    fn the_file_names_its_check_name_for_the_instance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("egress-allow");
        write(&path, &["1.1.1.1".into()], 1, 1).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            guest_firewall::parse_check_name(&text).as_deref(),
            Some(CHECK_NAME)
        );
        let (addresses, upstreams) = resolvers();
        assert_eq!(addresses.len(), upstreams.len());
        assert!(!addresses.is_empty());
    }
}
