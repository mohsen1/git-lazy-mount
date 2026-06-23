//! Platform path representation and validation (spec §30).
//!
//! Repository paths are canonical Git bytes ([`RepoPath`] already rejects NUL,
//! absolute, traversal, and empty components and preserves arbitrary non-UTF-8
//! bytes). This module adds *platform representability*: which names a target OS
//! cannot store faithfully, and how sibling names collide under a platform's
//! case-/normalization-folding. It also provides the four collision policies
//! (spec §30) and a deterministic, reversible, collision-free escape.
//!
//! The rules here are validated against the **real** Windows and macOS
//! filesystems by platform-gated tests (run on the CI runners), not just
//! asserted in the abstract.

use glm_core::RepoPath;
use unicode_normalization::UnicodeNormalization;

/// A target platform whose path rules we are checking against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetPlatform {
    /// Linux: preserves arbitrary non-NUL bytes; case- and normalization-sensitive.
    Linux,
    /// macOS (default APFS): case-insensitive and normalization-insensitive.
    Macos,
    /// Windows (NTFS): case-insensitive; reserved names; forbidden characters.
    Windows,
}

impl TargetPlatform {
    /// The platform this binary is running on.
    pub fn current() -> TargetPlatform {
        #[cfg(target_os = "windows")]
        {
            TargetPlatform::Windows
        }
        #[cfg(target_os = "macos")]
        {
            TargetPlatform::Macos
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            TargetPlatform::Linux
        }
    }
}

/// A way a single path component is not faithfully representable on a platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathIssue {
    /// A Windows reserved device name (CON, NUL, COM1, …), possibly with an
    /// extension (`CON.txt`).
    ReservedName(String),
    /// A character forbidden on the platform.
    ForbiddenChar(char),
    /// A control character (0x01–0x1F).
    ControlChar(u8),
    /// A trailing `.` (silently stripped by Windows).
    TrailingDot,
    /// A trailing space (silently stripped by Windows).
    TrailingSpace,
    /// Alternate-data-stream syntax (`name:stream`) on Windows.
    AlternateDataStream,
    /// The component is too long for the platform.
    TooLong,
    /// The bytes are not valid UTF-8 and cannot map to the platform's native
    /// (UTF-16) path encoding.
    NotUtf8,
}

/// The policy for handling unrepresentable / colliding entries (spec §30).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPolicy {
    /// Mount without a full scan; fail the specific directory op on discovery.
    FailOnDiscovery,
    /// Traverse all trees before mounting and fail before projection
    /// (`O(repository entries)`).
    Preflight,
    /// Hide unrepresentable entries; report them via status/diagnostics.
    Hide,
    /// Map unrepresentable names through a deterministic reversible namespace.
    Escape,
}

/// The reserved prefix for the tool's own control namespace (spec §30: reserve
/// it and handle a repository entry that collides).
pub const CONTROL_PREFIX: &str = ".glm-";

const WINDOWS_FORBIDDEN: &[u8] = b"<>:\"|?*\\";

fn windows_reserved_stem(stem_upper: &str) -> bool {
    matches!(stem_upper, "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || (stem_upper.len() == 4
            && (stem_upper.starts_with("COM") || stem_upper.starts_with("LPT"))
            && stem_upper.as_bytes()[3].is_ascii_digit()
            && stem_upper.as_bytes()[3] != b'0')
}

/// Check one path component for representability issues on `target`.
pub fn check_component(name: &[u8], target: TargetPlatform) -> Vec<PathIssue> {
    let mut issues = Vec::new();
    match target {
        TargetPlatform::Linux => {
            // Linux preserves any non-NUL byte; RepoPath already enforces NUL/
            // separator/traversal rules, so nothing further is unrepresentable.
        }
        TargetPlatform::Macos => {
            // macOS allows almost any byte except `/` and NUL; collisions
            // (case/normalization) are handled separately. Overlong components
            // (>255 bytes) are still a problem.
            if name.len() > 255 {
                issues.push(PathIssue::TooLong);
            }
        }
        TargetPlatform::Windows => {
            if std::str::from_utf8(name).is_err() {
                issues.push(PathIssue::NotUtf8);
            }
            for &b in name {
                if b < 0x20 {
                    issues.push(PathIssue::ControlChar(b));
                } else if WINDOWS_FORBIDDEN.contains(&b) {
                    if b == b':' {
                        issues.push(PathIssue::AlternateDataStream);
                    } else {
                        issues.push(PathIssue::ForbiddenChar(b as char));
                    }
                }
            }
            match name.last() {
                Some(b'.') => issues.push(PathIssue::TrailingDot),
                Some(b' ') => issues.push(PathIssue::TrailingSpace),
                _ => {}
            }
            // Reserved device name: the stem before the first '.'.
            if let Ok(s) = std::str::from_utf8(name) {
                let stem = s.split('.').next().unwrap_or(s);
                let stem_upper = stem.trim_end().to_ascii_uppercase();
                if windows_reserved_stem(&stem_upper) {
                    issues.push(PathIssue::ReservedName(stem_upper));
                }
            }
            if name.len() > 255 {
                issues.push(PathIssue::TooLong);
            }
        }
    }
    issues
}

/// Whether a component is representable on `target` (no issues).
pub fn is_representable(name: &[u8], target: TargetPlatform) -> bool {
    check_component(name, target).is_empty()
}

/// Per-component issues across a whole path; empty if fully representable.
pub fn check_path(path: &RepoPath, target: TargetPlatform) -> Vec<(Vec<u8>, Vec<PathIssue>)> {
    path.components()
        .filter_map(|c| {
            let issues = check_component(c, target);
            if issues.is_empty() {
                None
            } else {
                Some((c.to_vec(), issues))
            }
        })
        .collect()
}

fn fold_case(name: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(name) {
        Ok(s) => s
            .chars()
            .flat_map(|c| c.to_lowercase())
            .collect::<String>()
            .into_bytes(),
        Err(_) => name.to_ascii_lowercase(),
    }
}

fn nfc(name: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(name) {
        Ok(s) => s.nfc().collect::<String>().into_bytes(),
        Err(_) => name.to_vec(),
    }
}

/// The folding key under which two sibling names *collide* on `target`:
/// identity on Linux, case-folded on Windows, case-folded + NFC on macOS.
pub fn collision_key(name: &[u8], target: TargetPlatform) -> Vec<u8> {
    match target {
        TargetPlatform::Linux => name.to_vec(),
        TargetPlatform::Windows => fold_case(name),
        TargetPlatform::Macos => fold_case(&nfc(name)),
    }
}

/// Group sibling names that collide on `target`. Returns the index groups with
/// more than one member.
pub fn detect_collisions(names: &[Vec<u8>], target: TargetPlatform) -> Vec<Vec<usize>> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<Vec<u8>, Vec<usize>> = BTreeMap::new();
    for (i, n) in names.iter().enumerate() {
        groups.entry(collision_key(n, target)).or_default().push(i);
    }
    groups.into_values().filter(|g| g.len() > 1).collect()
}

/// Deterministic, reversible, collision-free escape of a component for `target`.
///
/// Percent-encodes every byte that is problematic on the target (plus `%`
/// itself, guaranteeing injectivity). Reserved device names and trailing
/// dot/space are neutralized by additionally encoding the offending boundary
/// character. Round-trips via [`unescape`].
pub fn platform_escape(name: &[u8], target: TargetPlatform) -> String {
    let mut out = String::with_capacity(name.len());
    let n = name.len();
    for (i, &b) in name.iter().enumerate() {
        let problematic = b == b'%'
            || match target {
                TargetPlatform::Windows => {
                    b < 0x20
                        || WINDOWS_FORBIDDEN.contains(&b)
                        || (i + 1 == n && (b == b'.' || b == b' '))
                }
                TargetPlatform::Macos | TargetPlatform::Linux => false,
            };
        if problematic {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    // Neutralize a reserved device name by encoding its first byte.
    if target == TargetPlatform::Windows {
        let issues = check_component(name, target);
        if issues
            .iter()
            .any(|i| matches!(i, PathIssue::ReservedName(_)))
            && !out.is_empty()
        {
            let first = name[0];
            out = format!("%{first:02X}{}", &out[1..]);
        }
    }
    out
}

/// Inverse of [`platform_escape`].
pub fn unescape(s: &str) -> Option<Vec<u8>> {
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return None;
            }
            let hi = (raw[i + 1] as char).to_digit(16)?;
            let lo = (raw[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_reserved_and_forbidden() {
        let w = TargetPlatform::Windows;
        assert!(check_component(b"CON", w)
            .iter()
            .any(|i| matches!(i, PathIssue::ReservedName(_))));
        assert!(check_component(b"con.txt", w)
            .iter()
            .any(|i| matches!(i, PathIssue::ReservedName(_))));
        assert!(check_component(b"COM1", w)
            .iter()
            .any(|i| matches!(i, PathIssue::ReservedName(_))));
        // COM0 is not reserved.
        assert!(!check_component(b"COM0", w)
            .iter()
            .any(|i| matches!(i, PathIssue::ReservedName(_))));
        assert!(check_component(b"a<b", w)
            .iter()
            .any(|i| matches!(i, PathIssue::ForbiddenChar('<'))));
        assert!(check_component(b"a:b", w).contains(&PathIssue::AlternateDataStream));
        assert!(check_component(b"trail.", w).contains(&PathIssue::TrailingDot));
        assert!(check_component(b"trail ", w).contains(&PathIssue::TrailingSpace));
        // A normal name is representable.
        assert!(is_representable(b"checker.rs", w));
    }

    #[test]
    fn linux_preserves_everything() {
        // Arbitrary bytes (including ones forbidden on Windows) are fine on Linux.
        assert!(is_representable(b"a:b<c>|", TargetPlatform::Linux));
        assert!(is_representable(&[0xff, 0xfe], TargetPlatform::Linux));
    }

    #[test]
    fn macos_case_and_normalization_collisions() {
        let m = TargetPlatform::Macos;
        // Case-insensitive.
        assert_eq!(collision_key(b"README", m), collision_key(b"readme", m));
        // Normalization-insensitive: NFC "é" vs NFD "e+combining acute".
        let nfc_e = "é.txt".as_bytes();
        let nfd_e = "e\u{301}.txt".as_bytes();
        assert_ne!(nfc_e, nfd_e); // different bytes
        assert_eq!(collision_key(nfc_e, m), collision_key(nfd_e, m));
        // On Linux they do NOT collide.
        assert_ne!(
            collision_key(nfc_e, TargetPlatform::Linux),
            collision_key(nfd_e, TargetPlatform::Linux)
        );
    }

    #[test]
    fn detect_collisions_groups() {
        let names = vec![b"Foo".to_vec(), b"foo".to_vec(), b"bar".to_vec()];
        let groups = detect_collisions(&names, TargetPlatform::Windows);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
        // Case-sensitive Linux: no collision.
        assert!(detect_collisions(&names, TargetPlatform::Linux).is_empty());
    }

    #[test]
    fn escape_roundtrips_and_neutralizes() {
        let w = TargetPlatform::Windows;
        for name in [
            &b"a:b"[..],
            &b"trail."[..],
            &b"trail "[..],
            &b"CON"[..],
            &b"a<b>c"[..],
            &b"100%real"[..],
            &b"normal.rs"[..],
        ] {
            let esc = platform_escape(name, w);
            // The escaped form is representable on Windows.
            assert!(
                is_representable(esc.as_bytes(), w),
                "escaped {:?} -> {esc:?} still not representable",
                String::from_utf8_lossy(name)
            );
            // And it round-trips exactly.
            assert_eq!(unescape(&esc).as_deref(), Some(name));
        }
    }

    // ---- real-OS-validated tests (run on the CI runners) ----

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_real_fs_preserves_non_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let raw = [b'a', 0xff, 0xfe, b'z'];
        let name = std::ffi::OsStr::from_bytes(&raw);
        let path = dir.path().join(name);
        std::fs::write(&path, b"x").unwrap();
        // The directory entry round-trips byte-for-byte.
        let got: Vec<u8> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().as_bytes().to_vec())
            .next()
            .unwrap();
        assert_eq!(got, raw);
        assert!(is_representable(&raw, TargetPlatform::Linux));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_real_fs_is_normalization_insensitive() {
        // Create a file under its NFD name, then open it by its NFC name.
        let dir = tempfile::tempdir().unwrap();
        let nfd = "e\u{301}.txt"; // e + combining acute
        let nfc = "é.txt";
        std::fs::write(dir.path().join(nfd), b"x").unwrap();
        // APFS (default) resolves the NFC name to the same file.
        let by_nfc = std::fs::metadata(dir.path().join(nfc));
        assert!(
            by_nfc.is_ok(),
            "macOS should resolve NFC/NFD to the same file"
        );
        // Our collision key agrees that they collide on macOS.
        assert_eq!(
            collision_key(nfc.as_bytes(), TargetPlatform::Macos),
            collision_key(nfd.as_bytes(), TargetPlatform::Macos)
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_real_fs_strips_trailing_dot_and_handles_ads() {
        let dir = tempfile::tempdir().unwrap();
        // Windows strips a trailing dot: the created entry is "a", not "a.".
        std::fs::write(dir.path().join("a."), b"x").unwrap();
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.iter().any(|n| n == "a") && !entries.iter().any(|n| n == "a."),
            "Windows should have stored 'a.' as 'a'; entries={entries:?}"
        );
        // Our checker flags the trailing dot, so we would escape it instead.
        assert!(check_component(b"a.", TargetPlatform::Windows).contains(&PathIssue::TrailingDot));
        let esc = platform_escape(b"a.", TargetPlatform::Windows);
        std::fs::write(dir.path().join(&esc), b"y").unwrap();
        let after: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // The escaped name is stored verbatim (no stripping) and round-trips.
        assert!(after.iter().any(|n| n == &esc));
        assert_eq!(unescape(&esc).as_deref(), Some(&b"a."[..]));
    }
}
