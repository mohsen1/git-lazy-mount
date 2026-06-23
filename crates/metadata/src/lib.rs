//! `glm-metadata` — tree metadata cache and stat policy (spec §18, §5.1).
//!
//! Loading Git trees lazily makes `readdir` `O(entries in that directory)`. This
//! crate caches parsed trees so repeated directory lookups avoid re-parsing,
//! keyed by `(object format, tree oid, parser version)` — absence of size is
//! never interpreted as zero (spec §18). It also defines the **stat policy**
//! (`exact` vs `manifest-assisted`, spec §5.1) and the provenance of a reported
//! size, so callers can distinguish a known raw size from one that required
//! filtering (metadata-triggered hydration).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, ObjectId, Result, TreeObject};
use serde::{Deserialize, Serialize};

/// Bump when the cached tree encoding changes (invalidates on-disk entries).
pub const PARSER_VERSION: u32 = 1;

/// How metadata (especially exact size) is resolved (spec §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataMode {
    /// Always return the correct size, fetching+filtering if necessary.
    Exact,
    /// Consult an optional content-addressed manifest of raw sizes/flags first
    /// (not implemented yet; falls back to `Exact`).
    ManifestAssisted,
}

/// Where a reported size came from — useful for hydration accounting (§5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeSource {
    /// Overlay/local content; no fetch.
    Local,
    /// A raw object size known without filtering.
    RawObject,
    /// Required obtaining and filtering content (metadata-triggered hydration).
    FilteredHydration,
    /// Supplied by a metadata manifest.
    Manifest,
}

/// An in-memory tree cache with optional on-disk persistence.
///
/// Keyed by object id; the on-disk layout additionally namespaces by
/// [`PARSER_VERSION`] so a format change cannot serve stale entries. A negative
/// cache records oids known to be absent to avoid repeated lookups.
pub struct TreeCache {
    dir: Option<PathBuf>,
    mem: Mutex<HashMap<ObjectId, TreeObject>>,
    negative: Mutex<HashSet<ObjectId>>,
}

impl Default for TreeCache {
    fn default() -> Self {
        TreeCache::in_memory()
    }
}

impl TreeCache {
    /// A purely in-memory cache (no persistence).
    pub fn in_memory() -> TreeCache {
        TreeCache {
            dir: None,
            mem: Mutex::new(HashMap::new()),
            negative: Mutex::new(HashSet::new()),
        }
    }

    /// A cache persisted under `dir` (created if needed), namespaced by parser
    /// version.
    pub fn persistent(dir: impl Into<PathBuf>) -> Result<TreeCache> {
        let dir = dir.into().join(format!("v{PARSER_VERSION}"));
        std::fs::create_dir_all(&dir)?;
        Ok(TreeCache {
            dir: Some(dir),
            mem: Mutex::new(HashMap::new()),
            negative: Mutex::new(HashSet::new()),
        })
    }

    fn disk_path(dir: &Path, id: &ObjectId) -> PathBuf {
        dir.join(format!("{}-{}.json", id.format.name(), id.to_hex()))
    }

    /// Fetch a cached tree, consulting memory then disk. `None` if not cached.
    pub fn get(&self, id: &ObjectId) -> Result<Option<TreeObject>> {
        if let Some(t) = self.mem.lock().unwrap().get(id) {
            return Ok(Some(t.clone()));
        }
        if let Some(dir) = &self.dir {
            let path = Self::disk_path(dir, id);
            if path.exists() {
                let bytes = std::fs::read(&path)?;
                let tree: TreeObject = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::new(
                        ErrorCode::LocalObjectCorruption,
                        format!("corrupt tree cache: {e}"),
                    )
                })?;
                self.mem.lock().unwrap().insert(id.clone(), tree.clone());
                return Ok(Some(tree));
            }
        }
        Ok(None)
    }

    /// Insert a parsed tree into the cache (memory + disk).
    pub fn put(&self, tree: TreeObject) -> Result<()> {
        let id = tree.id.clone();
        if let Some(dir) = &self.dir {
            let bytes = serde_json::to_vec(&tree)
                .map_err(|e| Error::internal(format!("encode tree: {e}")))?;
            let target = Self::disk_path(dir, &id);
            let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
            tmp.write_all(&bytes)?;
            tmp.as_file().sync_all()?;
            tmp.persist(&target).map_err(|e| {
                Error::internal(format!("persist tree cache: {e}")).with_source(e.error)
            })?;
        }
        self.negative.lock().unwrap().remove(&id);
        self.mem.lock().unwrap().insert(id, tree);
        Ok(())
    }

    /// Record that `id` is known to be absent (negative caching, spec §18).
    pub fn mark_absent(&self, id: ObjectId) {
        self.negative.lock().unwrap().insert(id);
    }

    /// Whether `id` is in the negative cache.
    pub fn is_known_absent(&self, id: &ObjectId) -> bool {
        self.negative.lock().unwrap().contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::{GitMode, ObjectFormat, TreeEntry};

    fn oid(b: u8) -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![b; 20],
        }
    }

    fn tree(id: u8) -> TreeObject {
        TreeObject {
            id: oid(id),
            entries: vec![TreeEntry {
                name: b"file".to_vec(),
                mode: GitMode::Regular,
                object_id: oid(99),
            }],
        }
    }

    #[test]
    fn in_memory_roundtrip_and_negative() {
        let c = TreeCache::in_memory();
        assert!(c.get(&oid(1)).unwrap().is_none());
        c.put(tree(1)).unwrap();
        assert_eq!(c.get(&oid(1)).unwrap().unwrap().entries.len(), 1);
        c.mark_absent(oid(2));
        assert!(c.is_known_absent(&oid(2)));
        // Inserting clears any negative mark.
        c.put(tree(2)).unwrap();
        assert!(!c.is_known_absent(&oid(2)));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let c = TreeCache::persistent(dir.path()).unwrap();
            c.put(tree(7)).unwrap();
        }
        let c = TreeCache::persistent(dir.path()).unwrap();
        assert_eq!(c.get(&oid(7)).unwrap().unwrap().id, oid(7));
    }

    #[test]
    fn modes_are_distinct() {
        assert_ne!(MetadataMode::Exact, MetadataMode::ManifestAssisted);
        assert_ne!(SizeSource::Local, SizeSource::FilteredHydration);
    }
}
