//! A received PUSH payload.

/// An application message delivered by the protocol.
#[derive(Debug, Clone)]
pub struct Message {
    payload: Vec<u8>,
}

impl Message {
    pub(crate) fn new(payload: Vec<u8>) -> Self {
        Message { payload }
    }

    /// The opaque application payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// UTF-8 lossy string representation of the payload.
    pub fn to_utf8_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let m = Message::new(vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(m.payload(), &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn utf8_lossy_valid() {
        let m = Message::new("héllo".as_bytes().to_vec());
        assert_eq!(m.to_utf8_lossy(), "héllo");
    }

    #[test]
    fn utf8_lossy_invalid() {
        let m = Message::new(vec![0xff, 0xfe, 0x00]);
        assert!(m.to_utf8_lossy().contains('\u{FFFD}'));
    }
}
