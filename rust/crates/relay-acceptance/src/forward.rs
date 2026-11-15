//! A loopback forwarder to a test instance on another host.
//!
//! mcp-relay takes a machine-service origin only on loopback or from its own
//! discovery (mcp-relay `MachineService`), and a second instance never
//! advertises itself, so the suite gives mcp-relay a loopback origin that
//! this process forwards to the test instance, byte for byte, for as long as
//! the process lives. Each accepted connection dials the instance afresh, so
//! a restart of the instance (AC-64, AC-65) is met by the next connection.
//! Only machine-service's HTTP goes this way; the relay link goes through the
//! rig to the broker as before.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

/// The `host` and `port` of an `http://host:port` origin.
pub fn host_port(origin: &str) -> Option<(String, u16)> {
    let rest = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let authority = rest.split('/').next()?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !host.ends_with('[') && (!host.contains(':') || host.ends_with(']')) =>
        {
            (host, port.parse().ok()?)
        }
        _ => (authority, 80),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// Whether an origin is on loopback, which mcp-relay takes as it is.
pub fn loopback(origin: &str) -> bool {
    host_port(origin)
        .is_some_and(|(host, _)| host == "localhost" || host == "::1" || host.starts_with("127."))
}

/// The origin mcp-relay is given for `origin`: itself on loopback, else a
/// loopback forwarder to it that runs until the process ends.
pub fn loopback_origin(origin: &str) -> Result<String, String> {
    if loopback(origin) {
        return Ok(origin.trim_end_matches('/').to_string());
    }
    let (host, port) = host_port(origin).ok_or_else(|| format!("not an http origin: {origin}"))?;
    forwarder(host, port)
}

/// A loopback listener whose every connection is carried to `host:port`.
fn forwarder(host: String, port: u16) -> Result<String, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("forwarder: {e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("forwarder: {e}"))?;
    thread::spawn(move || {
        for incoming in listener.incoming().flatten() {
            let (host, port) = (host.clone(), port);
            thread::spawn(move || {
                let _ = forward(incoming, &host, port);
            });
        }
    });
    Ok(format!("http://{local}"))
}

fn forward(client: TcpStream, host: &str, port: u16) -> std::io::Result<()> {
    let address = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("the instance's name does not resolve"))?;
    let upstream = TcpStream::connect_timeout(&address, Duration::from_secs(20))?;
    let (mut client_in, mut upstream_out) = (client.try_clone()?, upstream.try_clone()?);
    let pump = thread::spawn(move || {
        let _ = copy(&mut client_in, &mut upstream_out);
        let _ = upstream_out.shutdown(Shutdown::Write);
    });
    let (mut upstream_in, mut client_out) = (upstream, client);
    let _ = copy(&mut upstream_in, &mut client_out);
    let _ = client_out.shutdown(Shutdown::Write);
    let _ = pump.join();
    Ok(())
}

fn copy(from: &mut TcpStream, to: &mut TcpStream) -> std::io::Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = from.read(&mut buffer)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buffer[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_parse_and_loopback_is_kept() {
        assert_eq!(
            host_port("http://mac.local:6262"),
            Some(("mac.local".into(), 6262))
        );
        assert_eq!(
            host_port("http://[2001:db8::1]:6262/"),
            Some(("2001:db8::1".into(), 6262))
        );
        assert_eq!(
            host_port("http://192.0.2.3"),
            Some(("192.0.2.3".into(), 80))
        );
        assert!(loopback("http://127.0.0.1:6262"));
        assert!(!loopback("http://192.0.2.3:6262"));
        assert_eq!(
            loopback_origin("http://127.0.0.1:6262/").unwrap(),
            "http://127.0.0.1:6262"
        );
    }

    #[test]
    fn the_forwarder_carries_a_request_and_its_answer() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        thread::spawn(move || {
            for mut stream in server.incoming().flatten() {
                let mut line = [0u8; 5];
                stream.read_exact(&mut line).unwrap();
                stream.write_all(&line).unwrap();
            }
        });
        let origin = forwarder("127.0.0.1".into(), address.port()).unwrap();
        assert_ne!(origin, format!("http://{address}"));
        for _ in 0..2 {
            let (host, port) = host_port(&origin).unwrap();
            let mut stream = TcpStream::connect((host.as_str(), port)).unwrap();
            stream.write_all(b"hello").unwrap();
            let mut echoed = [0u8; 5];
            stream.read_exact(&mut echoed).unwrap();
            assert_eq!(&echoed, b"hello");
        }
    }
}
