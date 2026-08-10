//! Error types for the RMT client.

use std::fmt;

/// Protocol / transport error returned by the library.
#[derive(Debug)]
pub enum Error {
    /// I/O or TLS failure on the underlying transport.
    Io(std::io::Error),
    /// A packet violated the wire format (bad magic, bad flags, truncated body...).
    Protocol(String),
    /// The server rejected the CONNECT.
    ConnRefused(u8),
    /// The client is not currently connected (can also mean "not connecting").
    Closed,
    /// A user-facing operation was called while the client was busy invalidly.
    NotConnected,
    /// Failed to parse the broker address.
    BadAddress(String),
    /// TLS was not compiled into this build.
    TlsUnavailable,
    /// TLS configuration/handshake error.
    Tls(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Protocol(s) => write!(f, "protocol violation: {s}"),
            Error::ConnRefused(rc) => write!(f, "connection refused (return code 0x{rc:02x})"),
            Error::Closed => write!(f, "connection is closed"),
            Error::NotConnected => write!(f, "not connected"),
            Error::BadAddress(s) => write!(f, "invalid address: {s}"),
            Error::TlsUnavailable => write!(f, "tls support not compiled in (enable the `tls` feature)"),
            Error::Tls(s) => write!(f, "tls error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Convenience result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(
            Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "x")).to_string(),
            "io error: x"
        );
        assert_eq!(Error::Protocol("bad magic".into()).to_string(), "protocol violation: bad magic");
        assert_eq!(Error::ConnRefused(0x03).to_string(), "connection refused (return code 0x03)");
        assert_eq!(Error::Closed.to_string(), "connection is closed");
        assert_eq!(Error::NotConnected.to_string(), "not connected");
        assert_eq!(Error::BadAddress("nope".into()).to_string(), "invalid address: nope");
        assert_eq!(
            Error::TlsUnavailable.to_string(),
            "tls support not compiled in (enable the `tls` feature)"
        );
        assert_eq!(Error::Tls("boom".into()).to_string(), "tls error: boom");
    }

    #[test]
    fn io_error_conversion() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "t");
        let e: Error = io.into();
        assert!(matches!(e, Error::Io(_)));
    }

    #[test]
    fn usable_as_std_error() {
        let boxed: Box<dyn std::error::Error> = Box::new(Error::Closed);
        assert_eq!(boxed.to_string(), "connection is closed");
        assert!(boxed.source().is_none());
    }
}