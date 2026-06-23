//! APFS case-/normalization-collision detection for the FSKit backend (issue
//! #7, spec §41).
//!
//! Two repository paths that Git treats as distinct byte strings can **collide**
//! on a macOS volume: a default APFS volume is case-insensitive *and*
//! normalization-insensitive, so `README` vs `readme`, or an NFC `é` vs an NFD
//! `e`+combining-accent, fold to the same on-disk name. The backend must
//! **surface** such collisions rather than silently merging the entries, and
//! must preserve the **exact bytes** Git recorded (`RepoPath` is an arbitrary
//! byte string the APFS path APIs cannot always represent verbatim).
//!
//! This module reuses the platform-level folding
//! ([`glm_platform::validate::macos_collision_key`]) — the same logic exercised
//! by the real-FS test on the macOS CI runner — and groups a directory's sibling
//! names accordingly. It is volume-aware: it works on both case-insensitive and
//! case-sensitive APFS volumes.

use glm_platform::validate::macos_collision_key;
pub use glm_platform::validate::AppleVolume;

/// A set of sibling names that collide under a volume's folding. The `names` are
/// the **exact recorded bytes** (never re-encoded), so the caller can report the
/// ambiguity precisely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision {
    /// The shared folding key the names collapse to.
    pub key: Vec<u8>,
    /// The distinct recorded names that fold to `key` (≥ 2, sorted).
    pub names: Vec<Vec<u8>>,
}

/// Group `names` (a directory's siblings, exact recorded bytes) into the sets
/// that collide on `volume`. Only groups with more than one distinct member are
/// returned. Deterministic: groups and members are sorted.
pub fn detect(names: &[Vec<u8>], volume: AppleVolume) -> Vec<Collision> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for n in names {
        let key = macos_collision_key(n, volume);
        let bucket = groups.entry(key).or_default();
        if !bucket.contains(n) {
            bucket.push(n.clone());
        }
    }
    groups
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(key, mut names)| {
            names.sort();
            Collision { key, names }
        })
        .collect()
}

/// The outcome of resolving a looked-up `name` against a directory's siblings on
/// a macOS volume. Normalization- and case-insensitivity mean a name may resolve
/// to a sibling whose recorded bytes differ (NFC vs NFD, `readme` vs `README`);
/// that is a successful *fuzzy* match, **not** a collision. A collision is only
/// when two or more *distinct* Git entries fold to the same key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolve {
    /// No sibling folds to `name`.
    NotFound,
    /// Exactly one sibling matches; its **exact recorded bytes** are returned so
    /// identity is preserved.
    Unique(Vec<u8>),
    /// Two or more distinct siblings fold together — a genuine collision to
    /// surface rather than silently pick one.
    Collision(Vec<Vec<u8>>),
}

/// Resolve `name` against `siblings` (exact recorded bytes) on `volume`.
pub fn resolve(name: &[u8], siblings: &[Vec<u8>], volume: AppleVolume) -> Resolve {
    let key = macos_collision_key(name, volume);
    let mut matches: Vec<Vec<u8>> = siblings
        .iter()
        .filter(|s| macos_collision_key(s, volume) == key)
        .cloned()
        .collect();
    matches.sort();
    matches.dedup();
    match matches.len() {
        0 => Resolve::NotFound,
        1 => Resolve::Unique(matches.into_iter().next().unwrap()),
        _ => Resolve::Collision(matches),
    }
}

/// The distinct sibling names that `name` collides with on `volume` (excluding
/// `name`'s own exact bytes). Empty when `name` is unambiguous.
pub fn colliding_with(name: &[u8], siblings: &[Vec<u8>], volume: AppleVolume) -> Vec<Vec<u8>> {
    let key = macos_collision_key(name, volume);
    let mut out: Vec<Vec<u8>> = siblings
        .iter()
        .filter(|s| s.as_slice() != name && macos_collision_key(s, volume) == key)
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `a` and `b` are a **case-only** (more precisely, folding-only)
/// rename on `volume`: distinct recorded bytes that fold to the same key, e.g.
/// `a.txt` → `A.txt` on a case-insensitive volume. The destination "already
/// exists" by the volume's comparison rules, but identity is preserved via the
/// inode table (spec §19), so this is a legal rename rather than a clobber.
pub fn is_case_only_rename(a: &[u8], b: &[u8], volume: AppleVolume) -> bool {
    a != b && macos_collision_key(a, volume) == macos_collision_key(b, volume)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn case_collision_detected_on_insensitive_volume() {
        let names = vec![v("README"), v("readme"), v("LICENSE")];
        let groups = detect(&names, AppleVolume::CaseInsensitive);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].names, vec![v("README"), v("readme")]);
        // Case-sensitive volume: README/readme are distinct, no collision.
        assert!(detect(&names, AppleVolume::CaseSensitive).is_empty());
    }

    #[test]
    fn normalization_collision_detected_on_both_volumes() {
        let nfc = "café.txt".as_bytes().to_vec();
        let nfd = "cafe\u{301}.txt".as_bytes().to_vec();
        assert_ne!(nfc, nfd); // distinct bytes
        let names = vec![nfc.clone(), nfd.clone()];
        // Both APFS variants are normalization-insensitive.
        for vol in [AppleVolume::CaseInsensitive, AppleVolume::CaseSensitive] {
            let groups = detect(&names, vol);
            assert_eq!(groups.len(), 1, "{vol:?} should fold NFC/NFD together");
            // Exact recorded bytes are preserved in the report.
            assert!(groups[0].names.contains(&nfc));
            assert!(groups[0].names.contains(&nfd));
        }
    }

    #[test]
    fn resolve_fuzzy_matches_but_flags_real_collisions() {
        let nfc = "café".as_bytes().to_vec();
        let nfd = "cafe\u{301}".as_bytes().to_vec();

        // Only the NFD form exists; looking it up by the NFC form resolves to the
        // single recorded entry (its exact bytes), not a collision.
        let one = vec![nfd.clone()];
        assert_eq!(
            resolve(&nfc, &one, AppleVolume::CaseInsensitive),
            Resolve::Unique(nfd.clone())
        );

        // A case-insensitive lookup of a differently-cased name resolves uniquely.
        assert_eq!(
            resolve(b"readme", &[v("README")], AppleVolume::CaseInsensitive),
            Resolve::Unique(v("README"))
        );

        // Two distinct entries that fold together are a genuine collision.
        match resolve(
            b"README",
            &[v("README"), v("readme")],
            AppleVolume::CaseInsensitive,
        ) {
            Resolve::Collision(names) => assert_eq!(names, vec![v("README"), v("readme")]),
            other => panic!("expected collision, got {other:?}"),
        }

        // Nothing folds to it -> not found.
        assert_eq!(
            resolve(b"missing", &[v("README")], AppleVolume::CaseInsensitive),
            Resolve::NotFound
        );
    }

    #[test]
    fn colliding_with_excludes_self() {
        let siblings = vec![v("Foo"), v("foo"), v("bar")];
        assert_eq!(
            colliding_with(b"Foo", &siblings, AppleVolume::CaseInsensitive),
            vec![v("foo")]
        );
        assert!(colliding_with(b"bar", &siblings, AppleVolume::CaseInsensitive).is_empty());
    }

    // Real-FS validation on the macOS CI runner / host: the resolver must agree
    // with how the actual volume folds names (issue #7).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_real_fs_case_behavior_matches_resolver() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo"), b"x").unwrap();
        // Probe the real volume's case behavior.
        let case_insensitive = std::fs::metadata(dir.path().join("foo")).is_ok();
        let volume = if case_insensitive {
            AppleVolume::CaseInsensitive
        } else {
            AppleVolume::CaseSensitive
        };
        // The resolver agrees: a differently-cased lookup resolves to the recorded
        // entry exactly when the real volume is case-insensitive.
        match resolve(b"foo", &[v("Foo")], volume) {
            Resolve::Unique(bytes) => {
                assert!(
                    case_insensitive,
                    "resolver folded case on a case-sensitive volume"
                );
                assert_eq!(bytes, v("Foo"), "must resolve to the exact recorded bytes");
            }
            Resolve::NotFound => assert!(
                !case_insensitive,
                "resolver missed a case-insensitive match the volume makes"
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    // Real-FS validation: a default APFS volume is normalization-insensitive, so
    // an NFD-created file is reachable by its NFC name and our resolver agrees.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_real_fs_normalization_matches_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let nfd = "cafe\u{301}.txt";
        let nfc = "café.txt";
        std::fs::write(dir.path().join(nfd), b"x").unwrap();
        if std::fs::metadata(dir.path().join(nfc)).is_ok() {
            // Normalization-insensitive volume: NFC resolves to the NFD entry.
            assert_eq!(
                resolve(nfc.as_bytes(), &[v(nfd)], AppleVolume::CaseInsensitive),
                Resolve::Unique(v(nfd))
            );
        }
    }

    #[test]
    fn case_only_rename_recognized() {
        assert!(is_case_only_rename(
            b"a.txt",
            b"A.txt",
            AppleVolume::CaseInsensitive
        ));
        // Not case-only on a case-sensitive volume (a genuine distinct name).
        assert!(!is_case_only_rename(
            b"a.txt",
            b"A.txt",
            AppleVolume::CaseSensitive
        ));
        // Distinct names that don't fold together are not a case-only rename.
        assert!(!is_case_only_rename(
            b"a.txt",
            b"b.txt",
            AppleVolume::CaseInsensitive
        ));
    }
}
