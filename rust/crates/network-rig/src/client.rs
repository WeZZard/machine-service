//! A WebSocket client for rig tests, standing where the relay client and the
//! relay agent will stand.
//!
//! It connects the way any client of the rig must: it dials the address the
//! rig resolves the broker's name to, and it speaks TLS and HTTP for the name
//! itself, so the name, not an address, is what reaches the edge (TC-6).

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

use crate::rig::ClientSide;
use crate::tls;

/// A client's TLS stream.
pub type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// How an exchange over the chain ended, in the terms a fault test compares.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The echo came back.
    Ok,
    /// The connection was reset (a TCP RST).
    Reset,
    /// The connection was closed, without a reset.
    Closed,
    /// Nothing came back in time and the connection stayed open.
    Stalled,
    /// The WebSocket was closed with this code.
    CloseFrame(u16),
    /// The upgrade was answered with this HTTP status.
    Refused(u16),
    /// The TLS handshake failed.
    Tls(String),
    /// Anything else.
    Other(String),
}

/// Classify a WebSocket error.
pub fn classify(error: &tungstenite::Error) -> Outcome {
    match error {
        tungstenite::Error::Io(error) => classify_io(error),
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => Outcome::Closed,
        tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => Outcome::Closed,
        tungstenite::Error::Http(response) => Outcome::Refused(response.status().as_u16()),
        other => Outcome::Other(other.to_string()),
    }
}

fn classify_io(error: &io::Error) -> Outcome {
    match error.kind() {
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => Outcome::Reset,
        io::ErrorKind::UnexpectedEof => Outcome::Closed,
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Outcome::Stalled,
        io::ErrorKind::InvalidData => match error
            .get_ref()
            .and_then(|e| e.downcast_ref::<rustls::Error>())
        {
            Some(tls) => Outcome::Tls(tls.to_string()),
            None => Outcome::Other(error.to_string()),
        },
        _ => Outcome::Other(error.to_string()),
    }
}

/// A WebSocket over TLS through the chain.
pub struct Session {
    /// The WebSocket.
    pub socket: WebSocket<TlsStream>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

/// Where and how to connect.
#[derive(Clone)]
pub struct Dial {
    /// The address to dial.
    pub addr: SocketAddr,
    /// The TLS server name: a host name, or an address for a negative test.
    pub server_name: ServerName<'static>,
    /// The URL of the upgrade request; its host is the `Host` header.
    pub url: String,
    /// The TLS configuration, with the roots the client trusts.
    pub tls: Arc<ClientConfig>,
}

impl Dial {
    /// Dial the broker through the rig, as the rig tells its clients to.
    pub fn through(client: &ClientSide) -> Result<Dial, crate::RigError> {
        let roots = std::fs::read_to_string(&client.ca_bundle)
            .map_err(crate::RigError::io("read the client CA bundle"))?;
        Ok(Dial {
            addr: client.connect,
            server_name: ServerName::try_from(client.host.clone())
                .map_err(|e| crate::RigError::Tls(e.to_string()))?,
            url: client.url.clone(),
            tls: tls::client_config(&roots)?,
        })
    }
}

impl Session {
    /// Open a session, with `timeout` for each read and write.
    pub fn open(dial: &Dial, timeout: Duration) -> Result<Session, Outcome> {
        let tcp = TcpStream::connect_timeout(&dial.addr, timeout).map_err(|e| classify_io(&e))?;
        let _ = tcp.set_nodelay(true);
        tcp.set_read_timeout(Some(timeout))
            .map_err(|e| classify_io(&e))?;
        tcp.set_write_timeout(Some(timeout))
            .map_err(|e| classify_io(&e))?;
        let connection = ClientConnection::new(dial.tls.clone(), dial.server_name.clone())
            .map_err(|e| Outcome::Tls(e.to_string()))?;
        let mut stream = StreamOwned::new(connection, tcp);
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .map_err(|e| classify_io(&e))?;
        }
        let request = dial
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| classify(&e))?;
        let (socket, _) = tungstenite::client(request, stream).map_err(|error| match error {
            tungstenite::HandshakeError::Failure(error) => classify(&error),
            tungstenite::HandshakeError::Interrupted(_) => Outcome::Stalled,
        })?;
        Ok(Session { socket })
    }

    /// The certificate chain the far end of the TLS connection presented.
    pub fn peer_certificates(&self) -> Vec<Vec<u8>> {
        self.socket
            .get_ref()
            .conn
            .peer_certificates()
            .map(|certs| certs.iter().map(|cert| cert.to_vec()).collect())
            .unwrap_or_default()
    }

    /// The local address of the TCP connection.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.get_ref().sock.local_addr().ok()
    }

    /// Change the read timeout.
    pub fn set_read_timeout(&self, timeout: Duration) {
        let _ = self.socket.get_ref().sock.set_read_timeout(Some(timeout));
    }

    /// Send `payload` and wait for its echo, skipping control frames.
    pub fn echo(&mut self, payload: &[u8]) -> Outcome {
        if let Err(error) = self.socket.send(Message::binary(payload.to_vec())) {
            return classify(&error);
        }
        loop {
            match self.socket.read() {
                Ok(Message::Binary(data)) if data.as_ref() == payload => return Outcome::Ok,
                Ok(Message::Binary(_) | Message::Text(_)) => {
                    return Outcome::Other("the echo did not match".into())
                }
                Ok(Message::Close(frame)) => {
                    return frame
                        .map(|f| Outcome::CloseFrame(f.code.into()))
                        .unwrap_or(Outcome::Closed)
                }
                Ok(_) => continue,
                Err(error) => return classify(&error),
            }
        }
    }

    /// Wait for the far side to end the connection, answering pings, until
    /// `timeout` passes. Returns how it ended, or `Stalled` if it did not.
    pub fn wait_for_end(&mut self, timeout: Duration) -> Outcome {
        self.set_read_timeout(timeout);
        loop {
            match self.socket.read() {
                Ok(Message::Close(frame)) => {
                    return frame
                        .map(|f| Outcome::CloseFrame(f.code.into()))
                        .unwrap_or(Outcome::Closed)
                }
                Ok(_) => continue,
                Err(error) => return classify(&error),
            }
        }
    }

    /// Send a WebSocket ping, the heartbeat of RS-4.
    pub fn ping(&mut self) -> Outcome {
        match self.socket.send(Message::Ping(Vec::new().into())) {
            Ok(()) => Outcome::Ok,
            Err(error) => classify(&error),
        }
    }
}
