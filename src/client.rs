//! The RMTT client: establish, heartbeat, receive pushes, reconnect.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::codec;
use crate::error::{Error, Result};
use crate::message::Message;
use crate::options::ClientOptions;

pub(crate) enum Command {
    Push(Vec<u8>),
    Shutdown,
}

enum PhaseEnd {
    Shutdown,
    Lost,
}

/// Any byte stream the client can talk RMTT over (TCP, TLS, ...).
pub(crate) trait Io: Read + Write + Send {}
impl<T: Read + Write + Send> Io for T {}

type BoxIo = Box<dyn Io>;

const ST_DISCONNECTED: u8 = 0;
const ST_CONNECTED: u8 = 2;
const ST_RECONNECTING: u8 = 3;

const READ_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_READ_BUFFER: usize = 16 * 1024 * 1024;

/// An RMTT client. Created by [`Client::connect`]; a background IO thread
/// drives reading, heartbeats and reconnects.
pub struct Client {
    tx: mpsc::Sender<Command>,
    _thread: Option<std::thread::JoinHandle<()>>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    server_kp: u16,
    handler: Arc<Mutex<Option<Box<dyn Fn(Message) + Send + Sync>>>>,
}

impl Client {
    /// Establish a connection to `addr` (`127.0.0.1:18883`, `tcp://host:port`
    /// or `tls://host:port`) and complete the CONNECT/CONNACK handshake.
    /// The returned client exposes [`Client::server_keepalive`] with the
    /// server-negotiated value in seconds.
    pub fn connect(addr: &str, opts: ClientOptions) -> Result<Client> {
        let mut stream = dial(addr, &opts)?;
        let kp = handshake(&mut stream, &opts)?;
        spawn_io(addr, opts, stream, kp)
    }

    /// True while the IO thread considers the link up (including during an
    /// automatic reconnect attempt).
    pub fn is_connected(&self) -> bool {
        let s = self.state.load(Ordering::SeqCst);
        s == ST_CONNECTED || s == ST_RECONNECTING
    }

    /// The server-negotiated keepalive in seconds (0 = disabled by server).
    pub fn server_keepalive(&self) -> u16 {
        self.server_kp
    }

    /// Register the callback invoked on every inbound PUSH payload.
    pub fn set_payload_handler<F>(&self, f: F)
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        *self.handler.lock().unwrap() = Some(Box::new(f));
    }

    /// Send an application message. Returns [`Error::NotConnected`] when the
    /// link is down.
    pub fn push(&self, payload: &[u8]) -> Result<()> {
        if !self.is_connected() {
            return Err(Error::NotConnected);
        }
        self.tx
            .send(Command::Push(payload.to_vec()))
            .map_err(|_| Error::Closed)
    }

    /// Gracefully send DISCONNECT(0x00) and close the underlying connection.
    pub fn disconnect(&self) {
        let _ = self.tx.send(Command::Shutdown);
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn spawn_io(addr: &str, opts: ClientOptions, stream: BoxIo, kp: u16) -> Result<Client> {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(AtomicU8::new(ST_CONNECTED));
    let stop = Arc::new(AtomicBool::new(false));
    let handler: Arc<Mutex<Option<Box<dyn Fn(Message) + Send + Sync>>>> =
        Arc::new(Mutex::new(None));

    let (tstate, tstop) = (state.clone(), stop.clone());
    let handler2 = handler.clone();
    let addr2 = addr.to_string();
    let handle = std::thread::Builder::new()
        .name("rmtt-io".into())
        .spawn(move || io_loop(addr2, opts, stream, rx, tstate, tstop, handler2, kp))
        .map_err(|e| Error::Io(std::io::Error::new(ErrorKind::Other, e)))?;

    Ok(Client { tx, _thread: Some(handle), state, stop, server_kp: kp, handler })
}

/// Owns the connection for its whole lifetime: reads packets, drives the
/// heartbeat, handles user commands and reconnects on loss.
fn io_loop(
    addr: String,
    opts: ClientOptions,
    mut stream: BoxIo,
    rx: mpsc::Receiver<Command>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    handler: Arc<Mutex<Option<Box<dyn Fn(Message) + Send + Sync>>>>,
    kp: u16,
) {
    let mut backoff = opts.reconnect_base;
    let mut first = true;
    let mut server_kp = kp;

    loop {
        if !first {
            // (re)connect attempt after a loss.
            match dial(&addr, &opts) {
                Ok(mut s) => match handshake_safe(&mut s, &opts) {
                    Ok(k) => {
                        stream = s;
                        server_kp = k;
                    }
                    Err(_) => {
                        if !opts.auto_reconnect {
                            state.store(ST_DISCONNECTED, Ordering::SeqCst);
                            break;
                        }
                        backoff = backoff_next(backoff, &opts);
                        sleep_jittered(backoff, &opts);
                        continue;
                    }
                },
                Err(_) => {
                    if !opts.auto_reconnect {
                        state.store(ST_DISCONNECTED, Ordering::SeqCst);
                        break;
                    }
                    backoff = backoff_next(backoff, &opts);
                    sleep_jittered(backoff, &opts);
                    continue;
                }
            }
        }
        first = false;

        state.store(ST_CONNECTED, Ordering::SeqCst);
        match connected_phase(&mut stream, &opts, &rx, &stop, &handler, server_kp) {
            PhaseEnd::Shutdown => {
                state.store(ST_DISCONNECTED, Ordering::SeqCst);
                break;
            }
            PhaseEnd::Lost => {
                if !opts.auto_reconnect {
                    state.store(ST_DISCONNECTED, Ordering::SeqCst);
                    break;
                }
                state.store(ST_RECONNECTING, Ordering::SeqCst);
                backoff = backoff_next(backoff, &opts);
                sleep_jittered(backoff, &opts);
            }
        }
    }
}

/// Reconnect helper: dial + CONNECT/CONNACK handshake.
fn handshake_safe(stream: &mut BoxIo, opts: &ClientOptions) -> Result<u16> {
    handshake(stream, opts)
}

/// The connected state loop: drain commands, run heartbeats, read and dispatch
/// packets. Returns when the connection ends.
fn connected_phase(
    stream: &mut BoxIo,
    opts: &ClientOptions,
    rx: &mpsc::Receiver<Command>,
    stop: &AtomicBool,
    handler: &Mutex<Option<Box<dyn Fn(Message) + Send + Sync>>>,
    server_kp: u16,
) -> PhaseEnd {
    // Heartbeat = server_keepalive when nonzero, otherwise the client
    // proposal. server_kp==0 disables PING and timeout judgements.
    let hb = if server_kp > 0 { server_kp } else { opts.heartbeat_seconds };
    let hb_disabled = hb == 0;
    let send_interval = Duration::from_secs(hb as u64);
    let response_timeout = Duration::from_secs_f64((hb as f64) * 1.5);

    let mut rbuf: Vec<u8> = Vec::new();
    let mut last_sent = Instant::now();
    let mut last_received = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = write_all_quiet(stream, &codec::encode_disconnect(codec::disconnect::NORMAL));
            return PhaseEnd::Shutdown;
        }

        // Drain user commands.
        loop {
            match rx.try_recv() {
                Ok(Command::Push(payload)) => {
                    if write_all_quiet(stream, &codec::encode_push(&payload)).is_err() {
                        return PhaseEnd::Lost;
                    }
                    last_sent = Instant::now();
                }
                Ok(Command::Shutdown) => {
                    let _ = write_all_quiet(stream, &codec::encode_disconnect(codec::disconnect::NORMAL));
                    return PhaseEnd::Shutdown;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return PhaseEnd::Shutdown,
            }
        }

        // Heartbeat timing.
        let now = Instant::now();
        if !hb_disabled {
            if now.duration_since(last_received) >= response_timeout {
                return PhaseEnd::Lost;
            }
            if now.duration_since(last_sent) >= send_interval {
                if write_all_quiet(stream, &codec::encode_pingreq()).is_err() {
                    return PhaseEnd::Lost;
                }
                last_sent = Instant::now();
            }
        }

        // Read whatever arrived (100ms read timeout).
        match read_available(stream, &mut rbuf) {
            ReadResult::Ok => {
                if !rbuf.is_empty() {
                    last_received = Instant::now();
                }
            }
            ReadResult::Closed => return PhaseEnd::Lost,
            ReadResult::Err => return PhaseEnd::Lost,
        }

        // Decode and dispatch complete frames.
        let mut consumed = 0;
        loop {
            match codec::decode(&rbuf[consumed..]) {
                Ok(Some((packet, n))) => {
                    consumed += n;
                    match packet {
                        codec::Packet::Push(payload) => {
                            if let Some(cb) = handler.lock().unwrap().as_ref() {
                                cb(Message::new(payload));
                            }
                        }
                        codec::Packet::Disconnect(rc) => {
                            let _ = rc;
                            return PhaseEnd::Shutdown;
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    // Protocol violation: answer with DISCONNECT(0x04).
                    let _ = write_all_quiet(stream, &codec::encode_disconnect(codec::disconnect::PROTOCOL_VIOLATION));
                    return PhaseEnd::Lost;
                }
            }
        }
        rbuf.drain(..consumed);
    }
}

enum ReadResult {
    Ok,
    Closed,
    Err,
}

fn read_available(stream: &mut BoxIo, buf: &mut Vec<u8>) -> ReadResult {
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return ReadResult::Closed,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_READ_BUFFER {
                    return ReadResult::Closed;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return ReadResult::Err,
        }
    }
    ReadResult::Ok
}

fn write_all_quiet(stream: &mut BoxIo, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(bytes)
}

// ---------------------------------------------------------------------------
// Transport establishment
// ---------------------------------------------------------------------------

fn split_hostport(addr: &str) -> Result<(String, u16)> {
    let addr = addr.trim_start_matches('/');
    if let Some((h, p)) = addr.rsplit_once(':') {
        let port: u16 = p
            .parse()
            .map_err(|_| Error::BadAddress(format!("bad port in '{addr}'")))?;
        Ok((h.to_string(), port))
    } else {
        Err(Error::BadAddress(format!("missing port in '{addr}'")))
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Io(e))?;
    addrs
        .next()
        .ok_or_else(|| Error::BadAddress(format!("cannot resolve {host}:{port}")))
}

fn dial(addr: &str, opts: &ClientOptions) -> Result<BoxIo> {
    let (scheme, rest) = match addr.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("tcp".to_string(), addr),
    };
    let (host, port) = split_hostport(rest)?;
    match scheme.as_str() {
        "tcp" => dial_tcp(&host, port, opts),
        "tls" => dial_tls(&host, port, opts),
        other => Err(Error::BadAddress(format!("unsupported scheme '{other}'"))),
    }
}

fn dial_tcp(host: &str, port: u16, opts: &ClientOptions) -> Result<BoxIo> {
    let sa = resolve(host, port)?;
    let stream = TcpStream::connect_timeout(&sa, opts.connect_timeout)?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(opts.write_timeout));
    Ok(Box::new(stream))
}

#[cfg(feature = "tls")]
fn dial_tls(host: &str, port: u16, opts: &ClientOptions) -> Result<BoxIo> {
    use std::time::SystemTime;

    use rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::SignatureScheme;

    let tcp = TcpStream::connect_timeout(&resolve(host, port)?, opts.connect_timeout)?;
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(READ_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(opts.write_timeout));

    let insecure = opts.tls.as_ref().map(|t| t.insecure).unwrap_or(false);

    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Error::Tls(e.to_string()))?;

    let config = if insecure {
        #[derive(Debug)]
        struct NoVerify;
        impl ServerCertVerifier for NoVerify {
            fn verify_server_cert(
                &self,
                _end_entity: &CertificateDer<'_>,
                _intermediates: &[CertificateDer<'_>],
                _server_name: &ServerName<'_>,
                _ocsp_response: &[u8],
                _now: UnixTime,
            ) -> std::result::Result<ServerCertVerified, rustls::Error> {
                Ok(ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &rustls::DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _message: &[u8],
                _cert: &CertificateDer<'_>,
                _dss: &rustls::DigitallySignedStruct,
            ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
                Ok(HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                rustls::crypto::ring::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs()
            .certs
            .into_iter()
            .map(|c| c.into())
        {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };

    let server_name = opts
        .tls
        .as_ref()
        .and_then(|t| t.server_name.clone())
        .unwrap_or_else(|| host.to_string());
    let server_name = ServerName::try_from(server_name)
        .map_err(|_| Error::Tls(format!("invalid server name '{host}'")))?;

    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| Error::Tls(e.to_string()))?;
    let mut stream = rustls::StreamOwned::new(conn, tcp);
    stream
        .flush()
        .map_err(|e| Error::Io(e))?;
    let _ = SystemTime::now(); // keep SystemTime import used when insecure path compiles out
    Ok(Box::new(stream))
}

#[cfg(not(feature = "tls"))]
fn dial_tls(_host: &str, _port: u16, _opts: &ClientOptions) -> Result<BoxIo> {
    Err(Error::TlsUnavailable)
}

/// Write CONNECT, then read CONNACK (within the connect timeout).
fn handshake(stream: &mut BoxIo, opts: &ClientOptions) -> Result<u16> {
    let connect = codec::encode_connect(opts.protocol_version, opts.heartbeat_seconds, &opts.credential);
    stream.write_all(&connect)?;

    let deadline = Instant::now() + opts.connect_timeout;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        match codec::decode(&buf) {
            Ok(Some((packet, _))) => match packet {
                codec::Packet::Connack { return_code, server_keepalive } => {
                    if return_code != codec::returncode::ACCEPTED {
                        return Err(Error::ConnRefused(return_code));
                    }
                    return Ok(server_keepalive);
                }
                _ => return Err(Error::Protocol("first packet is not a CONNACK".into())),
            },
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(Error::Io(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "CONNACK timeout",
                    )));
                }
                match stream.read(&mut tmp) {
                    Ok(0) => {
                        return Err(Error::Io(std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "connection closed during CONNACK",
                        )))
                    }
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(Error::Io(e)),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Reconnect backoff
// ---------------------------------------------------------------------------

fn backoff_next(cur: Duration, opts: &ClientOptions) -> Duration {
    let doubled = cur * 2;
    if doubled >= opts.reconnect_max {
        opts.reconnect_max
    } else {
        doubled
    }
}

fn sleep_jittered(base: Duration, opts: &ClientOptions) {
    let jitter = opts
        .reconnect_jitter();
    let factor = 1.0 - jitter + rand_jitter() * (2.0 * jitter);
    std::thread::sleep(Duration::from_secs_f64(base.as_secs_f64() * factor));
}

fn rand_jitter() -> f64 {
    // simple deterministic-free jitter using time + an LCG; no extra dep.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// In-memory transport: serves a canned server response, records what the
    /// client wrote. Satisfies `Io` so the real `handshake` path runs on it.
    struct FakeStream {
        responses: std::io::Cursor<Vec<u8>>,
        written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.responses.read(buf)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fake_connack(rc: u8, kp: u16) -> Vec<u8> {
        let mut v = vec![codec::mtype::CONNACK << 4, 0x03, rc];
        v.extend_from_slice(&kp.to_be_bytes());
        v
    }

    fn fake_server(responses: Vec<u8>) -> (BoxIo, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stream = Box::new(FakeStream {
            responses: std::io::Cursor::new(responses),
            written: written.clone(),
        });
        (stream, written)
    }

    #[test]
    fn handshake_accepts_connack() {
        let (mut s, written) = fake_server(fake_connack(codec::returncode::ACCEPTED, 30));
        let kp = handshake(&mut s, &ClientOptions::default()).unwrap();
        assert_eq!(kp, 30);
        let w = written.lock().unwrap();
        assert_eq!(w[0], codec::mtype::CONNECT << 4);
        assert_eq!(&w[1..], &[10u8, 0x63, 0x7a, 0x71, 0x75, 0x01, 0x00, 0x00, 0x0a, 0x00, 0x00]);
    }

    #[test]
    fn handshake_rejects_connack_return_code() {
        let (mut s, _) = fake_server(fake_connack(codec::returncode::NOT_AUTHORISED, 0));
        match handshake(&mut s, &ClientOptions::default()) {
            Err(Error::ConnRefused(0x03)) => {}
            other => panic!("expected ConnRefused(0x03), got {other:?}"),
        }
    }

    #[test]
    fn handshake_rejects_non_connack_first_packet() {
        let (mut s, _) = fake_server(vec![0x60, 0x00]);
        assert!(matches!(handshake(&mut s, &ClientOptions::default()), Err(Error::Protocol(_))));
    }

    #[test]
    fn split_hostport_ok() {
        assert_eq!(split_hostport("127.0.0.1:18883").unwrap(), ("127.0.0.1".to_string(), 18883));
        assert_eq!(split_hostport("/127.0.0.1:80").unwrap(), ("127.0.0.1".to_string(), 80));
        assert_eq!(split_hostport("[::1]:8080").unwrap(), ("[::1]".to_string(), 8080));
    }

    #[test]
    fn split_hostport_missing_port() {
        assert!(matches!(split_hostport("127.0.0.1"), Err(Error::BadAddress(_))));
    }

    #[test]
    fn split_hostport_bad_port() {
        assert!(matches!(split_hostport("127.0.0.1:abc"), Err(Error::BadAddress(_))));
    }

    #[test]
    fn dial_rejects_unknown_scheme() {
        assert!(matches!(
            dial("quic://127.0.0.1:443", &ClientOptions::default()),
            Err(Error::BadAddress(_))
        ));
    }

    #[test]
    fn backoff_doubles_up_to_max() {
        let opts = ClientOptions::default();
        assert_eq!(backoff_next(Duration::from_secs(1), &opts), Duration::from_secs(2));
        assert_eq!(backoff_next(Duration::from_secs(300), &opts), Duration::from_secs(600));
        assert_eq!(backoff_next(Duration::from_secs(400), &opts), Duration::from_secs(600));
        let capped = ClientOptions { reconnect_max: Duration::from_secs(10), ..Default::default() };
        assert_eq!(backoff_next(Duration::from_secs(6), &capped), Duration::from_secs(10));
    }

    #[test]
    fn rand_jitter_within_unit_interval() {
        for _ in 0..100 {
            let j = rand_jitter();
            assert!((0.0..1.0).contains(&j), "jitter {j} out of range");
        }
    }
}