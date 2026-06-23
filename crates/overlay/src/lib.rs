//! `glm-overlay` — the writable copy-on-write overlay (spec §8, §21).
//!
//! The overlay durably stores **locally materialized** content: bytes written
//! through the filesystem, plus *tombstones* for deletions. Clean tracked
//! content is NOT stored here — it stays recoverable from Git objects (spec
//! §3.7/§3.8). Dirty, new, and otherwise materialized content lives here so it
//! survives unmount/remount and crashes (spec §53.11/§53.12).
//!
//! Each path maps to at most one entry, addressed by a hash of the path bytes
//! (so arbitrary non-UTF-8 paths are handled without filesystem-name hazards).
//! Writes are published atomically: content is written to a temp file, fsynced,
//! renamed into place, and only then is the metadata record renamed in — so a
//! crash never yields a metadata record pointing at absent/torn content.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, RepoPath, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What an overlay entry represents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayKind {
    /// A regular file; `executable` tracks the Git executable bit.
    File {
        /// Whether the Git executable bit is set.
        executable: bool,
    },
    /// A symbolic link; content bytes are the link target.
    Symlink,
    /// A deletion tombstone (the path is removed in the working tree).
    Tombstone,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EntryMeta {
    path: RepoPath,
    kind: OverlayKind,
}

/// The copy-on-write overlay store rooted at a directory.
pub struct Overlay {
    root: PathBuf,
    index: Mutex<HashMap<RepoPath, OverlayKind>>,
}

impl Overlay {
    /// Open (creating if needed) an overlay at `root`, rebuilding the in-memory
    /// index by scanning persisted entries (crash-safe: torn temp files are
    /// ignored).
    pub fn open(root: impl Into<PathBuf>) -> Result<Overlay> {
        let root = root.into();
        std::fs::create_dir_all(root.join("meta"))?;
        std::fs::create_dir_all(root.join("content"))?;
        let mut index = HashMap::new();
        for entry in std::fs::read_dir(root.join("meta"))? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue; // skip temp files
            }
            let bytes = std::fs::read(&p)?;
            match serde_json::from_slice::<EntryMeta>(&bytes) {
                Ok(meta) => {
                    index.insert(meta.path, meta.kind);
                }
                Err(_) => continue, // ignore corrupt/partial records
            }
        }
        Ok(Overlay {
            root,
            index: Mutex::new(index),
        })
    }

    fn id_for(path: &RepoPath) -> String {
        let mut h = Sha256::new();
        h.update(path.as_bytes());
        hex::encode(h.finalize())
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join("meta").join(format!("{id}.json"))
    }

    fn content_path(&self, id: &str) -> PathBuf {
        self.root.join("content").join(id)
    }

    /// Write a regular file's full content into the overlay (atomic).
    pub fn put_file(&self, path: &RepoPath, bytes: &[u8], executable: bool) -> Result<()> {
        self.put(path, bytes, OverlayKind::File { executable })
    }

    /// Write a symlink's target bytes into the overlay (atomic).
    pub fn put_symlink(&self, path: &RepoPath, target: &[u8]) -> Result<()> {
        self.put(path, target, OverlayKind::Symlink)
    }

    fn put(&self, path: &RepoPath, bytes: &[u8], kind: OverlayKind) -> Result<()> {
        let id = Self::id_for(path);
        // 1) content (fsync, rename) — published before metadata references it.
        atomic_write(&self.content_path(&id), bytes)?;
        // 2) metadata (fsync, rename).
        let meta = EntryMeta {
            path: path.clone(),
            kind: kind.clone(),
        };
        let json = serde_json::to_vec(&meta)
            .map_err(|e| Error::new(ErrorCode::OverlayCorruption, format!("encode meta: {e}")))?;
        atomic_write(&self.meta_path(&id), &json)?;
        self.index.lock().unwrap().insert(path.clone(), kind);
        Ok(())
    }

    /// Record a deletion tombstone for `path` (spec §21 delete).
    pub fn tombstone(&self, path: &RepoPath) -> Result<()> {
        let id = Self::id_for(path);
        let meta = EntryMeta {
            path: path.clone(),
            kind: OverlayKind::Tombstone,
        };
        let json = serde_json::to_vec(&meta)
            .map_err(|e| Error::new(ErrorCode::OverlayCorruption, format!("encode meta: {e}")))?;
        atomic_write(&self.meta_path(&id), &json)?;
        let _ = std::fs::remove_file(self.content_path(&id));
        self.index
            .lock()
            .unwrap()
            .insert(path.clone(), OverlayKind::Tombstone);
        Ok(())
    }

    /// Remove any overlay entry for `path` (dematerialize back to clean; spec
    /// §24 step 12).
    pub fn clear(&self, path: &RepoPath) -> Result<()> {
        let id = Self::id_for(path);
        let _ = std::fs::remove_file(self.meta_path(&id));
        let _ = std::fs::remove_file(self.content_path(&id));
        self.index.lock().unwrap().remove(path);
        Ok(())
    }

    /// The kind of overlay entry for `path`, if any.
    pub fn entry(&self, path: &RepoPath) -> Option<OverlayKind> {
        self.index.lock().unwrap().get(path).cloned()
    }

    /// Whether `path` has a tombstone.
    pub fn is_tombstone(&self, path: &RepoPath) -> bool {
        matches!(self.entry(path), Some(OverlayKind::Tombstone))
    }

    /// Read overlay content bytes for `path`. `None` if no content entry (e.g.
    /// tombstone or absent).
    pub fn read_content(&self, path: &RepoPath) -> Result<Option<Vec<u8>>> {
        match self.entry(path) {
            Some(OverlayKind::File { .. }) | Some(OverlayKind::Symlink) => {
                let id = Self::id_for(path);
                Ok(Some(std::fs::read(self.content_path(&id))?))
            }
            _ => Ok(None),
        }
    }

    /// The byte length of overlay content for `path`, if present (no read of
    /// the full content needed beyond a `stat`).
    pub fn content_len(&self, path: &RepoPath) -> Result<Option<u64>> {
        match self.entry(path) {
            Some(OverlayKind::File { .. }) | Some(OverlayKind::Symlink) => {
                let id = Self::id_for(path);
                Ok(Some(std::fs::metadata(self.content_path(&id))?.len()))
            }
            _ => Ok(None),
        }
    }

    /// Snapshot of all overlay entries (the basis for O(overlay) status; spec
    /// §2.7, §49).
    pub fn entries(&self) -> Vec<(RepoPath, OverlayKind)> {
        self.index
            .lock()
            .unwrap()
            .iter()
            .map(|(p, k)| (p.clone(), k.clone()))
            .collect()
    }

    /// Number of overlay entries (dirty/new/deleted paths).
    pub fn len(&self) -> usize {
        self.index.lock().unwrap().len()
    }

    /// Whether the overlay is empty (the workspace is clean).
    pub fn is_empty(&self) -> bool {
        self.index.lock().unwrap().is_empty()
    }
}

/// Atomically write `bytes` to `target`: temp file in the same dir, fsync,
/// rename, then best-effort fsync of the directory.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| Error::new(ErrorCode::OverlayCorruption, "overlay target has no parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target).map_err(|e| {
        Error::new(ErrorCode::OverlayCorruption, format!("overlay rename: {e}"))
            .with_source(e.error)
    })?;
    // Best-effort durability of the rename itself.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn put_read_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        {
            let ov = Overlay::open(dir.path()).unwrap();
            ov.put_file(&p("src/main.rs"), b"fn main() {}", false)
                .unwrap();
            ov.put_file(&p("run.sh"), b"#!/bin/sh\n", true).unwrap();
            assert_eq!(
                ov.read_content(&p("src/main.rs")).unwrap().unwrap(),
                b"fn main() {}"
            );
            assert_eq!(ov.len(), 2);
        }
        // Reopen: entries survive (dirty state across remount; spec §53.11).
        let ov = Overlay::open(dir.path()).unwrap();
        assert_eq!(ov.len(), 2);
        assert_eq!(
            ov.entry(&p("run.sh")),
            Some(OverlayKind::File { executable: true })
        );
        assert_eq!(
            ov.read_content(&p("run.sh")).unwrap().unwrap(),
            b"#!/bin/sh\n"
        );
    }

    #[test]
    fn tombstone_hides_content() {
        let dir = tempfile::tempdir().unwrap();
        let ov = Overlay::open(dir.path()).unwrap();
        ov.put_file(&p("a"), b"data", false).unwrap();
        ov.tombstone(&p("a")).unwrap();
        assert!(ov.is_tombstone(&p("a")));
        assert!(ov.read_content(&p("a")).unwrap().is_none());
    }

    #[test]
    fn clear_dematerializes() {
        let dir = tempfile::tempdir().unwrap();
        let ov = Overlay::open(dir.path()).unwrap();
        ov.put_file(&p("a"), b"data", false).unwrap();
        ov.clear(&p("a")).unwrap();
        assert!(ov.entry(&p("a")).is_none());
        assert!(ov.is_empty());
    }

    #[test]
    fn non_utf8_path_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let ov = Overlay::open(dir.path()).unwrap();
        let path = RepoPath::from_bytes(vec![0xff, b'x']).unwrap();
        ov.put_file(&path, b"bytes", false).unwrap();
        let ov2 = Overlay::open(dir.path()).unwrap();
        assert_eq!(ov2.read_content(&path).unwrap().unwrap(), b"bytes");
    }

    #[test]
    fn content_len_without_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let ov = Overlay::open(dir.path()).unwrap();
        ov.put_file(&p("big"), &vec![0u8; 4096], false).unwrap();
        assert_eq!(ov.content_len(&p("big")).unwrap(), Some(4096));
    }
}
