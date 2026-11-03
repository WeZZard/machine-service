//! Names resolved over DNS over TLS to the guests' own public resolvers,
//! never through the host's resolver, whose network may answer with fake
//! `198.18.x.x` addresses (LR-5, DC-1xx-EPH-10).

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

/// Resolve a name to its IPv4 addresses.
pub trait Resolve: Send + Sync {
    /// The name's A records.
    fn a(&self, name: &str) -> Result<Vec<Ipv4Addr>, String>;
}

/// An A query for `name` with the message ID `id`, recursion desired.
pub fn query(id: u16, name: &str) -> Result<Vec<u8>, String> {
    let mut message = Vec::with_capacity(32 + name.len());
    message.extend_from_slice(&id.to_be_bytes());
    message.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("not a host name: {name:?}"));
        }
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.push(0);
    message.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
    Ok(message)
}

/// Skip a possibly compressed name at `at`; answers the offset after it.
fn skip_name(message: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = *message.get(at)? as usize;
        if length == 0 {
            return Some(at + 1);
        }
        if length & 0xc0 == 0xc0 {
            return Some(at + 2);
        }
        at += 1 + length;
    }
}

/// The A records of an answer to the query `id`, or why there are none.
pub fn parse_answer(id: u16, message: &[u8]) -> Result<Vec<Ipv4Addr>, String> {
    if message.len() < 12 {
        return Err("short DNS answer".into());
    }
    if u16::from_be_bytes([message[0], message[1]]) != id {
        return Err("DNS answer to another query".into());
    }
    let rcode = message[3] & 0x0f;
    if rcode != 0 {
        return Err(format!("DNS answered rcode {rcode}"));
    }
    let questions = u16::from_be_bytes([message[4], message[5]]);
    let answers = u16::from_be_bytes([message[6], message[7]]);
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(message, at).ok_or("bad DNS question")? + 4;
    }
    let mut addresses = Vec::new();
    for _ in 0..answers {
        at = skip_name(message, at).ok_or("bad DNS answer")?;
        let header = message.get(at..at + 10).ok_or("short DNS record")?;
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let class = u16::from_be_bytes([header[2], header[3]]);
        let length = u16::from_be_bytes([header[8], header[9]]) as usize;
        let data = message
            .get(at + 10..at + 10 + length)
            .ok_or("short DNS record data")?;
        if kind == 1 && class == 1 && length == 4 {
            addresses.push(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        }
        at += 10 + length;
    }
    Ok(addresses)
}

/// DNS over TLS to public resolvers, the guests' own (DC-16).
pub struct OverTls {
    resolver: guest_dot_forwarder::Resolver,
}

impl OverTls {
    /// A resolver over `upstreams`, each `a.b.c.d#name` as
    /// `MACHINE_SERVICE_GUEST_DNS` writes them.
    pub fn new(upstreams: &[String]) -> Result<OverTls, String> {
        let upstreams = upstreams
            .iter()
            .map(|text| guest_dot_forwarder::Upstream::parse(text))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OverTls {
            resolver: guest_dot_forwarder::Resolver::new(
                upstreams,
                guest_dot_forwarder::tls_config(),
            ),
        })
    }
}

impl Resolve for OverTls {
    fn a(&self, name: &str) -> Result<Vec<Ipv4Addr>, String> {
        let id: u16 = rand::random();
        let message = self
            .resolver
            .resolve(&query(id, name)?)
            .ok_or_else(|| format!("no resolver answered for {name}"))?;
        parse_answer(id, &message)
    }
}

/// Fixed answers, for a test build's seams.
pub struct Fixed(pub BTreeMap<String, Vec<Ipv4Addr>>);

impl Resolve for Fixed {
    fn a(&self, name: &str) -> Result<Vec<Ipv4Addr>, String> {
        self.0
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no answer for {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_and_its_compressed_answer_round_trip() {
        let request = query(0x1234, "api.github.com").unwrap();
        assert_eq!(&request[..4], &[0x12, 0x34, 0x01, 0x00]);
        assert_eq!(&request[12..16], &[3, b'a', b'p', b'i']);
        // An answer: the question, a CNAME, then an A record, both with
        // compressed names.
        let mut answer = request.clone();
        answer[2] = 0x81;
        answer[3] = 0x80;
        answer[7] = 2;
        answer.extend_from_slice(&[0xc0, 0x0c, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 0x10]);
        answer.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 140, 82, 112, 6]);
        assert_eq!(
            parse_answer(0x1234, &answer).unwrap(),
            vec![Ipv4Addr::new(140, 82, 112, 6)]
        );
        assert!(parse_answer(0x9999, &answer).is_err());
        let mut refused = request.clone();
        refused[3] = 0x83;
        assert!(parse_answer(0x1234, &refused)
            .unwrap_err()
            .contains("rcode 3"));
        assert!(query(1, "a..b").is_err());
    }
}
