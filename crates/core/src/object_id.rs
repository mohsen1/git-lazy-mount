//! Git object identity, abstracted over the repository's object format.
//!
//! Requirement: never assume SHA-1 or 40-character object IDs.
//! Object IDs are opaque byte strings tagged with the format Git reported for
//! the repository. We parse and compare them; we never compute them ourselves
//! (Git remains authoritative for hashing).

use std::fmt;

use serde::{Deserialize, Serialize};

/// The hash algorithm a repository uses for its object names.
///
/// We deliberately keep an `Other` arm so that a future Git object format does
/// not require a code change to *parse* (only to optimize).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectFormat {
    /// 160-bit SHA-1 (40 hex chars). The historical default.
    Sha1,
    /// 256-bit SHA-256 (64 hex chars). Git's transition hash.
    Sha256,
    /// Any other format name as reported by `git rev-parse --show-object-format`.
    Other(String),
}

impl ObjectFormat {
    /// Parse the value Git prints for `--show-object-format`.
    pub fn parse(name: &str) -> Self {
        match name.trim() {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            other => ObjectFormat::Other(other.to_string()),
        }
    }

    /// The canonical lowercase name Git uses for this format.
    pub fn name(&self) -> &str {
        match self {
            ObjectFormat::Sha1 => "sha1",
            ObjectFormat::Sha256 => "sha256",
            ObjectFormat::Other(s) => s.as_str(),
        }
    }

    /// Expected raw digest length in bytes for known formats.
    ///
    /// Returns `None` for unknown formats, in which case length is whatever the
    /// parsed hex implied — we do not enforce a size we do not know.
    pub fn raw_len(&self) -> Option<usize> {
        match self {
            ObjectFormat::Sha1 => Some(20),
            ObjectFormat::Sha256 => Some(32),
            ObjectFormat::Other(_) => None,
        }
    }
}

/// An opaque, format-tagged Git object name.
///
/// Stored as raw bytes (not hex) so equality and hashing are cheap and exact.
/// Use [`ObjectId::to_hex`] for display and [`ObjectId::parse_hex`] to build one.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectId {
    /// The format this id belongs to. Two ids of different formats are never
    /// equal even if their bytes coincide.
    pub format: ObjectFormat,
    /// Raw digest bytes.
    pub bytes: Vec<u8>,
}

impl ObjectId {
    /// Parse a hex object name in the given format.
    ///
    /// Validates that the hex decodes and, for known formats, that the length
    /// matches. Rejects non-hex input.
    pub fn parse_hex(format: ObjectFormat, hex_str: &str) -> Result<Self, ObjectIdParseError> {
        let hex_str = hex_str.trim();
        let bytes = hex::decode(hex_str).map_err(|_| ObjectIdParseError::NotHex)?;
        if let Some(expected) = format.raw_len() {
            if bytes.len() != expected {
                return Err(ObjectIdParseError::WrongLength {
                    expected,
                    actual: bytes.len(),
                });
            }
        }
        if bytes.is_empty() {
            return Err(ObjectIdParseError::Empty);
        }
        Ok(ObjectId { format, bytes })
    }

    /// Lowercase hex representation, as Git prints it.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.bytes)
    }

    /// Whether this id is the all-zero "null" oid for its format (used by Git
    /// to denote "no object", e.g. the old value of a ref being created).
    pub fn is_null(&self) -> bool {
        !self.bytes.is_empty() && self.bytes.iter().all(|&b| b == 0)
    }

    /// Construct the null oid for a format (used as the compare-and-swap
    /// "expected old value" when creating a new ref).
    pub fn null(format: ObjectFormat) -> Self {
        let len = format.raw_len().unwrap_or(20);
        ObjectId {
            format,
            bytes: vec![0u8; len],
        }
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({}:{})", self.format.name(), self.to_hex())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Failure to parse an [`ObjectId`] from hex.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ObjectIdParseError {
    /// Input was not valid hexadecimal.
    #[error("object id is not valid hex")]
    NotHex,
    /// Input decoded to the wrong length for a known format.
    #[error("object id has wrong length: expected {expected} bytes, got {actual}")]
    WrongLength {
        /// Bytes required by the format.
        expected: usize,
        /// Bytes actually decoded.
        actual: usize,
    },
    /// Input decoded to zero bytes.
    #[error("object id is empty")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_roundtrip() {
        let hexs = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"; // empty blob sha1
        let oid = ObjectId::parse_hex(ObjectFormat::Sha1, hexs).unwrap();
        assert_eq!(oid.to_hex(), hexs);
        assert_eq!(oid.bytes.len(), 20);
        assert!(!oid.is_null());
    }

    #[test]
    fn sha256_length_enforced() {
        let too_short = "abcd";
        let err = ObjectId::parse_hex(ObjectFormat::Sha256, too_short).unwrap_err();
        assert!(matches!(err, ObjectIdParseError::WrongLength { .. }));
    }

    #[test]
    fn rejects_non_hex() {
        let err = ObjectId::parse_hex(ObjectFormat::Sha1, "zz").unwrap_err();
        assert_eq!(err, ObjectIdParseError::NotHex);
    }

    #[test]
    fn null_oid_detected() {
        let z = ObjectId::null(ObjectFormat::Sha1);
        assert!(z.is_null());
        assert_eq!(z.bytes.len(), 20);
    }

    #[test]
    fn different_formats_not_equal() {
        let a = ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![1, 2, 3],
        };
        let b = ObjectId {
            format: ObjectFormat::Sha256,
            bytes: vec![1, 2, 3],
        };
        assert_ne!(a, b);
    }

    #[test]
    fn unknown_format_skips_length_check() {
        let oid = ObjectId::parse_hex(ObjectFormat::Other("blake3".into()), "00112233").unwrap();
        assert_eq!(oid.bytes.len(), 4);
    }
}
