//! Bounded JSON reads for discovery and aggregation (`docs/arch/federation.md`
//! FD-2, FD-3).
//!
//! Every request here carries a deadline, because one dead instance must
//! never hold the merged list hostage. Requests go direct, never through an
//! HTTP proxy from the environment: the Consul agent is on loopback and the
//! instances are on the LAN, and a proxy that cannot reach either would turn
//! a working instance into an unreachable one.

use std::time::Duration;

use serde_json::Value;

/// One `GET` of a JSON document within `timeout`, with `headers` added.
///
/// Answers the HTTP status and the parsed body for any status, or a short
/// reason when no answer arrived: `connection refused`, `timed out after
/// 5 s`, `host not found`, or the transport's own text. An empty body is an
/// empty object, as the CLI's other reads treat it.
pub fn get_json(
    url: &str,
    timeout: Duration,
    headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    request_json("GET", url, timeout, headers)
}

/// One `POST` with an empty body, read as [`get_json`] reads its answer:
/// a reload of an instance's image catalog (IC-3, CL-4).
pub fn post_json(
    url: &str,
    timeout: Duration,
    headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    request_json("POST", url, timeout, headers)
}

fn request_json(
    method: &str,
    url: &str,
    timeout: Duration,
    headers: &[(String, String)],
) -> Result<(u16, Value), String> {
    if timeout.is_zero() {
        return Err(timed_out(timeout));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .build()
        .into();
    let response = if method == "POST" {
        let mut request = agent
            .post(url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json");
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request.send("")
    } else {
        let mut request = agent.get(url).header("Accept", "application/json");
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request.call()
    };
    let mut response = response.map_err(|error| reason(&error, timeout))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| reason(&error, timeout))?;
    if text.trim().is_empty() {
        return Ok((status, Value::Object(Default::default())));
    }
    match serde_json::from_str(&text) {
        Ok(value) => Ok((status, value)),
        Err(error) => Err(format!(
            "HTTP {status} with a body that is not JSON ({error})"
        )),
    }
}

fn timed_out(timeout: Duration) -> String {
    let seconds = timeout.as_secs_f64();
    if seconds.fract() == 0.0 {
        format!("timed out after {} s", seconds as u64)
    } else {
        format!("timed out after {seconds:.1} s")
    }
}

/// The short reason a request got no answer.
fn reason(error: &ureq::Error, timeout: Duration) -> String {
    match error {
        ureq::Error::Timeout(_) => timed_out(timeout),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => timed_out(timeout),
            std::io::ErrorKind::ConnectionReset => "connection reset".to_string(),
            _ => {
                let text = io.to_string();
                match io.raw_os_error() {
                    Some(code) => text
                        .strip_suffix(&format!(" (os error {code})"))
                        .unwrap_or(&text)
                        .to_lowercase(),
                    None => text,
                }
            }
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_port_reads_as_connection_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = get_json(
            &format!("http://127.0.0.1:{port}/machines"),
            Duration::from_secs(2),
            &[],
        )
        .unwrap_err();
        assert_eq!(error, "connection refused");
    }

    /// FD-3: a listener that accepts and never answers costs the deadline,
    /// not more.
    #[test]
    fn silent_listener_times_out_within_the_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let started = std::time::Instant::now();
        let error = get_json(
            &format!("http://127.0.0.1:{port}/machines"),
            Duration::from_millis(300),
            &[],
        )
        .unwrap_err();
        assert!(error.starts_with("timed out after"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(listener);
    }

    #[test]
    fn zero_budget_fails_without_a_request() {
        assert_eq!(
            get_json("http://127.0.0.1:9/", Duration::ZERO, &[]).unwrap_err(),
            "timed out after 0 s"
        );
    }
}
