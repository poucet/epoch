//! The kernel's content-address: a 32-byte BLAKE3 digest.

use std::fmt;
use std::str::FromStr;

/// A content-address: the BLAKE3 digest of a canonical byte encoding.
///
/// This is the kernel's own id type. It is deliberately id-agnostic — it knows
/// nothing of `BlobId`/`TreeId` or any domain newtype, which are layered on top
/// elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    /// The BLAKE3 digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    /// Construct from raw digest bytes (no hashing performed).
    pub const fn from_bytes(bytes: [u8; 32]) -> Hash {
        Hash(bytes)
    }

    /// The raw 32-byte digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex representation (64 chars).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in &self.0 {
            // Two lowercase hex digits per byte.
            s.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
        }
        s
    }
}

/// Hex on both `Display` and `Debug` so logs and errors are readable.
impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

/// The error returned when a hex string cannot be parsed into a [`Hash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseHashError {
    /// The input was not exactly 64 hex characters long.
    BadLength {
        /// The actual length received.
        got: usize,
    },
    /// The input contained a non-hex character.
    BadChar {
        /// The offending character.
        ch: char,
    },
}

impl fmt::Display for ParseHashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseHashError::BadLength { got } => {
                write!(f, "invalid hash length: expected 64 hex chars, got {got}")
            }
            ParseHashError::BadChar { ch } => {
                write!(f, "invalid hex character in hash: {ch:?}")
            }
        }
    }
}

impl std::error::Error for ParseHashError {}

impl FromStr for Hash {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 64 {
            return Err(ParseHashError::BadLength { got: s.len() });
        }
        let mut out = [0u8; 32];
        let bytes = s.as_bytes();
        for i in 0..32 {
            let hi = hex_val(bytes[i * 2])?;
            let lo = hex_val(bytes[i * 2 + 1])?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Hash(out))
    }
}

fn hex_val(b: u8) -> Result<u8, ParseHashError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(ParseHashError::BadChar { ch: b as char }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_is_deterministic() {
        assert_eq!(Hash::of(b"hello"), Hash::of(b"hello"));
        assert_ne!(Hash::of(b"hello"), Hash::of(b"world"));
    }

    #[test]
    fn hex_roundtrip() {
        let h = Hash::of(b"some content");
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        let parsed: Hash = s.parse().unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn parse_rejects_bad_length() {
        assert_eq!(
            "abc".parse::<Hash>(),
            Err(ParseHashError::BadLength { got: 3 })
        );
    }

    #[test]
    fn parse_rejects_bad_char() {
        let bad = "z".repeat(64);
        assert!(matches!(
            bad.parse::<Hash>(),
            Err(ParseHashError::BadChar { .. })
        ));
    }

    #[test]
    fn matches_known_blake3_vector() {
        // BLAKE3 of the empty input.
        let h = Hash::of(b"");
        assert_eq!(
            h.to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
