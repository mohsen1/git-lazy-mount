//! Canonical Git repository paths, stored as raw bytes.
//!
//! Spec §17 / §30: Git path bytes are never *implicitly* UTF-8. A `RepoPath`
//! preserves arbitrary non-NUL bytes (legal on Linux), uses `/` as the only
//! separator, and exposes *separate* APIs for identity (`as_bytes`), lossy
//! human display (`display`), and reversible escaping for logs/JSON
//! (`escape`/`unescape`). Lossy Unicode conversion is never used as an identity
//! key.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A canonical, validated Git path (relative to the repository root).
///
/// Invariants enforced by [`RepoPath::from_bytes`]:
/// * no NUL byte,
/// * not absolute (no leading `/`),
/// * no empty components (`a//b`),
/// * no `.` or `..` components (traversal),
/// * `/` is the separator.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath {
    bytes: Vec<u8>,
}

/// Reasons a byte string is not a valid [`RepoPath`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathError {
    /// Contained a NUL byte.
    #[error("path contains NUL")]
    ContainsNul,
    /// Began with `/` (absolute).
    #[error("path is absolute")]
    Absolute,
    /// Contained an empty component (e.g. `a//b` or a trailing slash).
    #[error("path contains an empty component")]
    EmptyComponent,
    /// Contained a `.` or `..` component.
    #[error("path contains a traversal component ('.' or '..')")]
    Traversal,
    /// A reversible escape string could not be decoded.
    #[error("invalid escaped path")]
    BadEscape,
}

impl RepoPath {
    /// The repository root (empty path).
    pub fn root() -> Self {
        RepoPath { bytes: Vec::new() }
    }

    /// Validate and construct from raw bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Ok(RepoPath { bytes });
        }
        if bytes.contains(&0) {
            return Err(PathError::ContainsNul);
        }
        if bytes[0] == b'/' {
            return Err(PathError::Absolute);
        }
        for component in bytes.split(|&b| b == b'/') {
            if component.is_empty() {
                return Err(PathError::EmptyComponent);
            }
            if component == b"." || component == b".." {
                return Err(PathError::Traversal);
            }
        }
        Ok(RepoPath { bytes })
    }

    /// Whether this is the repository root.
    pub fn is_root(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Raw canonical bytes. This is the identity of the path.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Iterate over path components as raw byte slices.
    ///
    /// Validated paths never contain empty components, so the `filter` only
    /// suppresses the single empty slice that `split` yields for the root.
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.bytes.split(|&b| b == b'/').filter(|c| !c.is_empty())
    }

    /// The final component (file/dir name), or `None` for the root.
    pub fn file_name(&self) -> Option<&[u8]> {
        if self.bytes.is_empty() {
            return None;
        }
        match self.bytes.iter().rposition(|&b| b == b'/') {
            Some(idx) => Some(&self.bytes[idx + 1..]),
            None => Some(&self.bytes),
        }
    }

    /// The parent path, or `None` for the root.
    pub fn parent(&self) -> Option<RepoPath> {
        if self.bytes.is_empty() {
            return None;
        }
        match self.bytes.iter().rposition(|&b| b == b'/') {
            Some(idx) => Some(RepoPath {
                bytes: self.bytes[..idx].to_vec(),
            }),
            None => Some(RepoPath::root()),
        }
    }

    /// Append a single component. The component is validated (no `/`, NUL,
    /// traversal, or emptiness).
    pub fn join(&self, component: &[u8]) -> Result<RepoPath, PathError> {
        if component.is_empty() {
            return Err(PathError::EmptyComponent);
        }
        if component.contains(&0) {
            return Err(PathError::ContainsNul);
        }
        if component.contains(&b'/') {
            return Err(PathError::EmptyComponent);
        }
        if component == b"." || component == b".." {
            return Err(PathError::Traversal);
        }
        let mut bytes = self.bytes.clone();
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(component);
        Ok(RepoPath { bytes })
    }

    /// Whether `self` is a strict or equal prefix directory of `other`.
    ///
    /// Root is a prefix of everything. `a/b` is a prefix of `a/b/c` but not of
    /// `a/bc`.
    pub fn is_prefix_of(&self, other: &RepoPath) -> bool {
        if self.bytes.is_empty() {
            return true;
        }
        if self.bytes == other.bytes {
            return true;
        }
        other.bytes.len() > self.bytes.len()
            && other.bytes.starts_with(&self.bytes)
            && other.bytes[self.bytes.len()] == b'/'
    }

    /// Lossy, human-readable rendering. **Not** an identity; do not parse back.
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Reversible, log/JSON-safe escaping.
    ///
    /// Printable ASCII passes through except `%` (escaped as `%25`). Everything
    /// else is percent-encoded byte-by-byte. Round-trips exactly via
    /// [`RepoPath::unescape`].
    pub fn escape(&self) -> String {
        let mut out = String::with_capacity(self.bytes.len());
        for &b in &self.bytes {
            if b == b'%' {
                out.push_str("%25");
            } else if (0x20..0x7f).contains(&b) {
                out.push(b as char);
            } else {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
        out
    }

    /// Inverse of [`RepoPath::escape`], then re-validates.
    pub fn unescape(s: &str) -> Result<Self, PathError> {
        let raw = s.as_bytes();
        let mut bytes = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' {
                if i + 2 >= raw.len() {
                    return Err(PathError::BadEscape);
                }
                let hi = hex_val(raw[i + 1]).ok_or(PathError::BadEscape)?;
                let lo = hex_val(raw[i + 2]).ok_or(PathError::BadEscape)?;
                bytes.push((hi << 4) | lo);
                i += 3;
            } else {
                bytes.push(raw[i]);
                i += 1;
            }
        }
        RepoPath::from_bytes(bytes)
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.escape())
    }
}

// Serialize via the reversible escape so non-UTF-8 paths survive JSON.
impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.escape())
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = RepoPath;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an escaped repo path string")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<RepoPath, E> {
                RepoPath::unescape(v).map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_str(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute() {
        assert_eq!(
            RepoPath::from_bytes(b"../etc".to_vec()).unwrap_err(),
            PathError::Traversal
        );
        assert_eq!(
            RepoPath::from_bytes(b"a/./b".to_vec()).unwrap_err(),
            PathError::Traversal
        );
        assert_eq!(
            RepoPath::from_bytes(b"/abs".to_vec()).unwrap_err(),
            PathError::Absolute
        );
        assert_eq!(
            RepoPath::from_bytes(b"a//b".to_vec()).unwrap_err(),
            PathError::EmptyComponent
        );
        assert_eq!(
            RepoPath::from_bytes(b"a\0b".to_vec()).unwrap_err(),
            PathError::ContainsNul
        );
    }

    #[test]
    fn preserves_non_utf8() {
        let p = RepoPath::from_bytes(vec![b'a', 0xff, 0xfe, b'/', b'b']).unwrap();
        assert_eq!(p.as_bytes(), &[b'a', 0xff, 0xfe, b'/', b'b']);
        // Escape round-trips exactly even though display is lossy.
        let esc = p.escape();
        assert_eq!(RepoPath::unescape(&esc).unwrap(), p);
        assert!(p.display().contains('\u{fffd}'));
    }

    #[test]
    fn components_and_parent() {
        let p = RepoPath::from_bytes(b"src/compiler/checker.rs".to_vec()).unwrap();
        let comps: Vec<&[u8]> = p.components().collect();
        assert_eq!(
            comps,
            vec![&b"src"[..], &b"compiler"[..], &b"checker.rs"[..]]
        );
        assert_eq!(p.file_name().unwrap(), b"checker.rs");
        assert_eq!(p.parent().unwrap().as_bytes(), b"src/compiler");
    }

    #[test]
    fn root_behaviour() {
        let r = RepoPath::root();
        assert!(r.is_root());
        assert_eq!(r.components().count(), 0);
        assert!(r.parent().is_none());
        assert!(r.is_prefix_of(&RepoPath::from_bytes(b"a/b".to_vec()).unwrap()));
    }

    #[test]
    fn prefix_semantics() {
        let a = RepoPath::from_bytes(b"a/b".to_vec()).unwrap();
        let abc = RepoPath::from_bytes(b"a/b/c".to_vec()).unwrap();
        let abx = RepoPath::from_bytes(b"a/bc".to_vec()).unwrap();
        assert!(a.is_prefix_of(&abc));
        assert!(!a.is_prefix_of(&abx));
        assert!(a.is_prefix_of(&a));
    }

    #[test]
    fn join_validates() {
        let a = RepoPath::from_bytes(b"a".to_vec()).unwrap();
        assert!(a.join(b"b").is_ok());
        assert_eq!(a.join(b"..").unwrap_err(), PathError::Traversal);
        assert_eq!(a.join(b"b/c").unwrap_err(), PathError::EmptyComponent);
    }

    #[test]
    fn escape_roundtrip_with_newline_and_tab() {
        let p = RepoPath::from_bytes(b"weird\tname\nhere".to_vec()).unwrap();
        let esc = p.escape();
        assert!(!esc.contains('\n'));
        assert!(!esc.contains('\t'));
        assert_eq!(RepoPath::unescape(&esc).unwrap(), p);
    }

    #[test]
    fn serde_json_roundtrip_non_utf8() {
        let p = RepoPath::from_bytes(vec![0xff, b'x']).unwrap();
        let j = serde_json::to_string(&p).unwrap();
        let back: RepoPath = serde_json::from_str(&j).unwrap();
        assert_eq!(p, back);
    }
}
