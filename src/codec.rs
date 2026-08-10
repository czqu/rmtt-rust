//! RMTT wire codec: packet encode/decode for the RMTT wire format.

use crate::error::Error;

/// Packet type identifiers (fixed header high nibble).
pub mod mtype {
    pub const CONNECT: u8 = 0x01;
    pub const CONNACK: u8 = 0x02;
    pub const PUSH: u8 = 0x03;
    pub const PINGREQ: u8 = 0x05;
    pub const PINGRESP: u8 = 0x06;
    pub const DISCONNECT: u8 = 0x0e;
}

/// Fixed magic number "czqu".
pub const MAGIC_NUMBER: u32 = 0x637A_7175;

pub const CONNECT_MAGIC_OFF: usize = 0;
pub const CONNECT_VER_OFF: usize = 4;
pub const CONNECT_FLAGS_OFF: usize = 5;
pub const CONNECT_KEEPALIVE_OFF: usize = 6;
pub const CONNECT_CRED_LEN_OFF: usize = 8;
pub const CONNECT_CRED_OFF: usize = 10;

/// CONNACK return codes.
pub mod returncode {
    pub const ACCEPTED: u8 = 0x00;
    pub const BAD_PROTOCOL_VERSION: u8 = 0x01;
    pub const SERVER_UNAVAILABLE: u8 = 0x02;
    pub const NOT_AUTHORISED: u8 = 0x03;
    pub const NETWORK_ERROR: u8 = 0xfe;
    pub const UNSUPPORTED: u8 = 0xff;
}

/// DISCONNECT return codes.
pub mod disconnect {
    pub const NORMAL: u8 = 0x00;
    pub const CREDENTIAL_EXPIRED: u8 = 0x01;
    pub const SESSION_TAKEN_OVER: u8 = 0x02;
    pub const SERVER_SHUTDOWN: u8 = 0x03;
    pub const PROTOCOL_VIOLATION: u8 = 0x04;
    pub const KEEPALIVE_TIMEOUT: u8 = 0x05;
    pub const KICKED_BY_ADMIN: u8 = 0x06;
    pub const RATE_LIMITED: u8 = 0x07;
    pub const CREDENTIAL_REJECTED: u8 = 0x08;
    pub const UNKNOWN: u8 = 0xfe;
}

/// A decoded inbound packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Connack { return_code: u8, server_keepalive: u16 },
    Push(Vec<u8>),
    Pingreq,
    Pingresp,
    Disconnect(u8),
}

/// Encode the fixed header (type flags + remaining length).
fn encode_fixed_header(msg_type: u8, remaining: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(msg_type << 4);
    let mut len = remaining;
    loop {
        let mut digit = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            digit |= 0x80;
        }
        out.push(digit);
        if len == 0 {
            break;
        }
    }
    out
}

/// Build a CONNECT packet body.
///
/// Frame: `U32(magic) | U8(ver) | U8(flags) | U16(keepalive) | String(credential)`.
pub fn encode_connect(protocol_version: u8, keepalive: u16, credential: &str) -> Vec<u8> {
    let cred = credential.as_bytes();
    let body_len = 10 + cred.len();
    let mut out = encode_fixed_header(mtype::CONNECT, body_len);
    out.extend_from_slice(&MAGIC_NUMBER.to_be_bytes());
    out.push(protocol_version);
    out.push(0x00); // flags, reserved
    out.extend_from_slice(&keepalive.to_be_bytes());
    out.extend_from_slice(&(cred.len() as u16).to_be_bytes());
    out.extend_from_slice(cred);
    out
}

/// Build a PUSH packet (reserved byte 0x00 + payload).
pub fn encode_push(payload: &[u8]) -> Vec<u8> {
    let mut out = encode_fixed_header(mtype::PUSH, 1 + payload.len());
    out.push(0x00);
    out.extend_from_slice(payload);
    out
}

/// Build a PINGREQ packet (no body).
pub fn encode_pingreq() -> Vec<u8> {
    encode_fixed_header(mtype::PINGREQ, 0)
}

/// Build a DISCONNECT packet with the given return code.
pub fn encode_disconnect(return_code: u8) -> Vec<u8> {
    let mut out = encode_fixed_header(mtype::DISCONNECT, 1);
    out.push(return_code);
    out
}

/// Attempt to decode a single complete packet from the buffer.
///
/// Returns `Ok(Some((packet, consumed)))` when a full frame is present,
/// `Ok(None)` when more bytes are needed (read more into `buf` and retry),
/// or `Err` on a wire/format violation.
pub fn decode(buf: &[u8]) -> std::result::Result<Option<(Packet, usize)>, Error> {
    if buf.is_empty() {
        return Ok(None);
    }
    let first = buf[0];
    let msg_type = first >> 4;
    let flags = first & 0x0f;
    if flags != 0x00 {
        return Err(Error::Protocol(format!("non-zero fixed header flags 0x{flags:02x}")));
    }

    // Decode variable-length remaining length (max 4 bytes as the server tolerates).
    let mut remaining: usize = 0;
    let mut multiplier: usize = 1;
    let mut idx = 1usize;
    let mut nl = 1usize;
    loop {
        if idx >= buf.len() {
            return Ok(None);
        }
        let digit = buf[idx];
        idx += 1;
        remaining += (digit as usize & 0x7f) * multiplier;
        if digit & 0x80 == 0 {
            break;
        }
        multiplier *= 128;
        nl += 1;
        if nl > 4 {
            return Err(Error::Protocol("malformed remaining length".into()));
        }
    }

    if buf.len() < idx + remaining {
        return Ok(None);
    }
    let body = &buf[idx..idx + remaining];
    let consumed = idx + remaining;

    let packet = match msg_type {
        mtype::CONNACK => {
            if body.len() < 3 {
                return Err(Error::Protocol("CONNACK body too short".into()));
            }
            let return_code = body[0];
            let server_keepalive = u16::from_be_bytes([body[1], body[2]]);
            Packet::Connack { return_code, server_keepalive }
        }
        mtype::PUSH => {
            if body.is_empty() {
                return Err(Error::Protocol("PUSH body missing reserved byte".into()));
            }
            Packet::Push(body[1..].to_vec())
        }
        mtype::PINGREQ => Packet::Pingreq,
        mtype::PINGRESP => Packet::Pingresp,
        mtype::DISCONNECT => {
            if body.is_empty() {
                return Err(Error::Protocol("DISCONNECT body too short".into()));
            }
            Packet::Disconnect(body[0])
        }
        other => return Err(Error::Protocol(format!("unsupported packet type 0x{other:02x}"))),
    };

    Ok(Some((packet, consumed)))
}

/// A reconnect decision derived from a server DISCONNECT return code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectPolicy {
    /// Normal disconnect — do not reconnect unless user requests.
    DoNotReconnect,
    /// Refresh the credential and reconnect.
    RefreshCredential,
    /// Exponential backoff + jitter.
    Backoff,
    /// Backoff with a large base (e.g. RateLimited).
    LongBackoff,
    /// Immediate reconnect.
    Immediately,
}

/// Map a server DISCONNECT return code to the recommended client action.
pub fn reconnect_policy(rc: u8) -> ReconnectPolicy {
    match rc {
        disconnect::NORMAL => ReconnectPolicy::DoNotReconnect,
        disconnect::CREDENTIAL_EXPIRED => ReconnectPolicy::RefreshCredential,
        disconnect::SESSION_TAKEN_OVER | disconnect::KICKED_BY_ADMIN => ReconnectPolicy::DoNotReconnect,
        disconnect::SERVER_SHUTDOWN => ReconnectPolicy::Backoff,
        disconnect::PROTOCOL_VIOLATION => ReconnectPolicy::Backoff,
        disconnect::KEEPALIVE_TIMEOUT => ReconnectPolicy::Immediately,
        disconnect::RATE_LIMITED => ReconnectPolicy::LongBackoff,
        disconnect::CREDENTIAL_REJECTED => ReconnectPolicy::Backoff,
        _ => ReconnectPolicy::Backoff,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_roundtrip_shape() {
        let pkt = encode_connect(1, 30, "credential");
        assert_eq!(pkt[0], mtype::CONNECT << 4);
        // body len = 4+1+1+2+2 + 10 ("credential") = 20
        assert_eq!(pkt[1], 20);
        let _ = &pkt[..]; // (keep slice borrow grouped with the length check)
        assert_eq!(&pkt[2..6], &MAGIC_NUMBER.to_be_bytes());
        assert_eq!(pkt[6], 1);
        assert_eq!(pkt[7], 0);
        assert_eq!(&pkt[8..10], &30u16.to_be_bytes());
        assert_eq!(&pkt[10..12], &10u16.to_be_bytes());
        assert_eq!(&pkt[12..], b"credential");
    }

    #[test]
    fn connack_decode() {
        // fixed header 0x20, remaining 3, body: rc=0, kp=30
        let buf = [0x20, 0x03, 0x00, 0x00, 0x1e];
        let (pkt, consumed) = decode(&buf).unwrap().unwrap();
        assert_eq!(consumed, 5);
        assert_eq!(pkt, Packet::Connack { return_code: 0, server_keepalive: 30 });
    }

    #[test]
    fn push_decode_ignores_reserved() {
        // fixed header 0x30, remaining 1+3, body reserved=0 payload="abc"
        let buf = [0x30, 0x04, 0x00, b'a', b'b', b'c'];
        let (pkt, consumed) = decode(&buf).unwrap().unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(pkt, Packet::Push(b"abc".to_vec()));
    }

    #[test]
    fn pingreq_and_pingresp() {
        assert_eq!(decode(&[0x50, 0x00]).unwrap().unwrap().0, Packet::Pingreq);
        assert_eq!(decode(&[0x60, 0x00]).unwrap().unwrap().0, Packet::Pingresp);
    }

    #[test]
    fn disconnect_decode() {
        let (pkt, _) = decode(&[0xe0, 0x01, 0x06]).unwrap().unwrap();
        assert_eq!(pkt, Packet::Disconnect(0x06));
    }

    #[test]
    fn non_zero_flags_rejected() {
        // type=PUSH but flags=1 -> 0x31
        assert!(decode(&[0x31, 0x00]).is_err());
    }

    #[test]
    fn partial_frame_returns_none() {
        // only half of a CONNACK frame
        assert_eq!(decode(&[0x20, 0x03, 0x00]).unwrap(), None);
    }

    #[test]
    fn unknown_type_rejected() {
        // type=0 -> frame is just header+0
        assert!(decode(&[0x00, 0x00]).is_err());
    }

    #[test]
    fn encoded_length_multibyte() {
        // payload of 200 bytes -> remaining 201 -> 0xC9 0x01
        let payload = vec![0u8; 200];
        let pkt = encode_push(&payload);
        assert_eq!(pkt[0], mtype::PUSH << 4);
        assert_eq!(pkt[1], 0xc9);
        assert_eq!(pkt[2], 0x01);
        let (back, _) = decode(&pkt).unwrap().unwrap();
        assert_eq!(back, Packet::Push(vec![0u8; 200]));
    }

    #[test]
    fn encode_pingreq_shape() {
        assert_eq!(encode_pingreq(), vec![mtype::PINGREQ << 4, 0x00]);
    }

    #[test]
    fn encode_disconnect_shape() {
        let pkt = encode_disconnect(disconnect::KICKED_BY_ADMIN);
        assert_eq!(pkt, vec![mtype::DISCONNECT << 4, 0x01, 0x06]);
    }

    #[test]
    fn decode_empty_buffer_returns_none() {
        assert_eq!(decode(&[]).unwrap(), None);
    }

    #[test]
    fn truncated_remaining_length_returns_none() {
        // header claims a multi-byte remaining length but only one digit byte present
        assert_eq!(decode(&[0x20, 0x80]).unwrap(), None);
    }

    #[test]
    fn remaining_length_too_long_rejected() {
        // five continuation digits -> malformed remaining length
        let buf = [0x30, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn connack_body_too_short_rejected() {
        assert!(decode(&[0x20, 0x02, 0x00, 0x00]).is_err());
    }

    #[test]
    fn push_without_reserved_byte_rejected() {
        assert!(decode(&[0x30, 0x00]).is_err());
    }

    #[test]
    fn disconnect_without_body_rejected() {
        assert!(decode(&[0xe0, 0x00]).is_err());
    }

    #[test]
    fn push_nonzero_reserved_ignored() {
        // the reserved byte is opaque: any value is stripped from the payload
        let buf = [0x30, 0x04, 0x7f, b'a', b'b', b'c'];
        let (pkt, _) = decode(&buf).unwrap().unwrap();
        assert_eq!(pkt, Packet::Push(b"abc".to_vec()));
    }

    #[test]
    fn multiple_packets_in_one_buffer() {
        // decode reports the consumed length so a caller can walk a stream
        let mut buf = encode_pingreq();
        buf.extend_from_slice(&encode_disconnect(disconnect::NORMAL));
        let (pkt, consumed) = decode(&buf).unwrap().unwrap();
        assert_eq!(pkt, Packet::Pingreq);
        assert_eq!(consumed, 2);
        let (pkt, consumed) = decode(&buf[consumed..]).unwrap().unwrap();
        assert_eq!(pkt, Packet::Disconnect(disconnect::NORMAL));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn reconnect_policy_mapping() {
        assert_eq!(reconnect_policy(disconnect::NORMAL), ReconnectPolicy::DoNotReconnect);
        assert_eq!(reconnect_policy(disconnect::CREDENTIAL_EXPIRED), ReconnectPolicy::RefreshCredential);
        assert_eq!(reconnect_policy(disconnect::SESSION_TAKEN_OVER), ReconnectPolicy::DoNotReconnect);
        assert_eq!(reconnect_policy(disconnect::KICKED_BY_ADMIN), ReconnectPolicy::DoNotReconnect);
        assert_eq!(reconnect_policy(disconnect::SERVER_SHUTDOWN), ReconnectPolicy::Backoff);
        assert_eq!(reconnect_policy(disconnect::PROTOCOL_VIOLATION), ReconnectPolicy::Backoff);
        assert_eq!(reconnect_policy(disconnect::KEEPALIVE_TIMEOUT), ReconnectPolicy::Immediately);
        assert_eq!(reconnect_policy(disconnect::RATE_LIMITED), ReconnectPolicy::LongBackoff);
        assert_eq!(reconnect_policy(disconnect::CREDENTIAL_REJECTED), ReconnectPolicy::Backoff);
    }

    #[test]
    fn reconnect_policy_unknown_code_defaults_to_backoff() {
        assert_eq!(reconnect_policy(0x77), ReconnectPolicy::Backoff);
    }
}