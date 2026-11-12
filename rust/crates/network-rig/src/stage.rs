//! The reset stage: the rig's own TCP relay, which cuts a chosen open
//! connection with a real TCP reset (DC-137).
//!
//! Toxiproxy's `reset_peer` resets only connections dialled while it is
//! armed; a connection that is already open is cut without a reset when it
//! next carries data. An abrupt reset of an open connection is a fault the
//! rig owns, so the stage is the rig's code and Toxiproxy stays unmodified.
//!
//! The stage is two hops, one at each end of the chain, so a reset reaches
//! the end it is meant for and not a proxy in between:
//!
//! ```text
//! client --> front hop --> Toxiproxy --> edge --> back hop --> broker
//!            (RST toward                          (RST toward
//!             the client)                          the broker)
//! ```
//!
//! A cut closes the chosen side's socket with a zero linger time, which sends
//! a TCP RST, and closes the hop's other socket normally, so the rest of the
//! chain sees an ordinary close. A reset or error that reaches a hop from one
//! side is passed on as a reset to the other, so the stage never turns a
//! reset by Toxiproxy into a clean close.
//!
//! A connection is named by the front hop's accept order. Its broker leg is
//! the back hop's connection that the edge opened for it; the two are paired
//! by the random of the client's TLS ClientHello, which the front hop reads
//! as it passes and the edge writes, with the source port of its connection
//! to the back hop, into the run's pairs file.
//!
//! Each connection is one thread that polls both sockets and forwards bytes as
//! soon as they arrive, with Nagle off, so the stage adds tens of
//! microseconds to a round trip (measured by the `rig_live` self-test).
//!
//! The back hop also carries the address-change fault (TC-4): on Linux, where
//! all of `127.0.0.0/8` is loopback and needs no root, each change moves the
//! back hop's connections to the next loopback source address, so the broker
//! sees a new source address and port; on macOS, whose loopback has one
//! address, only the source port changes.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockRef, Socket, Type};

use crate::admit::Admit;
use crate::RigError;

/// Which end of a connection a cut resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The client or relay agent receives the reset.
    Client,
    /// The broker receives the reset.
    Broker,
    /// Both ends receive a reset.
    Both,
}

impl Side {
    fn client(self) -> bool {
        matches!(self, Side::Client | Side::Both)
    }

    fn broker(self) -> bool {
        matches!(self, Side::Broker | Side::Both)
    }
}

/// Which open connections a cut acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    /// Every open connection.
    All,
    /// The connection with this ID ([`Connection::id`]).
    Id(u64),
    /// The open connection at `pick` modulo their number, in the order they
    /// were opened: the schedule's choice, which a replay repeats as long as
    /// the same connections are open.
    Pick(u64),
}

/// An open connection through the stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The connection's ID, in the order the stage accepted them.
    pub id: u64,
    /// The client's address, as the stage sees it.
    pub client: SocketAddr,
    /// The source address of the broker leg, as the broker sees it, once
    /// the edge has opened that leg.
    pub broker_source: Option<SocketAddr>,
}

/// What the address-change fault changes on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressChange {
    /// The broker sees a new loopback source address, and a new port.
    SourceAddress,
    /// The broker sees a new source port only.
    SourcePort,
}

/// The running stage.
#[derive(Debug)]
pub struct Stage {
    front: Hop,
    back: Hop,
    pairs: PathBuf,
    source: Arc<SourceAddress>,
}

impl Stage {
    /// Start both hops: the front one forwards to `edge_side` (Toxiproxy's
    /// listener), the back one to `broker`. `pairs` is the file the edge
    /// writes its pairs to.
    pub fn start(
        edge_side: SocketAddr,
        broker: SocketAddr,
        pairs: &Path,
    ) -> Result<Stage, RigError> {
        Stage::start_on(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            Admit::Any,
            edge_side,
            broker,
            pairs,
        )
    }

    /// [`Stage::start`] with the front hop, the chain's one port, listening
    /// on `listen` and admitting `admit`: a relay agent's chain on a VM host
    /// listens where the host's guests reach it, and admits only them.
    pub fn start_on(
        listen: SocketAddr,
        admit: Admit,
        edge_side: SocketAddr,
        broker: SocketAddr,
        pairs: &Path,
    ) -> Result<Stage, RigError> {
        let source = Arc::new(SourceAddress::detect(broker));
        let back = Hop::start_with(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            broker,
            Some(source.clone()),
            false,
            Admit::Any,
        )?;
        let front = Hop::start_with(listen, edge_side, None, true, admit)?;
        Ok(Stage {
            front,
            back,
            pairs: pairs.into(),
            source,
        })
    }

    /// Where clients connect: the chain's one port.
    pub fn addr(&self) -> SocketAddr {
        self.front.addr()
    }

    /// Where the edge connects to reach the broker.
    pub fn broker_side(&self) -> SocketAddr {
        self.back.addr()
    }

    /// What an address change changes here.
    pub fn address_change(&self) -> AddressChange {
        if self.source.rotates {
            AddressChange::SourceAddress
        } else {
            AddressChange::SourcePort
        }
    }

    /// Make the broker legs dialled from now on come from the next source
    /// address. Open connections are left alone; the rig closes them.
    pub fn change_address(&self) {
        self.source.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// The open connections, in the order they were opened.
    pub fn connections(&self) -> Vec<Connection> {
        let pairs = self.pairs();
        let backs = self.back.entries();
        self.front
            .entries()
            .into_iter()
            .map(|(id, entry)| Connection {
                id,
                client: entry.peer,
                broker_source: broker_leg(&entry, &pairs, &backs).map(|(_, back)| back.local),
            })
            .collect()
    }

    /// Cut the selected connections on `side` with a TCP reset, and return
    /// the IDs of the connections cut. Returns when the sockets are closed.
    ///
    /// The broker side of a connection whose broker leg is not open yet is
    /// the front hop's socket toward the edge, so the cut still reaches the
    /// chain on the broker's side.
    pub fn cut(&self, select: Select, side: Side) -> Vec<u64> {
        let fronts = self.front.entries();
        let backs = self.back.entries();
        let pairs = self.pairs();
        let mut orders: Vec<(&Hop, u64, Close)> = Vec::new();
        let chosen: Vec<u64> = match select {
            Select::All => {
                if side.client() {
                    orders.extend(fronts.iter().map(|(id, _)| (&self.front, *id, Close::DOWN)));
                }
                if side.broker() {
                    orders.extend(backs.iter().map(|(id, _)| (&self.back, *id, Close::UP)));
                }
                fronts.iter().map(|(id, _)| *id).collect()
            }
            Select::Id(_) | Select::Pick(_) => {
                let found = match select {
                    Select::Id(id) => fronts.iter().find(|(open, _)| *open == id),
                    Select::Pick(pick) if !fronts.is_empty() => {
                        fronts.get((pick % fronts.len() as u64) as usize)
                    }
                    _ => None,
                };
                let Some((id, entry)) = found else {
                    return Vec::new();
                };
                let leg = broker_leg(entry, &pairs, &backs).map(|(back, _)| back);
                match (side, leg) {
                    (Side::Client, _) => orders.push((&self.front, *id, Close::DOWN)),
                    (Side::Broker, Some(back)) => orders.push((&self.back, back, Close::UP)),
                    (Side::Broker, None) => orders.push((&self.front, *id, Close::UP)),
                    (Side::Both, Some(back)) => {
                        orders.push((&self.front, *id, Close::DOWN));
                        orders.push((&self.back, back, Close::UP));
                    }
                    (Side::Both, None) => orders.push((&self.front, *id, Close::BOTH)),
                }
                vec![*id]
            }
        };
        for (hop, id, close) in &orders {
            hop.order(*id, *close);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while orders.iter().any(|(hop, id, _)| hop.is_open(*id)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        chosen
    }

    /// The edge's pairs so far: ClientHello random to the source port of the
    /// edge's connection to the back hop. A later line wins.
    fn pairs(&self) -> HashMap<String, u16> {
        #[derive(Deserialize)]
        struct Pair {
            random: String,
            port: u16,
        }
        std::fs::read_to_string(&self.pairs)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Pair>(line).ok())
            .map(|pair| (pair.random, pair.port))
            .collect()
    }
}

/// The back hop's connection that carries `front`'s broker leg.
fn broker_leg<'a>(
    front: &Entry,
    pairs: &HashMap<String, u16>,
    backs: &'a [(u64, Entry)],
) -> Option<(u64, &'a Entry)> {
    let port = pairs.get(front.hello.as_deref()?)?;
    backs
        .iter()
        .find(|(_, back)| back.peer.port() == *port)
        .map(|(id, back)| (*id, back))
}

/// The source address of the back hop's connections.
#[derive(Debug)]
struct SourceAddress {
    rotates: bool,
    epoch: AtomicU32,
}

impl SourceAddress {
    /// Rotate through loopback addresses when the broker is on loopback and
    /// this machine has a second loopback address that a process can bind
    /// without root. A loopback source cannot reach a broker on another
    /// machine, which then sees a new source port only.
    fn detect(broker: SocketAddr) -> SourceAddress {
        SourceAddress {
            rotates: broker.ip().is_loopback() && TcpListener::bind((loopback(1), 0)).is_ok(),
            epoch: AtomicU32::new(0),
        }
    }

    fn current(&self) -> Option<Ipv4Addr> {
        self.rotates
            .then(|| loopback(self.epoch.load(Ordering::SeqCst) % 254))
    }
}

/// The loopback address `offset` after `127.0.0.1`.
fn loopback(offset: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(Ipv4Addr::LOCALHOST) + offset)
}

/// How a hop closes a connection's two sockets: `down` toward the side that
/// dialled the hop, `up` toward the side the hop dialled. A socket marked
/// for a reset is closed with a zero linger time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Close {
    reset_down: bool,
    reset_up: bool,
}

impl Close {
    const NORMAL: Close = Close {
        reset_down: false,
        reset_up: false,
    };
    const DOWN: Close = Close {
        reset_down: true,
        reset_up: false,
    };
    const UP: Close = Close {
        reset_down: false,
        reset_up: true,
    };
    const BOTH: Close = Close {
        reset_down: true,
        reset_up: true,
    };
}

/// One relay of the stage: accepts on a loopback port and forwards each
/// connection to its upstream.
#[derive(Debug)]
pub struct Hop {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct Shared {
    upstream: SocketAddr,
    source: Option<Arc<SourceAddress>>,
    read_hello: bool,
    admit: Admit,
    next_id: AtomicU64,
    open: Mutex<BTreeMap<u64, Entry>>,
    stopping: AtomicBool,
}

#[derive(Debug, Clone)]
struct Entry {
    /// Who dialled the hop.
    peer: SocketAddr,
    /// The hop's own address on its connection upstream.
    local: SocketAddr,
    /// The ClientHello random, hex, once it has passed.
    hello: Option<String>,
    order: Arc<Order>,
}

#[derive(Debug)]
struct Order {
    close: Mutex<Option<Close>>,
    wake: UnixStream,
}

impl Hop {
    /// A hop on a free loopback port that forwards to `upstream`.
    pub fn start(upstream: SocketAddr) -> Result<Hop, RigError> {
        Hop::start_with(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            upstream,
            None,
            false,
            Admit::Any,
        )
    }

    fn start_with(
        listen: SocketAddr,
        upstream: SocketAddr,
        source: Option<Arc<SourceAddress>>,
        read_hello: bool,
        admit: Admit,
    ) -> Result<Hop, RigError> {
        let listener = TcpListener::bind(listen)
            .map_err(RigError::io(format!("bind the reset stage on {listen}")))?;
        let addr = listener
            .local_addr()
            .map_err(RigError::io("read the reset stage's address"))?;
        let shared = Arc::new(Shared {
            upstream,
            source,
            read_hello,
            admit,
            next_id: AtomicU64::new(0),
            open: Mutex::new(BTreeMap::new()),
            stopping: AtomicBool::new(false),
        });
        let accept = {
            let shared = shared.clone();
            std::thread::spawn(move || accept_loop(listener, shared))
        };
        Ok(Hop {
            addr,
            shared,
            accept: Some(accept),
        })
    }

    /// Where the hop listens.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The number of open connections.
    pub fn open(&self) -> usize {
        self.entries().len()
    }

    fn entries(&self) -> Vec<(u64, Entry)> {
        self.shared
            .open
            .lock()
            .map(|open| open.iter().map(|(id, e)| (*id, e.clone())).collect())
            .unwrap_or_default()
    }

    fn is_open(&self, id: u64) -> bool {
        self.shared
            .open
            .lock()
            .map(|open| open.contains_key(&id))
            .unwrap_or(false)
    }

    fn order(&self, id: u64, close: Close) {
        let order = self
            .shared
            .open
            .lock()
            .ok()
            .and_then(|open| open.get(&id).map(|entry| entry.order.clone()));
        if let Some(order) = order {
            order.send(close);
        }
    }
}

impl Order {
    fn send(&self, close: Close) {
        if let Ok(mut slot) = self.close.lock() {
            slot.get_or_insert(close);
        }
        let _ = (&self.wake).write(&[1]);
    }
}

impl Drop for Hop {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop, then close what is still open.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        for (_, entry) in self.entries() {
            entry.order.send(Close::NORMAL);
        }
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    for down in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(down) = down else { continue };
        // A peer the hop does not admit is closed before anything is read.
        if !down
            .peer_addr()
            .is_ok_and(|peer| shared.admit.admits(peer.ip()))
        {
            continue;
        }
        let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
        let shared = shared.clone();
        std::thread::spawn(move || serve(&shared, id, down));
    }
}

fn dial(shared: &Shared) -> io::Result<TcpStream> {
    let Some(source) = shared.source.as_ref().and_then(|s| s.current()) else {
        return TcpStream::connect(shared.upstream);
    };
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&SocketAddr::from((source, 0)).into())?;
    socket.connect(&shared.upstream.into())?;
    Ok(socket.into())
}

fn serve(shared: &Shared, id: u64, down: TcpStream) {
    let up = match dial(shared) {
        Ok(up) => up,
        Err(_) => {
            // Nothing to forward to: refuse the way a dead upstream does.
            close(down, true);
            return;
        }
    };
    let setup = (|| {
        let peer = down.peer_addr()?;
        let local = up.local_addr()?;
        for socket in [&down, &up] {
            socket.set_nodelay(true)?;
            socket.set_nonblocking(true)?;
        }
        let (wake, woken) = UnixStream::pair()?;
        woken.set_nonblocking(true)?;
        wake.set_nonblocking(true)?;
        Ok::<_, io::Error>((peer, local, wake, woken))
    })();
    let Ok((peer, local, wake, woken)) = setup else {
        close(down, true);
        close(up, true);
        return;
    };
    let order = Arc::new(Order {
        close: Mutex::new(None),
        wake,
    });
    if let Ok(mut open) = shared.open.lock() {
        open.insert(
            id,
            Entry {
                peer,
                local,
                hello: None,
                order: order.clone(),
            },
        );
    }
    let hello = |random: String| {
        if let Ok(mut open) = shared.open.lock() {
            if let Some(entry) = open.get_mut(&id) {
                entry.hello = Some(random);
            }
        }
    };
    let ending = if shared.stopping.load(Ordering::SeqCst) {
        Close::NORMAL
    } else {
        relay(&down, &up, &woken, &order, shared.read_hello, hello)
    };
    close(down, ending.reset_down);
    close(up, ending.reset_up);
    if let Ok(mut open) = shared.open.lock() {
        open.remove(&id);
    }
}

/// Close `socket`, with a TCP reset when `reset` holds.
fn close(socket: TcpStream, reset: bool) {
    if reset {
        let _ = SockRef::from(&socket).set_linger(Some(Duration::ZERO));
    }
    drop(socket);
}

const BUFFER: usize = 64 * 1024;

/// One direction of a connection.
struct Pipe {
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    eof: bool,
    shut: bool,
}

impl Pipe {
    fn new() -> Pipe {
        Pipe {
            buffer: vec![0; BUFFER].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            shut: false,
        }
    }

    fn pending(&self) -> bool {
        self.start < self.end
    }

    fn wants_read(&self) -> bool {
        !self.eof && !self.pending()
    }

    /// Read from `from`. An error is the connection's end.
    fn fill(&mut self, mut from: &TcpStream) -> io::Result<&[u8]> {
        match from.read(&mut self.buffer) {
            Ok(0) => {
                self.eof = true;
                Ok(&[])
            }
            Ok(read) => {
                (self.start, self.end) = (0, read);
                Ok(&self.buffer[..read])
            }
            Err(error) if transient(&error) => Ok(&[]),
            Err(error) => Err(error),
        }
    }

    /// Write what is pending to `to`, and pass on an end once it is written.
    fn drain(&mut self, mut to: &TcpStream) -> io::Result<()> {
        while self.pending() {
            match to.write(&self.buffer[self.start..self.end]) {
                Ok(written) => self.start += written,
                Err(error) if transient(&error) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        if self.eof && !self.shut {
            self.shut = true;
            let _ = to.shutdown(Shutdown::Write);
        }
        Ok(())
    }

    fn done(&self) -> bool {
        self.eof && self.shut
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Forward both directions until both end, an error ends one, or an order
/// comes. Returns how to close the two sockets.
fn relay(
    down: &TcpStream,
    up: &TcpStream,
    woken: &UnixStream,
    order: &Order,
    read_hello: bool,
    on_hello: impl Fn(String),
) -> Close {
    let mut toward_up = Pipe::new();
    let mut toward_down = Pipe::new();
    let mut hello = read_hello.then(Vec::new);
    // A socket whose reads wait for the other side to drain is still watched
    // for an error, so a reset reaches the other side at once. A hang-up
    // without an error mutes it until its reads resume, so it cannot spin.
    let (mut down_muted, mut up_muted) = (false, false);
    loop {
        let flag = |on: bool, flag: libc::c_short| if on { flag } else { 0 };
        let interest = |reading: &Pipe, writing: &Pipe| {
            flag(reading.wants_read(), libc::POLLIN) | flag(writing.pending(), libc::POLLOUT)
        };
        down_muted &= toward_up.pending();
        up_muted &= toward_down.pending();
        // A negative descriptor is skipped: a socket with nothing left to
        // read or write never wakes the loop.
        let pollfd = |fd: i32, events: libc::c_short, watched: bool| libc::pollfd {
            fd: if events != 0 || watched { fd } else { -1 },
            events,
            revents: 0,
        };
        let mut fds = [
            pollfd(
                down.as_raw_fd(),
                interest(&toward_up, &toward_down),
                !toward_up.eof && !down_muted,
            ),
            pollfd(
                up.as_raw_fd(),
                interest(&toward_down, &toward_up),
                !toward_down.eof && !up_muted,
            ),
            pollfd(woken.as_raw_fd(), libc::POLLIN, true),
        ];
        // SAFETY: poll(2) on three pollfd structs this function owns.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Close::BOTH;
        }
        if fds[2].revents != 0 {
            let mut drained = [0u8; 16];
            while matches!((&*woken).read(&mut drained), Ok(n) if n > 0) {}
            if let Some(close) = order.close.lock().ok().and_then(|slot| *slot) {
                return close;
            }
        }
        // An error on a socket whose reads wait is passed on as a reset.
        for (index, socket, muted, other) in [
            (0, down, &mut down_muted, Close::UP),
            (1, up, &mut up_muted, Close::DOWN),
        ] {
            let hung_up = fds[index].revents & (libc::POLLHUP | libc::POLLERR) != 0;
            if fds[index].events & libc::POLLIN == 0 && hung_up {
                match socket.take_error() {
                    Ok(None) => *muted = true,
                    _ => return other,
                }
            }
        }
        // Read what is ready and write it on at once; a failure on one socket
        // resets the other.
        let readable =
            |revents: libc::c_short| revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        if readable(fds[0].revents) && toward_up.wants_read() {
            match toward_up.fill(down) {
                Ok(data) => {
                    if let Some(buffer) = hello.as_mut() {
                        let take = data.len().min(HELLO_PREFIX - buffer.len());
                        buffer.extend_from_slice(&data[..take]);
                        if buffer.len() == HELLO_PREFIX {
                            if let Some(random) = client_random(buffer) {
                                on_hello(random);
                            }
                            hello = None;
                        }
                    }
                }
                Err(_) => return Close::UP,
            }
        }
        if readable(fds[1].revents) && toward_down.wants_read() && toward_down.fill(up).is_err() {
            return Close::DOWN;
        }
        if toward_up.drain(up).is_err() {
            return Close::DOWN;
        }
        if toward_down.drain(down).is_err() {
            return Close::UP;
        }
        if toward_up.done() && toward_down.done() {
            return Close::NORMAL;
        }
    }
}

/// The bytes of a TLS record up to the end of the ClientHello's random: the
/// record header (5), the handshake header (4), the version (2), the random (32).
const HELLO_PREFIX: usize = 43;

/// The random of a ClientHello at the start of a stream, hex.
fn client_random(prefix: &[u8]) -> Option<String> {
    const HANDSHAKE: u8 = 0x16;
    const CLIENT_HELLO: u8 = 0x01;
    if prefix.len() < HELLO_PREFIX || prefix[0] != HANDSHAKE || prefix[5] != CLIENT_HELLO {
        return None;
    }
    Some(
        prefix[11..HELLO_PREFIX]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain TCP echo server.
    fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    loop {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if stream.write_all(&buffer[..n]).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    fn connect(addr: SocketAddr) -> TcpStream {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
    }

    fn echo(stream: &mut TcpStream, data: &[u8]) -> io::Result<Vec<u8>> {
        stream.write_all(data)?;
        let mut back = vec![0u8; data.len()];
        stream.read_exact(&mut back)?;
        Ok(back)
    }

    fn wait_open(hop: &Hop, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while hop.open() != count {
            assert!(
                Instant::now() < deadline,
                "{} open, not {count}",
                hop.open()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_hop_forwards_both_ways_and_passes_on_a_close() {
        let hop = Hop::start(echo_server()).unwrap();
        let mut stream = connect(hop.addr());
        let big = vec![5u8; 300_000];
        assert_eq!(echo(&mut stream, &big).unwrap(), big);
        wait_open(&hop, 1);
        stream.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        assert_eq!(stream.read_to_end(&mut rest).unwrap(), 0, "a clean close");
        wait_open(&hop, 0);
    }

    #[test]
    fn a_cut_toward_the_dialler_is_a_reset() {
        let hop = Hop::start(echo_server()).unwrap();
        let mut stream = connect(hop.addr());
        assert_eq!(echo(&mut stream, b"hi").unwrap(), b"hi");
        wait_open(&hop, 1);
        let (id, _) = hop.entries()[0].clone();
        hop.order(id, Close::DOWN);
        wait_open(&hop, 0);
        let error = echo(&mut stream, b"again").unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_reset_from_upstream_is_passed_on_as_a_reset() {
        // An upstream that resets every connection as soon as it is accepted.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                close(stream, true);
            }
        });
        let hop = Hop::start(upstream).unwrap();
        let mut stream = connect(hop.addr());
        let error = echo(&mut stream, b"x").unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_stage_cuts_the_chosen_connection_and_picks_in_order() {
        let server = echo_server();
        let pairs = tempfile::NamedTempFile::new().unwrap();
        let stage = Stage::start(server, server, pairs.path()).unwrap();
        let mut streams: Vec<TcpStream> = (0..3).map(|_| connect(stage.addr())).collect();
        for stream in &mut streams {
            assert_eq!(echo_bytes(stream), b"ok");
        }
        wait_open(&stage.front, 3);
        let ids: Vec<u64> = stage.connections().iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        // Pick 4 of 3 open connections is the second.
        assert_eq!(stage.cut(Select::Pick(4), Side::Client), vec![ids[1]]);
        assert!(echo(&mut streams[1], b"x").is_err());
        for index in [0, 2] {
            assert_eq!(echo_bytes(&mut streams[index]), b"ok");
        }
        assert_eq!(
            stage.cut(Select::Id(ids[1]), Side::Client),
            Vec::<u64>::new()
        );
        assert_eq!(stage.cut(Select::All, Side::Client), vec![ids[0], ids[2]]);
        assert!(stage.connections().is_empty());
        assert!(stage.cut(Select::Pick(0), Side::Both).is_empty());
    }

    fn echo_bytes(stream: &mut TcpStream) -> Vec<u8> {
        echo(stream, b"ok").unwrap()
    }

    #[test]
    fn the_client_random_is_read_from_a_client_hello() {
        let mut record = vec![
            0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc, 0x03, 0x03,
        ];
        record.extend(0u8..32);
        assert_eq!(
            client_random(&record).unwrap(),
            (0u8..32).map(|b| format!("{b:02x}")).collect::<String>()
        );
        assert!(client_random(&record[..42]).is_none());
        record[0] = 0x17;
        assert!(client_random(&record).is_none());
    }

    #[test]
    fn source_addresses_rotate_through_loopback() {
        assert_eq!(loopback(0), Ipv4Addr::LOCALHOST);
        assert_eq!(loopback(1), Ipv4Addr::new(127, 0, 0, 2));
        let source = SourceAddress {
            rotates: true,
            epoch: AtomicU32::new(0),
        };
        assert_eq!(source.current(), Some(Ipv4Addr::LOCALHOST));
        source.epoch.store(254, Ordering::SeqCst);
        assert_eq!(source.current(), Some(Ipv4Addr::LOCALHOST));
        let fixed = SourceAddress {
            rotates: false,
            epoch: AtomicU32::new(3),
        };
        assert_eq!(fixed.current(), None);
        let remote = SourceAddress::detect("192.0.2.1:6241".parse().unwrap());
        assert!(
            !remote.rotates,
            "a loopback source cannot reach another machine"
        );
    }
}
