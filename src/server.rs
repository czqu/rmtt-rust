//! The RMTT server: accept device connections, authenticate them, route
//! downstream pushes and enforce keepalive.
//!
//! Mirrors the semantics of the reference `rmtt-java` / `rmtt-go` servers:
//!
//! - CONNECT is validated (magic, protocol version) and handed to the
//!   registered authenticator, which maps the credential to a device id.
//! - The keepalive policy clamps the client proposal and echoes the final
//!   value in CONNACK; a connection that sends nothing for `1.5x` that
//!   interval is reaped with DISCONNECT(0x05).
//! - A second CONNECT for the same device id takes the session over: the
//!   superseded connection receives DISCONNECT(0x02).
//! - Uplink PUSH payloads are dispatched to the message handler; downstream
//!   pushes go through [`Server::push`].
//!
//! All callbacks run on the connection's IO thread; keep them fast or hand
//! the work off to your own pool.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::codec;
use crate::error::{Error, Result};

/// How long a fresh connection may sit idle before delivering CONNECT.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds how long a single outbound packet write may block the IO thread.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

const READ_POLL: Duration = Duration::from_millis(100);
const ACCEPT_POLL: Duration = Duration::from_millis(20);
const MAX_READ_BUFFER: usize = 16 * 1024 * 1024;

type Authenticator = Box<dyn Fn(&str) -> AuthResult + Send + Sync>;
type MessageHandler = Box<dyn Fn(&str, &[u8]) + Send + Sync>;
type ConnectCallback = Box<dyn Fn(&str) + Send + Sync>;
type DisconnectCallback = Box<dyn Fn(&str, &str) + Send + Sync>;

/// Server-side keepalive negotiation policy. The server owns the final
/// heartbeat interval: it clamps the client's CONNECT proposal into
/// `[min_seconds, max_seconds]`, falls back to `default_seconds` when the
/// proposal is 0, and echoes the result in CONNACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepalivePolicy {
    /// Lower bound for the final interval.
    pub min_seconds: u16,
    /// Upper bound for the final interval.
    pub max_seconds: u16,
    /// Interval used when the client does not propose one (proposal 0).
    pub default_seconds: u16,
    /// Whether a client proposal of 0 may disable keepalive enforcement
    /// entirely (CONNACK then carries `server_keepalive = 0`).
    pub allow_disable: bool,
}

impl Default for KeepalivePolicy {
    fn default() -> Self {
        KeepalivePolicy {
            min_seconds: 30,
            max_seconds: 600,
            default_seconds: 60,
            allow_disable: false,
        }
    }
}

impl KeepalivePolicy {
    /// Map the client's keepalive proposal (seconds) to the server-side
    /// value echoed in CONNACK.
    pub fn decide(&self, client_keepalive: u16) -> u16 {
        if client_keepalive == 0 {
            if self.allow_disable {
                0
            } else {
                self.default_seconds
            }
        } else {
            client_keepalive.clamp(self.min_seconds, self.max_seconds)
        }
    }
}

/// Result of the authenticator callback: accept the credential bound to a
/// device id, or reject it with a CONNACK return code.
#[derive(Debug, Clone)]
pub struct AuthResult {
    allowed: bool,
    device_id: String,
    return_code: u8,
}

impl AuthResult {
    /// Accept the credential and bind it to `device_id`.
    pub fn allow(device_id: impl Into<String>) -> AuthResult {
        AuthResult {
            allowed: true,
            device_id: device_id.into(),
            return_code: codec::returncode::ACCEPTED,
        }
    }

    /// Reject the credential with [`codec::returncode::NOT_AUTHORISED`].
    pub fn reject() -> AuthResult {
        AuthResult::reject_with(codec::returncode::NOT_AUTHORISED)
    }

    /// Reject the credential with an explicit CONNACK return code
    /// (e.g. [`codec::returncode::SERVER_UNAVAILABLE`]).
    pub fn reject_with(return_code: u8) -> AuthResult {
        AuthResult {
            allowed: false,
            device_id: String::new(),
            return_code,
        }
    }

    /// Whether the CONNECT was accepted.
    pub fn allowed(&self) -> bool {
        self.allowed
    }

    /// The device id bound to the accepted credential (empty when rejected).
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The CONNACK return code sent back to the client.
    pub fn return_code(&self) -> u8 {
        self.return_code
    }
}

/// Server configuration. Passed to [`Server::bind`].
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Keepalive negotiation policy.
    pub keepalive: KeepalivePolicy,
    /// Deadline for a fresh connection to deliver its CONNECT packet.
    pub connect_timeout: Duration,
    /// Deadline for flushing a single outbound packet to a device.
    pub write_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions {
            keepalive: KeepalivePolicy::default(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
        }
    }
}

/// Work items sent from the [`Server`] handle to a connection's IO thread.
enum Command {
    /// Encode and write a PUSH frame.
    Push(Vec<u8>),
    /// Write DISCONNECT with the given return code, then end the connection.
    SendDisconnect(u8),
}

/// Shared per-connection state: the command channel and the liveness flag.
struct ConnHandle {
    device_id: String,
    tx: mpsc::Sender<Command>,
    active: AtomicBool,
}

impl ConnHandle {
    fn new(device_id: String) -> (ConnHandle, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        (
            ConnHandle {
                device_id,
                tx,
                active: AtomicBool::new(true),
            },
            rx,
        )
    }
}

/// Active device connections keyed by device id. A second registration for
/// the same id takes the session over and yields the previous connection.
struct ConnectionStore {
    map: Mutex<HashMap<String, Arc<ConnHandle>>>,
}

impl ConnectionStore {
    fn new() -> ConnectionStore {
        ConnectionStore {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, device_id: String, conn: Arc<ConnHandle>) -> Option<Arc<ConnHandle>> {
        self.map.lock().unwrap().insert(device_id, conn)
    }

    fn get(&self, device_id: &str) -> Option<Arc<ConnHandle>> {
        self.map.lock().unwrap().get(device_id).cloned()
    }

    /// Remove the mapping if and only if `conn` is still the registered one
    /// (a takeover may already have replaced it).
    fn remove_if_same(&self, device_id: &str, conn: &Arc<ConnHandle>) -> bool {
        let mut map = self.map.lock().unwrap();
        match map.get(device_id) {
            Some(existing) if Arc::ptr_eq(existing, conn) => {
                map.remove(device_id);
                true
            }
            _ => false,
        }
    }

    fn all(&self) -> Vec<Arc<ConnHandle>> {
        self.map.lock().unwrap().values().cloned().collect()
    }

    fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

/// State shared between the accept loop, the connection threads and the
/// [`Server`] handle.
struct Shared {
    opts: ServerOptions,
    authenticator: Mutex<Option<Authenticator>>,
    message_handler: Mutex<Option<MessageHandler>>,
    on_connect: Mutex<Option<ConnectCallback>>,
    on_disconnect: Mutex<Option<DisconnectCallback>>,
    store: ConnectionStore,
    stop: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Shared {
    fn new(opts: ServerOptions) -> Shared {
        Shared {
            opts,
            authenticator: Mutex::new(None),
            message_handler: Mutex::new(None),
            on_connect: Mutex::new(None),
            on_disconnect: Mutex::new(None),
            store: ConnectionStore::new(),
            stop: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        }
    }

    fn track_thread(&self, handle: JoinHandle<()>) {
        let mut threads = self.threads.lock().unwrap();
        threads.retain(|h| !h.is_finished());
        threads.push(handle);
    }
}

/// An RMTT server. Created by [`Server::bind`]; an acceptor thread plus one
/// IO thread per connection drive authentication, heartbeats and routing.
pub struct Server {
    shared: Arc<Shared>,
    accept_thread: Mutex<Option<JoinHandle<()>>>,
    local: SocketAddr,
}

impl Server {
    /// Bind `addr` (e.g. `127.0.0.1:18883`; use port `0` for an ephemeral
    /// port, see [`Server::local_addr`]) and start accepting connections.
    ///
    /// The default authenticator accepts every credential and uses it as the
    /// device id; install a stricter one with [`Server::set_authenticator`].
    pub fn bind(addr: &str, opts: ServerOptions) -> Result<Server> {
        let listener = TcpListener::bind(addr).map_err(Error::Io)?;
        let local = listener.local_addr().map_err(Error::Io)?;
        listener.set_nonblocking(true).map_err(Error::Io)?;

        let shared = Arc::new(Shared::new(opts));
        let s2 = shared.clone();
        let accept_thread = std::thread::Builder::new()
            .name("rmtt-accept".into())
            .spawn(move || accept_loop(listener, s2))
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;

        Ok(Server {
            shared,
            accept_thread: Mutex::new(Some(accept_thread)),
            local,
        })
    }

    /// The bound socket address (useful with port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Install the CONNECT authenticator mapping a credential to a device id
    /// or a rejection. Replaces any previously installed authenticator.
    pub fn set_authenticator<F>(&self, f: F)
    where
        F: Fn(&str) -> AuthResult + Send + Sync + 'static,
    {
        *self.shared.authenticator.lock().unwrap() = Some(Box::new(f));
    }

    /// Install the callback invoked for every uplink PUSH payload, with the
    /// sending device id. Replaces any previously installed handler.
    pub fn set_message_handler<F>(&self, f: F)
    where
        F: Fn(&str, &[u8]) + Send + Sync + 'static,
    {
        *self.shared.message_handler.lock().unwrap() = Some(Box::new(f));
    }

    /// Install the callback invoked when a device completes its handshake.
    pub fn set_on_connect<F>(&self, f: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        *self.shared.on_connect.lock().unwrap() = Some(Box::new(f));
    }

    /// Install the callback invoked when a device connection ends, with a
    /// short reason (e.g. `"connection closed"`, `"keepalive timeout"`,
    /// `"session taken over"`, `"kicked with reason 0x06"`).
    pub fn set_on_disconnect<F>(&self, f: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        *self.shared.on_disconnect.lock().unwrap() = Some(Box::new(f));
    }

    /// Send a downstream PUSH to a connected device. Returns
    /// [`Error::NotConnected`] when the device is offline.
    pub fn push(&self, device_id: &str, payload: &[u8]) -> Result<()> {
        let conn = self
            .shared
            .store
            .get(device_id)
            .filter(|c| c.active.load(Ordering::SeqCst))
            .ok_or(Error::NotConnected)?;
        conn.tx
            .send(Command::Push(payload.to_vec()))
            .map_err(|_| Error::NotConnected)
    }

    /// Kick a device offline, sending it DISCONNECT with the given return
    /// code (see [`codec::disconnect`]). Returns [`Error::NotConnected`]
    /// when the device is offline.
    pub fn kick(&self, device_id: &str, reason: u8) -> Result<()> {
        let conn = self
            .shared
            .store
            .get(device_id)
            .filter(|c| c.active.load(Ordering::SeqCst))
            .ok_or(Error::NotConnected)?;
        conn.tx
            .send(Command::SendDisconnect(reason))
            .map_err(|_| Error::NotConnected)
    }

    /// Whether the device currently has an active connection.
    pub fn is_online(&self, device_id: &str) -> bool {
        self.shared
            .store
            .get(device_id)
            .map(|c| c.active.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Number of currently connected devices.
    pub fn online_count(&self) -> usize {
        self.shared.store.len()
    }

    /// Stop accepting, send DISCONNECT(0x03) to every device and wait for
    /// all connection threads to finish. Idempotent.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);

        // Joining the acceptor first guarantees every spawned connection
        // thread is tracked before we join them below.
        if let Some(handle) = self.accept_thread.lock().unwrap().take() {
            let _ = handle.join();
        }

        for conn in self.shared.store.all() {
            let _ = conn
                .tx
                .send(Command::SendDisconnect(codec::disconnect::SERVER_SHUTDOWN));
        }

        let handles = std::mem::take(&mut *self.shared.threads.lock().unwrap());
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Acceptor
// ---------------------------------------------------------------------------

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let s2 = shared.clone();
                let spawned = std::thread::Builder::new()
                    .name("rmtt-conn".into())
                    .spawn(move || connection_main(stream, s2));
                if let Ok(handle) = spawned {
                    shared.track_thread(handle);
                }
            }
            // Non-blocking listener: no connection pending right now.
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection IO thread
// ---------------------------------------------------------------------------

fn connection_main(mut stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_POLL));
    let _ = stream.set_write_timeout(Some(shared.opts.write_timeout));

    // CONNECT handshake.
    let connect = match read_connect(&mut stream, &shared) {
        Ok(Some(connect)) => connect,
        Ok(None) => return, // deadline or shutdown: close silently
        Err(_) => {
            let _ = stream.write_all(&codec::encode_disconnect(
                codec::disconnect::PROTOCOL_VIOLATION,
            ));
            return;
        }
    };

    // Bad magic -> close without any RMTT message.
    if connect.magic != codec::MAGIC_NUMBER {
        return;
    }
    // Bad protocol version -> CONNACK(0x01) then close.
    if connect.protocol_version != 1 {
        let _ = stream.write_all(&codec::encode_connack(
            codec::returncode::BAD_PROTOCOL_VERSION,
            0,
        ));
        return;
    }

    let auth = match shared.authenticator.lock().unwrap().as_ref() {
        Some(authenticator) => authenticator(&connect.credential),
        None => AuthResult::allow(connect.credential.clone()),
    };
    if !auth.allowed() {
        let _ = stream.write_all(&codec::encode_connack(auth.return_code(), 0));
        return;
    }
    let device_id = auth.device_id().to_string();
    if device_id.is_empty() {
        let _ = stream.write_all(&codec::encode_connack(codec::returncode::NOT_AUTHORISED, 0));
        return;
    }

    let server_kp = shared.opts.keepalive.decide(connect.keepalive);
    let (handle, rx) = ConnHandle::new(device_id.clone());
    let handle = Arc::new(handle);

    // Register before CONNACK so downstream pushes can route as soon as the
    // client's connect() returns.
    let prev = shared.store.register(device_id.clone(), handle.clone());
    if let Some(prev) = prev {
        // Tear the superseded session down on its own IO thread so a stuck
        // old client cannot delay the connection that won the takeover.
        let _ = prev.tx.send(Command::SendDisconnect(
            codec::disconnect::SESSION_TAKEN_OVER,
        ));
    }

    if stream
        .write_all(&codec::encode_connack(
            codec::returncode::ACCEPTED,
            server_kp,
        ))
        .is_err()
    {
        handle.active.store(false, Ordering::SeqCst);
        shared.store.remove_if_same(&device_id, &handle);
        return;
    }

    if let Some(cb) = shared.on_connect.lock().unwrap().as_ref() {
        cb(&device_id);
    }

    let reason = connected_loop(&mut stream, &shared, &handle, &rx, server_kp);

    handle.active.store(false, Ordering::SeqCst);
    shared.store.remove_if_same(&device_id, &handle);
    if let Some(cb) = shared.on_disconnect.lock().unwrap().as_ref() {
        cb(&device_id, &reason);
    }
}

/// Read exactly one CONNECT packet, honouring `connect_timeout` and the
/// shutdown flag. `Ok(None)` = give up silently.
fn read_connect(stream: &mut TcpStream, shared: &Shared) -> Result<Option<codec::Connect>> {
    let deadline = Instant::now() + shared.opts.connect_timeout;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match codec::decode_connect(&buf) {
            Ok(Some((connect, _))) => return Ok(Some(connect)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                match stream.read(&mut tmp) {
                    Ok(0) => {
                        return Err(Error::Io(std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "connection closed during CONNECT",
                        )))
                    }
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(e)
                        if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock =>
                    {
                        continue
                    }
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// The connected state loop: drain commands, enforce keepalive, dispatch
/// packets. Returns the disconnect reason reported to the lifecycle
/// callback.
fn connected_loop(
    stream: &mut TcpStream,
    shared: &Shared,
    handle: &ConnHandle,
    rx: &mpsc::Receiver<Command>,
    server_kp: u16,
) -> String {
    let kp_enabled = server_kp > 0;
    let response_timeout = Duration::from_secs_f64(server_kp as f64 * 1.5);

    let mut rbuf: Vec<u8> = Vec::new();
    let mut last_received = Instant::now();

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            let _ = stream.write_all(&codec::encode_disconnect(
                codec::disconnect::SERVER_SHUTDOWN,
            ));
            return "server shutdown".to_string();
        }

        // Drain commands from the Server handle.
        loop {
            match rx.try_recv() {
                Ok(Command::Push(payload)) => {
                    if stream.write_all(&codec::encode_push(&payload)).is_err() {
                        return "connection closed".to_string();
                    }
                }
                Ok(Command::SendDisconnect(rc)) => {
                    let _ = stream.write_all(&codec::encode_disconnect(rc));
                    return command_reason(rc);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return "connection closed".to_string(),
            }
        }

        // Keepalive enforcement: any inbound packet refreshes the deadline.
        if kp_enabled && Instant::now().duration_since(last_received) >= response_timeout {
            let _ = stream.write_all(&codec::encode_disconnect(
                codec::disconnect::KEEPALIVE_TIMEOUT,
            ));
            return "keepalive timeout".to_string();
        }

        // Read whatever arrived (100ms read poll).
        match read_available(stream, &mut rbuf) {
            ReadOutcome::Data(n) => {
                if n > 0 {
                    last_received = Instant::now();
                }
            }
            ReadOutcome::Closed | ReadOutcome::Failed => return "connection closed".to_string(),
        }

        // Decode and dispatch complete frames.
        let mut consumed = 0;
        loop {
            match codec::decode(&rbuf[consumed..]) {
                Ok(Some((packet, n))) => {
                    consumed += n;
                    match packet {
                        codec::Packet::Push(payload) => {
                            if let Some(cb) = shared.message_handler.lock().unwrap().as_ref() {
                                cb(&handle.device_id, &payload);
                            }
                        }
                        codec::Packet::Pingreq => {
                            if stream.write_all(&codec::encode_pingresp()).is_err() {
                                return "connection closed".to_string();
                            }
                        }
                        codec::Packet::Disconnect(_) => return "connection closed".to_string(),
                        // CONNACK/PINGRESP from a client (or any future
                        // type) is a protocol violation.
                        _ => {
                            let _ = stream.write_all(&codec::encode_disconnect(
                                codec::disconnect::PROTOCOL_VIOLATION,
                            ));
                            return "protocol violation".to_string();
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    // Protocol violation: answer with DISCONNECT(0x04).
                    let _ = stream.write_all(&codec::encode_disconnect(
                        codec::disconnect::PROTOCOL_VIOLATION,
                    ));
                    return "protocol violation".to_string();
                }
            }
        }
        rbuf.drain(..consumed);
        if rbuf.len() > MAX_READ_BUFFER {
            return "connection closed".to_string();
        }
    }
}

/// Reason string for a connection ended by an outbound DISCONNECT command.
fn command_reason(rc: u8) -> String {
    match rc {
        codec::disconnect::SESSION_TAKEN_OVER => "session taken over".to_string(),
        codec::disconnect::SERVER_SHUTDOWN => "server shutdown".to_string(),
        other => format!("kicked with reason 0x{other:02x}"),
    }
}

enum ReadOutcome {
    Data(usize),
    Closed,
    Failed,
}

fn read_available(stream: &mut TcpStream, buf: &mut Vec<u8>) -> ReadOutcome {
    let mut tmp = [0u8; 4096];
    let mut total = 0usize;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return ReadOutcome::Closed,
            Ok(n) => {
                total += n;
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_READ_BUFFER {
                    return ReadOutcome::Closed;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return ReadOutcome::Failed,
        }
    }
    ReadOutcome::Data(total)
}

// The interop tests drive the real `rmtt::client`, hence the `client` gate.
#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;
    use crate::{Client, ClientOptions};

    fn test_server(opts: ServerOptions) -> Arc<Server> {
        Arc::new(Server::bind("127.0.0.1:0", opts).unwrap())
    }

    fn client_opts(credential: &str) -> ClientOptions {
        ClientOptions {
            credential: credential.into(),
            heartbeat_seconds: 30,
            auto_reconnect: false,
            ..Default::default()
        }
    }

    fn wait_until<F: Fn() -> bool>(cond: F) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not met within 5s");
    }

    /// Open a raw socket and send CONNECT, bypassing the rmtt client.
    fn raw_connect(server: &Server, version: u8, keepalive: u16, credential: &str) -> TcpStream {
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let _ = stream.write_all(&codec::encode_connect(version, keepalive, credential));
        stream
    }

    /// Read one complete packet from a raw socket (5s deadline).
    fn read_packet(stream: &mut TcpStream) -> Option<codec::Packet> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            if let Ok(Some((packet, _))) = codec::decode(&buf) {
                return Some(packet);
            }
            if Instant::now() >= deadline {
                return None;
            }
            match stream.read(&mut tmp) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn keepalive_policy_decide() {
        let p = KeepalivePolicy::default();
        assert_eq!(p.decide(0), 60);
        assert_eq!(p.decide(5), 30);
        assert_eq!(p.decide(30), 30);
        assert_eq!(p.decide(300), 300);
        assert_eq!(p.decide(3600), 600);

        let p = KeepalivePolicy {
            allow_disable: true,
            ..KeepalivePolicy::default()
        };
        assert_eq!(p.decide(0), 0);
    }

    #[test]
    fn auth_result_shapes() {
        let a = AuthResult::allow("dev-1");
        assert!(a.allowed());
        assert_eq!(a.device_id(), "dev-1");
        assert_eq!(a.return_code(), codec::returncode::ACCEPTED);

        let r = AuthResult::reject();
        assert!(!r.allowed());
        assert_eq!(r.return_code(), codec::returncode::NOT_AUTHORISED);

        let r = AuthResult::reject_with(codec::returncode::SERVER_UNAVAILABLE);
        assert_eq!(r.return_code(), codec::returncode::SERVER_UNAVAILABLE);
    }

    #[test]
    fn echo_roundtrip_and_clean_disconnect() {
        let server = test_server(ServerOptions::default());

        let s2 = server.clone();
        server.set_message_handler(move |device_id, payload| {
            let echoed = format!("echo:{}", String::from_utf8_lossy(payload));
            let _ = s2.push(device_id, echoed.as_bytes());
        });
        let (ev_tx, ev_rx) = mpsc::channel();
        server.set_on_disconnect(move |id, reason| {
            let _ = ev_tx.send((id.to_string(), reason.to_string()));
        });

        let addr = server.local_addr().to_string();
        let client = Client::connect(&addr, client_opts("dev-1")).unwrap();
        assert_eq!(client.server_keepalive(), 30);
        assert!(server.is_online("dev-1"));
        assert_eq!(server.online_count(), 1);

        let (got_tx, got_rx) = mpsc::channel();
        client.set_payload_handler(move |msg| {
            let _ = got_tx.send(msg.to_utf8_lossy().to_string());
        });
        client.push(b"hello").unwrap();
        assert_eq!(
            got_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "echo:hello"
        );

        client.disconnect();
        let (id, reason) = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(id, "dev-1");
        assert_eq!(reason, "connection closed");
        wait_until(|| !server.is_online("dev-1"));
        server.shutdown();
    }

    #[test]
    fn default_authenticator_maps_credential_to_device_id() {
        let server = test_server(ServerOptions::default());
        let addr = server.local_addr().to_string();
        let client = Client::connect(&addr, client_opts("dev-9")).unwrap();
        assert!(server.is_online("dev-9"));
        client.disconnect();
        server.shutdown();
    }

    #[test]
    fn server_push_and_offline_push() {
        let server = test_server(ServerOptions::default());
        let addr = server.local_addr().to_string();
        let client = Client::connect(&addr, client_opts("dev-1")).unwrap();

        let (got_tx, got_rx) = mpsc::channel();
        client.set_payload_handler(move |msg| {
            let _ = got_tx.send(msg.to_utf8_lossy().to_string());
        });
        server.push("dev-1", b"from-server").unwrap();
        assert_eq!(
            got_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "from-server"
        );

        assert!(matches!(
            server.push("ghost", b"x"),
            Err(Error::NotConnected)
        ));
        client.disconnect();
        server.shutdown();
    }

    #[test]
    fn authenticator_can_reject() {
        let server = test_server(ServerOptions::default());
        server.set_authenticator(|_| AuthResult::reject());

        let addr = server.local_addr().to_string();
        match Client::connect(&addr, client_opts("dev-1")) {
            Err(Error::ConnRefused(rc)) => assert_eq!(rc, codec::returncode::NOT_AUTHORISED),
            Err(other) => panic!("expected ConnRefused, got {other:?}"),
            Ok(_) => panic!("expected ConnRefused, got Ok"),
        }
        assert_eq!(server.online_count(), 0);
        server.shutdown();
    }

    #[test]
    fn kick_sends_disconnect_with_reason() {
        let server = test_server(ServerOptions::default());
        let (ev_tx, ev_rx) = mpsc::channel();
        server.set_on_disconnect(move |id, reason| {
            let _ = ev_tx.send((id.to_string(), reason.to_string()));
        });

        let addr = server.local_addr().to_string();
        let client = Client::connect(&addr, client_opts("dev-1")).unwrap();
        server
            .kick("dev-1", codec::disconnect::KICKED_BY_ADMIN)
            .unwrap();

        let (id, reason) = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(id, "dev-1");
        assert_eq!(reason, "kicked with reason 0x06");
        wait_until(|| !client.is_connected());
        assert!(matches!(
            server.kick("dev-1", codec::disconnect::KICKED_BY_ADMIN),
            Err(Error::NotConnected)
        ));
        server.shutdown();
    }

    #[test]
    fn session_takeover_kicks_previous_connection() {
        let server = test_server(ServerOptions::default());
        let (ev_tx, ev_rx) = mpsc::channel();
        server.set_on_disconnect(move |id, reason| {
            let _ = ev_tx.send((id.to_string(), reason.to_string()));
        });

        let addr = server.local_addr().to_string();
        let client1 = Client::connect(&addr, client_opts("dev-1")).unwrap();
        let client2 = Client::connect(&addr, client_opts("dev-1")).unwrap();

        let (id, reason) = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(id, "dev-1");
        assert_eq!(reason, "session taken over");
        wait_until(|| !client1.is_connected());
        assert!(client2.is_connected());
        assert_eq!(server.online_count(), 1);

        client2.disconnect();
        server.shutdown();
    }

    #[test]
    fn keepalive_timeout_reaps_silent_connection() {
        let opts = ServerOptions {
            keepalive: KeepalivePolicy {
                min_seconds: 1,
                max_seconds: 1,
                default_seconds: 1,
                allow_disable: false,
            },
            ..Default::default()
        };
        let server = test_server(opts);
        let (ev_tx, ev_rx) = mpsc::channel();
        server.set_on_disconnect(move |id, reason| {
            let _ = ev_tx.send((id.to_string(), reason.to_string()));
        });

        let mut stream = raw_connect(&server, 1, 30, "dev-1");
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Connack {
                return_code: codec::returncode::ACCEPTED,
                server_keepalive: 1
            })
        );

        // Stay silent: the server must reap us with DISCONNECT(0x05).
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Disconnect(
                codec::disconnect::KEEPALIVE_TIMEOUT
            ))
        );
        let (id, reason) = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(id, "dev-1");
        assert_eq!(reason, "keepalive timeout");
        server.shutdown();
    }

    #[test]
    fn bad_magic_is_closed_without_reply() {
        let server = test_server(ServerOptions::default());

        // CONNECT with the magic overwritten with zeroes.
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut pkt = codec::encode_connect(1, 30, "x");
        pkt[2..6].copy_from_slice(&0u32.to_be_bytes());
        let _ = stream.write_all(&pkt);

        assert_eq!(read_packet(&mut stream), None);
        assert_eq!(server.online_count(), 0);
        server.shutdown();
    }

    #[test]
    fn bad_protocol_version_gets_connack_0x01() {
        let server = test_server(ServerOptions::default());
        let mut stream = raw_connect(&server, 2, 30, "dev-1");
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Connack {
                return_code: codec::returncode::BAD_PROTOCOL_VERSION,
                server_keepalive: 0
            })
        );
        assert_eq!(server.online_count(), 0);
        server.shutdown();
    }

    #[test]
    fn midstream_garbage_gets_disconnect_0x04() {
        let server = test_server(ServerOptions::default());
        let mut stream = raw_connect(&server, 1, 30, "dev-1");
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Connack {
                return_code: codec::returncode::ACCEPTED,
                server_keepalive: 30
            })
        );

        // PUSH with non-zero fixed header flags = wire violation.
        let _ = stream.write_all(&[0x31, 0x00]);
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Disconnect(
                codec::disconnect::PROTOCOL_VIOLATION
            ))
        );
        server.shutdown();
    }

    #[test]
    fn oversized_frame_header_gets_disconnect_0x04() {
        // A fixed header declaring more than the max frame size must be
        // answered with DISCONNECT(0x04) immediately, without waiting for a
        // body that will never arrive.
        let server = test_server(ServerOptions::default());
        let mut stream = raw_connect(&server, 1, 30, "dev-1");
        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Connack {
                return_code: codec::returncode::ACCEPTED,
                server_keepalive: 30
            })
        );

        // PUSH header claiming remaining length = 16 MiB + 1, body never sent.
        let _ = stream.write_all(&[0x30, 0x81, 0x80, 0x80, 0x08]);

        assert_eq!(
            read_packet(&mut stream),
            Some(codec::Packet::Disconnect(
                codec::disconnect::PROTOCOL_VIOLATION
            ))
        );
        server.shutdown();
    }

    #[test]
    fn on_connect_fires_with_device_id() {
        let server = test_server(ServerOptions::default());
        let (ev_tx, ev_rx) = mpsc::channel();
        server.set_on_connect(move |id| {
            let _ = ev_tx.send(id.to_string());
        });

        let addr = server.local_addr().to_string();
        let client = Client::connect(&addr, client_opts("dev-7")).unwrap();
        assert_eq!(ev_rx.recv_timeout(Duration::from_secs(5)).unwrap(), "dev-7");
        client.disconnect();
        server.shutdown();
    }
}
