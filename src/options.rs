//! Options controlling client behaviour.

use std::time::Duration;

/// TLS verification mode (used by the `tls` feature only).
#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    /// Server name for SNI / cert verification. Defaults to the host name.
    pub server_name: Option<String>,
    /// Disable certificate verification (for self-signed / no-SAN test servers).
    pub insecure: bool,
}

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Credential presented in CONNECT. Opaque to the protocol.
    pub credential: String,
    /// Fixed heartbeat proposal in seconds (0 = do not specify, the server
    /// decides; a nonzero value is subject to the server's [-min, max] policy).
    pub heartbeat_seconds: u16,
    /// Protocol version (must be 1).
    pub protocol_version: u8,
    /// Timeout for establishing the transport + completing the CONNECT handshake.
    pub connect_timeout: Duration,
    /// Deadline for writing any outbound packet / push.
    pub write_timeout: Duration,
    /// Time to wait for a PINGRESP/any inbound packet before calling the link dead.
    pub response_timeout: Duration,
    /// Whether to automatically reconnect on an unexpected connection loss.
    pub auto_reconnect: bool,
    /// Base interval for reconnect exponential backoff.
    pub reconnect_base: Duration,
    /// Upper bound for the reconnect backoff interval.
    pub reconnect_max: Duration,
    /// Jitter factor (0..=1) applied to the backoff interval.
    pub reconnect_jitter: f64,
    /// TLS configuration / verification mode.
    pub tls: Option<TlsConfig>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            credential: String::new(),
            heartbeat_seconds: 10,
            protocol_version: 1,
            connect_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(30),
            response_timeout: Duration::from_secs(0), // auto: 1.5x heartbeat
            auto_reconnect: true,
            reconnect_base: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(600),
            reconnect_jitter: 0.25,
            tls: None,
        }
    }
}

impl ClientOptions {
    /// Effective response timeout: explicit value, else 1.5x the heartbeat.
    pub fn effective_response_timeout(&self) -> Duration {
        if self.response_timeout > Duration::ZERO {
            return self.response_timeout;
        }
        Duration::from_secs_f64(((self.heartbeat_seconds as f64) * 1.5).max(1.0))
    }

    /// The jitter factor, clamped to [0, 1].
    pub fn reconnect_jitter(&self) -> f64 {
        self.reconnect_jitter.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let o = ClientOptions::default();
        assert_eq!(o.credential, "");
        assert_eq!(o.heartbeat_seconds, 10);
        assert_eq!(o.protocol_version, 1);
        assert_eq!(o.connect_timeout, Duration::from_secs(5));
        assert_eq!(o.write_timeout, Duration::from_secs(30));
        assert_eq!(o.response_timeout, Duration::ZERO);
        assert!(o.auto_reconnect);
        assert_eq!(o.reconnect_base, Duration::from_secs(1));
        assert_eq!(o.reconnect_max, Duration::from_secs(600));
        assert_eq!(o.reconnect_jitter(), 0.25);
        assert!(o.tls.is_none());
    }

    #[test]
    fn explicit_response_timeout_wins() {
        let o = ClientOptions { response_timeout: Duration::from_secs(7), ..Default::default() };
        assert_eq!(o.effective_response_timeout(), Duration::from_secs(7));
    }

    #[test]
    fn response_timeout_derived_from_heartbeat() {
        let o = ClientOptions { heartbeat_seconds: 4, ..Default::default() };
        assert_eq!(o.effective_response_timeout(), Duration::from_secs(6));
    }

    #[test]
    fn response_timeout_floor_of_one_second() {
        // heartbeat 0 (unspecified) must not collapse the timeout to zero
        let o = ClientOptions { heartbeat_seconds: 0, ..Default::default() };
        assert_eq!(o.effective_response_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn jitter_clamped_to_unit_range() {
        let high = ClientOptions { reconnect_jitter: 1.5, ..Default::default() };
        assert_eq!(high.reconnect_jitter(), 1.0);
        let low = ClientOptions { reconnect_jitter: -0.5, ..Default::default() };
        assert_eq!(low.reconnect_jitter(), 0.0);
        let mid = ClientOptions { reconnect_jitter: 0.3, ..Default::default() };
        assert_eq!(mid.reconnect_jitter(), 0.3);
    }
}