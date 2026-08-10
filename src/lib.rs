//! # rmtt
//!
//! A minimal library for the RMTT protocol (Remote Message Telemetry
//! Transport), a lightweight point-to-point long-lived message push protocol.
//!
//! ## Full client (default features)
//!
//! With the default `client` + `tls` features you get the [`Client`] reactor
//! (self-managed IO thread: handshake, heartbeat, payload handler, reconnect)
//! over TCP/TLS.
//!
//! ```no_run
//! # #[cfg(feature = "client")]
//! # {
//! use rmtt::{Client, ClientOptions};
//!
//! let opts = ClientOptions {
//!     credential: "dev-001".into(),
//!     heartbeat_seconds: 5,
//!     ..Default::default()
//! };
//! let client = Client::connect("127.0.0.1:18883", opts)?;
//! client.push(b"hello");
//! client.disconnect();
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Codec-only (no transport whatsoever)
//!
//! If you bring your **own** transport (HTTP/2, QUIC, a custom framing, raw
//! sockets, ...), depend on the crate with no default features and use just the
//! [`codec`] module. `default-features = false` compiles only `codec` and
//! `error` — **no `TcpStream`, no `rustls`/`ring`**:
//!
//! ```toml
//! [dependencies]
//! rmtt = { version = "0.1", default-features = false }
//! ```
//!
//! ```no_run
//! use rmtt::codec::{decode, encode_connect, encode_push, Packet};
//!
//! let connect = encode_connect(1, 30, "dev-001");
//! // my_stream.write_all(&connect)?; // any Reader/Writer of your own
//!
//! let mut buf: Vec<u8> = Vec::new();
//! buf.extend_from_slice(&connect);
//! match decode(&buf)? {
//!     Some((Packet::Connack { return_code, server_keepalive }, _)) => { /* ... */ }
//!     _ => { /* more bytes needed, or handle another packet type */ }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! - `default-features = false` → codec + error only.
//! - `features = ["client"]` → codec + the blocking TCP client (`tls://` is a
//!   `TlsUnavailable` error).

pub mod codec;
pub mod error;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub mod message;
#[cfg(feature = "client")]
pub mod options;

pub use error::{Error, Result};

#[cfg(feature = "client")]
pub use client::Client;
#[cfg(feature = "client")]
pub use message::Message;
#[cfg(feature = "client")]
pub use options::{ClientOptions, TlsConfig};