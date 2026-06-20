//! The kernel's content-address: a 32-byte BLAKE3 digest.

use std::fmt;
use std::str::FromStr;

/// A content-address: the BLAKE3 digest of a canonical byte encoding.
///
/// This is the kernel's own id type. It is deliberately id-agnostic — it knows
/// nothing of `BlobId`/`TreeId` or any domain newtype, which are layered on top
/// elsewhere. Named `CASHash` rather than `Hash` so it never reads ambiguously
/// against [`std::hash::Hash`] (which it also implements, by hand — see below).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CASHash([u8; 32]);

impl CASHash {
    /// The BLAKE3 digest of `bytes`.
    pub fn of(bytes: &[u8]) -> CASHash {
        CASHash(*blake3::hash(bytes).as_bytes())
    }

    /// Construct from raw digest bytes (no hashing performed).
    pub const fn from_bytes(bytes: [u8; 32]) -> CASHash {
        CASHash(bytes)
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

/// Hand-written rather than derived: a `CASHash` is *already* a uniformly
/// distributed BLAKE3 digest, so its leading eight bytes are themselves a
/// perfectly good hash. Feeding just those to the [`Hasher`] avoids re-mixing
/// all 32 bytes on every map lookup. Equal digests share their leading bytes,
/// so the `k == k' ⟹ hash(k) == hash(k')` invariant holds.
///
/// [`Hasher`]: std::hash::Hasher
impl std::hash::Hash for CASHash {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(u64::from_ne_bytes(
            self.0[..8].try_into().expect("a CASHash is 32 bytes"),
        ));
    }
}

/// Hex on both `Display` and `Debug` so logs and errors are readable.
impl fmt::Display for CASHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for CASHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CASHash({})", self.to_hex())
    }
}

/// The error returned when a hex string cannot be parsed into a [`CASHash`].
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

impl FromStr for CASHash {
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
        Ok(CASHash(out))
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
        assert_eq!(CASHash::of(b"hello"), CASHash::of(b"hello"));
        assert_ne!(CASHash::of(b"hello"), CASHash::of(b"world"));
    }

    #[test]
    fn hex_roundtrip() {
        let h = CASHash::of(b"some content");
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        let parsed: CASHash = s.parse().unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn parse_rejects_bad_length() {
        assert_eq!(
            "abc".parse::<CASHash>(),
            Err(ParseHashError::BadLength { got: 3 })
        );
    }

    #[test]
    fn parse_rejects_bad_char() {
        let bad = "z".repeat(64);
        assert!(matches!(
            bad.parse::<CASHash>(),
            Err(ParseHashError::BadChar { .. })
        ));
    }

    #[test]
    fn matches_known_blake3_vector() {
        // BLAKE3 of the empty input.
        let h = CASHash::of(b"");
        assert_eq!(
            h.to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
