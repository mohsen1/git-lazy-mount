//! macOS-injected metadata commit policy (issue #8, spec §41).
//!
//! macOS attaches metadata that Git does not model: Finder databases
//! (`.DS_Store`), AppleDouble resource-fork sidecars (`._*`), extended attributes
//! (`com.apple.*`, the Finder-info / resource-fork xattrs, the quarantine flag),
//! and BSD file flags. Spec §41 requires these are **never silently committed**
//! as Git content. This module is the single, documented **policy table** and the
//! classifier the engine consults to enforce it.
//!
//! Each category resolves to one [`Disposition`]:
//!
//! | Category            | Examples                                   | Disposition   |
//! |---------------------|--------------------------------------------|---------------|
//! | Finder metadata     | `.DS_Store`                                | [`Ignored`]   |
//! | AppleDouble forks   | `._*`                                      | [`Ignored`]   |
//! | Resource-fork xattr | `com.apple.ResourceFork`                   | [`OverlayOnly`] |
//! | Finder-info xattr   | `com.apple.FinderInfo`                     | [`OverlayOnly`] |
//! | Quarantine xattr    | `com.apple.quarantine`                     | [`OverlayOnly`] |
//! | Other xattrs        | any `user.*` / `com.apple.*` / …           | [`OverlayOnly`] |
//! | BSD file flags      | `UF_HIDDEN`, `UF_IMMUTABLE`, …             | [`OverlayOnly`] |
//!
//! Enforcement (verified by tests against the staged tree):
//!
//! * [`Ignored`] paths are screened out of staging ([`is_never_committed_path`]),
//!   so `.DS_Store` / `._*` can never reach a staged tree or commit — even via
//!   `add -A` or the git-interop bridge.
//! * [`OverlayOnly`] categories (xattrs, resource forks, file flags) have **no
//!   commit channel at all**: the engine serializes only Git blob content plus
//!   the type/exec mode into a tree, so they are structurally never committed.
//! * [`Rejected`] is reserved for inputs the write boundary must refuse outright.
//!
//! [`Ignored`]: Disposition::Ignored
//! [`OverlayOnly`]: Disposition::OverlayOnly
//! [`Rejected`]: Disposition::Rejected

use glm_core::RepoPath;

/// A category of macOS-injected metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacMetadataKind {
    /// Finder directory metadata (`.DS_Store`).
    FinderMetadata,
    /// AppleDouble resource-fork sidecar file (`._name`).
    AppleDouble,
    /// The resource-fork extended attribute (`com.apple.ResourceFork`).
    ResourceFork,
    /// The Finder-info extended attribute (`com.apple.FinderInfo`).
    FinderInfo,
    /// The download quarantine flag (`com.apple.quarantine`).
    Quarantine,
    /// Any other extended attribute.
    ExtendedAttribute,
    /// BSD file flags (`st_flags`: `UF_HIDDEN`, `UF_IMMUTABLE`, …).
    FileFlags,
}

impl MacMetadataKind {
    /// A stable label for diagnostics / JSON.
    pub fn label(&self) -> &'static str {
        match self {
            MacMetadataKind::FinderMetadata => "finder_metadata",
            MacMetadataKind::AppleDouble => "apple_double",
            MacMetadataKind::ResourceFork => "resource_fork",
            MacMetadataKind::FinderInfo => "finder_info",
            MacMetadataKind::Quarantine => "quarantine",
            MacMetadataKind::ExtendedAttribute => "extended_attribute",
            MacMetadataKind::FileFlags => "file_flags",
        }
    }
}

/// What the engine does with a category of macOS metadata (spec §41).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Hidden from the committable working tree: never staged, never committed.
    Ignored,
    /// Persisted locally for fidelity, but never leaks into a staged tree or
    /// commit (no Git channel exists for it).
    OverlayOnly,
    /// Refused at the write boundary.
    Rejected,
}

impl Disposition {
    /// A stable label for diagnostics / JSON.
    pub fn label(&self) -> &'static str {
        match self {
            Disposition::Ignored => "ignored",
            Disposition::OverlayOnly => "overlay_only",
            Disposition::Rejected => "rejected",
        }
    }

    /// Whether content under this disposition may ever appear in a commit.
    /// Always `false` — the whole point of the policy (spec §41).
    pub fn is_committable(&self) -> bool {
        false
    }
}

/// Classify a working-tree **path** as macOS-injected metadata, if it is one.
/// Looks only at the final component, so the same name nested anywhere is caught
/// (`a/b/.DS_Store`, `a/._b`).
pub fn classify_path(path: &RepoPath) -> Option<(MacMetadataKind, Disposition)> {
    let name = path.file_name()?;
    if name == b".DS_Store" {
        return Some((MacMetadataKind::FinderMetadata, Disposition::Ignored));
    }
    // AppleDouble sidecars are `._<name>` (but not the parent refs `.`/`..`,
    // which `RepoPath` already rejects as components).
    if name.starts_with(b"._") {
        return Some((MacMetadataKind::AppleDouble, Disposition::Ignored));
    }
    None
}

/// Classify an **extended-attribute name**. Every xattr is `OverlayOnly`: Git has
/// no channel to commit extended attributes, so they are persisted locally only.
pub fn classify_xattr(name: &str) -> (MacMetadataKind, Disposition) {
    let kind = match name {
        "com.apple.ResourceFork" => MacMetadataKind::ResourceFork,
        "com.apple.FinderInfo" => MacMetadataKind::FinderInfo,
        "com.apple.quarantine" => MacMetadataKind::Quarantine,
        _ => MacMetadataKind::ExtendedAttribute,
    };
    (kind, Disposition::OverlayOnly)
}

/// The disposition of BSD file flags. `OverlayOnly`: Git tracks only the type and
/// the executable bit, never `st_flags`.
pub fn file_flags_disposition() -> (MacMetadataKind, Disposition) {
    (MacMetadataKind::FileFlags, Disposition::OverlayOnly)
}

/// Whether a path is macOS-injected metadata that must never reach a commit.
/// This is the predicate the staging path uses to screen `.DS_Store` / `._*`.
pub fn is_never_committed_path(path: &RepoPath) -> bool {
    matches!(
        classify_path(path),
        Some((_, Disposition::Ignored)) | Some((_, Disposition::OverlayOnly))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn ds_store_anywhere_is_ignored() {
        assert_eq!(
            classify_path(&p(".DS_Store")),
            Some((MacMetadataKind::FinderMetadata, Disposition::Ignored))
        );
        assert_eq!(
            classify_path(&p("src/ui/.DS_Store")),
            Some((MacMetadataKind::FinderMetadata, Disposition::Ignored))
        );
        assert!(is_never_committed_path(&p("src/ui/.DS_Store")));
    }

    #[test]
    fn appledouble_sidecars_are_ignored() {
        assert_eq!(
            classify_path(&p("._resource")),
            Some((MacMetadataKind::AppleDouble, Disposition::Ignored))
        );
        assert_eq!(
            classify_path(&p("a/b/._c")),
            Some((MacMetadataKind::AppleDouble, Disposition::Ignored))
        );
    }

    #[test]
    fn ordinary_files_are_not_metadata() {
        assert_eq!(classify_path(&p("README.md")), None);
        assert_eq!(classify_path(&p(".gitignore")), None);
        // A dotfile that merely starts with one dot is not AppleDouble.
        assert_eq!(classify_path(&p(".env")), None);
        assert!(!is_never_committed_path(&p("README.md")));
    }

    #[test]
    fn every_xattr_is_overlay_only_never_committable() {
        for name in [
            "com.apple.ResourceFork",
            "com.apple.FinderInfo",
            "com.apple.quarantine",
            "com.apple.metadata:kMDItemWhereFroms",
            "user.custom",
        ] {
            let (_, disp) = classify_xattr(name);
            assert_eq!(disp, Disposition::OverlayOnly);
            assert!(!disp.is_committable());
        }
        assert_eq!(
            classify_xattr("com.apple.ResourceFork").0,
            MacMetadataKind::ResourceFork
        );
    }

    #[test]
    fn file_flags_are_overlay_only() {
        assert_eq!(file_flags_disposition().1, Disposition::OverlayOnly);
    }
}
