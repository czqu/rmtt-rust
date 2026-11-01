# rmtt (Rust)

A minimal, dependency-light client and server library for the **RMTT**
protocol (Remote Message Telemetry Transport), a point-to-point long-lived
message push protocol.

## Transport support

| Scheme | Transport |
|--------|-----------|
| `tcp://` | TCP (built-in, no deps) |
| `tls://` | TLS 1.3 via `rustls`, feature-gated (`tls`, on by default) |

## Feature selection

| Cargo features | What is compiled |
|----------------|------------------|
| `default` (= `client` + `tls`) | full [`Client`] over TCP + TLS |
| `default-features = false` | `codec` + `error` only — no TCP, no rustls |
| `features = ["client"]` | `codec` + error + the blocking TCP client (`tls://` → `TlsUnavailable`) |
| `features = ["server"]` | `codec` + error + the TCP `server` (opt-in, not in `default`) |

Whether you link the network stack is a compile-time decision that has no
effect on runtime performance of the parts you keep; with no default features
the crate builds no `TcpStream` and no `rustls`/`ring` code at all.

## Codec-only usage

When you bring your own transport (HTTP/2, QUIC, a custom protocol, raw
sockets), depend with `default-features = false` and drive `rmtt::codec`
yourself:

```toml
[dependencies]
rmtt = { version = "0.1", default-features = false }
```

```rust
use rmtt::codec::{decode, encode_connect, encode_push, Packet};

let connect = encode_connect(1, 30, "dev-001");
my_stream.write_all(&connect)?; // any Reader/Writer you own

match decode(&buf)? {
    Some((Packet::Connack { return_code, server_keepalive }, _)) => { /* ... */ }
    _ => { /* need more bytes, or another packet type */ }
}
```

`codec` exposes the six wire packet types (CONNECT/CONNACK/PUSH/PINGREQ/
PINGRESP/DISCONNECT), their encode helpers, a streaming `decode` (returns
`Ok(None)` while a frame is partial) and the shared constants
(`mtype`/`returncode`/`disconnect`) plus the `reconnect_policy` map.

## Usage

```rust
use rmtt::{Client, ClientOptions};

let opts = ClientOptions {
    credential: "dev-001".into(),   // opaque credential offered in CONNECT
    heartbeat_seconds: 30,          // keepalive proposal (0 = not specified)
    ..Default::default()
};

// Blocks until CONNECT/CONNACK completes, or errors.
let client = Client::connect("tcp://127.0.0.1:18883", opts)?;

client.set_payload_handler(|msg| {
    println!("recv: {}", msg.to_utf8_lossy());
});

client.push(b"hello device")?;      // send a PUSH
client.disconnect();                // clean DISCONNECT(0x00), blocks until written
```

`server_keepalive()` reports the server-negotiated value from CONNACK; the
library then drives PINGREQ/PINGRESP automatically at that cadence and tears
down / reconnects when no packet is received for `1.5 × keepalive`.

## Server usage

The `server` feature (opt-in, no extra dependencies) provides a TCP server
mirroring the reference `rmtt-java` / `rmtt-go` servers: CONNECT
authentication, keepalive negotiation and enforcement, session takeover,
downstream push and kick.

```toml
[dependencies]
rmtt = { version = "0.1", features = ["server"] }
```

```rust
use std::sync::Arc;

use rmtt::server::{AuthResult, Server, ServerOptions};

let server = Arc::new(Server::bind("0.0.0.0:18883", ServerOptions::default())?);

// Map CONNECT credentials to device ids (default: credential == device id).
server.set_authenticator(|credential| AuthResult::allow(credential));

// Uplink PUSH dispatch; echo every payload back to the sender.
let s = server.clone();
server.set_message_handler(move |device_id, payload| {
    let _ = s.push(device_id, payload);
});

server.set_on_connect(|device_id| println!("{device_id} online"));
server.set_on_disconnect(|device_id, reason| println!("{device_id} left: {reason}"));

assert!(server.is_online("dev-001"));
server.push("dev-001", b"hello device")?;      // downstream PUSH
server.kick("dev-001", 0x06)?;                  // DISCONNECT(0x06) + close
server.shutdown();                              // DISCONNECT(0x03) to all
```

Server behaviour (aligned with the Java/Go references):

- **Handshake** — bad magic closes silently, a bad protocol version answers
  `CONNACK(0x01)`, a rejected credential answers `CONNACK(<return code>)`.
- **Keepalive** — `KeepalivePolicy` clamps the client proposal (default
  `[30, 600]`s, 60s fallback) and echoes it in CONNACK; a silent connection
  is reaped with `DISCONNECT(0x05)` after `1.5 × server_keepalive`.
- **Session takeover** — a second CONNECT for the same device id supersedes
  the old connection, which receives `DISCONNECT(0x02)`.
- **Protocol violations** — mid-stream garbage is answered with
  `DISCONNECT(0x04)`.

## Options

- `heartbeat_seconds` — keepalive proposal. `0` = "do not specify"; the server
  decides (and may disable it via `server_kp=0`).
- `connect_timeout` / `write_timeout` — deadlines for the handshake and each write.
- `auto_reconnect` — on an unexpected loss, reconnect with exponential backoff
  (`reconnect_base`, `reconnect_max`, `reconnect_jitter`).
- `tls` — `TlsConfig { server_name, insecure }`. `insecure = true` skips cert
  verification (for self-signed / no-SAN test servers).

## Message flow

On CONNECT the protocol magic (`0x637A7175 "czqu"`), version and credential are
validated by the server. PUSH bodies carry the `Reserved` byte then the opaque
payload. DISCONNECT carries a 1-byte return code (`codec::disconnect`).

A frame body larger than 16 MiB (`codec::MAX_FRAME_LEN`) is a protocol
violation: it is rejected as soon as the fixed header is parsed — before any
body byte is buffered — with `DISCONNECT(0x04)`.

## Testing

```
cargo test                # codec + client tests (default features)
cargo test --all-features # additionally runs the server interop tests
```

The cross-language E2E matrix (java/rust servers x go/rust clients) lives in
`e2e/` and runs in CI; reproduce a cell locally with
`e2e/run_e2e.sh <java|rust> <go|rust>` after building the components.