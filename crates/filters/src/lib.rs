//! `glm-filters` — working-tree filter policy, trust model, and cache keys
//! (spec §25, §46).
//!
//! The byte-level filtering itself is performed by Git's own plumbing
//! (`cat-file --filters` / `hash-object --path`, in `glm-git-store`) so that
//! projected clean content matches a real checkout. This crate decides *whether*
//! external filters are permitted (trust + mode), and computes the filtered-
//! content **cache key**, which must include every input that can change the
//! transformation (spec §25) so a stale cache entry is impossible.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, ObjectId, RepoId, RepoPath, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How working-tree filtering is performed (spec §25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilterMode {
    /// Match a normal checkout, including external clean/smudge drivers (only
    /// for trusted repositories).
    Faithful,
    /// Apply Git's built-in conversions (EOL, encoding, ident) but refuse
    /// external filter drivers.
    DenyExternal,
    /// No filtering at all — projected bytes are the raw blob. Must be selected
    /// explicitly and does NOT match a normal checkout (spec §25).
    Raw,
}

/// The decision for a specific blob/path given the mode and trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterDecision {
    /// Run Git's full working-tree filtering.
    RunGitFilters,
    /// Serve the raw blob with no filtering.
    RawOnly,
    /// Refuse: an external filter is configured but not permitted.
    Refuse,
}

/// Decide how to filter, given the mode, whether the repo is trusted, and
/// whether the path has an external `filter` driver configured.
pub fn decide(mode: FilterMode, trusted: bool, has_external_filter: bool) -> FilterDecision {
    match mode {
        FilterMode::Raw => FilterDecision::RawOnly,
        FilterMode::DenyExternal => {
            if has_external_filter {
                FilterDecision::Refuse
            } else {
                FilterDecision::RunGitFilters
            }
        }
        FilterMode::Faithful => {
            if has_external_filter && !trusted {
                FilterDecision::Refuse
            } else {
                FilterDecision::RunGitFilters
            }
        }
    }
}

/// Build the actionable error for a [`FilterDecision::Refuse`].
pub fn refusal_error(path: &RepoPath) -> Error {
    Error::new(
        ErrorCode::FilterFailure,
        format!(
            "path {} uses an external filter driver that is not permitted",
            path.escape()
        ),
    )
    .with_action("grant trust with `git lazy-mount trust grant`, or use --filters=raw")
}

/// Persistent record of which repositories the user trusts to run external
/// filters/hooks/commands (spec §46). Without trust, no repository-provided
/// code runs.
pub struct TrustStore {
    file: PathBuf,
    trusted: Mutex<BTreeSet<String>>,
}

#[derive(Default, Serialize, Deserialize)]
struct TrustData {
    trusted: BTreeSet<String>,
}

impl TrustStore {
    /// Open (or initialize) the trust store at `file`.
    pub fn open(file: impl Into<PathBuf>) -> Result<TrustStore> {
        let file = file.into();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let trusted = if file.exists() {
            let bytes = std::fs::read(&file)?;
            let data: TrustData = serde_json::from_slice(&bytes).map_err(|e| {
                Error::new(
                    ErrorCode::Configuration,
                    format!("corrupt trust store: {e}"),
                )
            })?;
            data.trusted
        } else {
            BTreeSet::new()
        };
        Ok(TrustStore {
            file,
            trusted: Mutex::new(trusted),
        })
    }

    /// Whether `repo` is trusted.
    pub fn is_trusted(&self, repo: &RepoId) -> bool {
        self.trusted.lock().unwrap().contains(&repo.0)
    }

    /// Grant trust to `repo`.
    pub fn grant(&self, repo: &RepoId) -> Result<()> {
        self.trusted.lock().unwrap().insert(repo.0.clone());
        self.persist()
    }

    /// Revoke trust from `repo`.
    pub fn revoke(&self, repo: &RepoId) -> Result<()> {
        self.trusted.lock().unwrap().remove(&repo.0);
        self.persist()
    }

    /// List trusted repository ids.
    pub fn list(&self) -> Vec<String> {
        self.trusted.lock().unwrap().iter().cloned().collect()
    }

    fn persist(&self) -> Result<()> {
        let data = TrustData {
            trusted: self.trusted.lock().unwrap().clone(),
        };
        let bytes = serde_json::to_vec_pretty(&data)
            .map_err(|e| Error::internal(format!("encode trust: {e}")))?;
        let dir = self.file.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.file)
            .map_err(|e| Error::internal(format!("persist trust: {e}")).with_source(e.error))?;
        Ok(())
    }
}

/// Inputs that determine a filtered-content representation. Every field that
/// can change the bytes is part of the cache key (spec §25), so changing
/// `.gitattributes`, EOL config, the attribute source, or the path invalidates
/// affected entries automatically.
#[derive(Clone, Debug)]
pub struct FilterContext<'a> {
    /// The raw blob object id.
    pub raw_blob: &'a ObjectId,
    /// The repository path (attributes are path-dependent).
    pub path: &'a RepoPath,
    /// Digest of the attribute source (e.g. base-commit tree id).
    pub attr_source: Option<&'a ObjectId>,
    /// Digest of the relevant Git configuration affecting filters.
    pub config_digest: &'a str,
    /// Identity of the filter command/process driver, if any.
    pub filter_identity: Option<&'a str>,
    /// Platform EOL mode marker.
    pub eol_mode: &'a str,
    /// Tool cache-format version.
    pub format_version: u32,
}

impl FilterContext<'_> {
    /// Compute the content-addressed cache key for this filtered representation.
    pub fn cache_key(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"glm-filtered-v");
        h.update(self.format_version.to_le_bytes());
        h.update(self.raw_blob.format.name().as_bytes());
        h.update(&self.raw_blob.bytes);
        h.update([0]);
        h.update(self.path.as_bytes());
        h.update([0]);
        if let Some(src) = self.attr_source {
            h.update(&src.bytes);
        }
        h.update([0]);
        h.update(self.config_digest.as_bytes());
        h.update([0]);
        if let Some(f) = self.filter_identity {
            h.update(f.as_bytes());
        }
        h.update([0]);
        h.update(self.eol_mode.as_bytes());
        hex::encode(h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::ObjectFormat;

    fn oid(b: u8) -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![b; 20],
        }
    }
    fn rp(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn decision_matrix() {
        // Raw never filters.
        assert_eq!(decide(FilterMode::Raw, true, true), FilterDecision::RawOnly);
        // Faithful runs filters when trusted, refuses untrusted external.
        assert_eq!(
            decide(FilterMode::Faithful, true, true),
            FilterDecision::RunGitFilters
        );
        assert_eq!(
            decide(FilterMode::Faithful, false, true),
            FilterDecision::Refuse
        );
        // No external filter: faithful runs built-ins regardless of trust.
        assert_eq!(
            decide(FilterMode::Faithful, false, false),
            FilterDecision::RunGitFilters
        );
        // DenyExternal refuses external drivers even if trusted.
        assert_eq!(
            decide(FilterMode::DenyExternal, true, true),
            FilterDecision::Refuse
        );
    }

    #[test]
    fn trust_store_persists() {
        let dir = tempfile::tempdir().unwrap();
        let repo = RepoId("github.com-o-r-abcdef".into());
        {
            let ts = TrustStore::open(dir.path().join("trust.json")).unwrap();
            assert!(!ts.is_trusted(&repo));
            ts.grant(&repo).unwrap();
            assert!(ts.is_trusted(&repo));
        }
        let ts = TrustStore::open(dir.path().join("trust.json")).unwrap();
        assert!(ts.is_trusted(&repo));
        ts.revoke(&repo).unwrap();
        assert!(!ts.is_trusted(&repo));
    }

    #[test]
    fn cache_key_changes_with_every_input() {
        let base = FilterContext {
            raw_blob: &oid(1),
            path: &rp("a.txt"),
            attr_source: Some(&oid(9)),
            config_digest: "cfg1",
            filter_identity: Some("lfs"),
            eol_mode: "native",
            format_version: 1,
        };
        let k0 = base.cache_key();

        let changed_blob = FilterContext {
            raw_blob: &oid(2),
            ..base.clone()
        };
        let changed_path = FilterContext {
            path: &rp("b.txt"),
            ..base.clone()
        };
        let changed_attr = FilterContext {
            attr_source: Some(&oid(10)),
            ..base.clone()
        };
        let changed_cfg = FilterContext {
            config_digest: "cfg2",
            ..base.clone()
        };
        let changed_eol = FilterContext {
            eol_mode: "crlf",
            ..base.clone()
        };
        for other in [
            changed_blob,
            changed_path,
            changed_attr,
            changed_cfg,
            changed_eol,
        ] {
            assert_ne!(k0, other.cache_key());
        }
    }
}
