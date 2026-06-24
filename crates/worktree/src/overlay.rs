//! The durable writable overlay (redesign.md §8, §32). The overlay is the layer
//! that records local working-tree changes on top of the read-only baseline:
//! created/modified files, symlinks, explicit (empty) directories, deletions
//! (tombstones), and clean-rename base-refs.
//!
//! Storage (§32): file **content lives in native files** addressed by an opaque
//! content id, served by FD — never buffered whole (§4.6); the **namespace** is
//! a parent-indexed map persisted as one atomic sidecar per entry, so dirty
//! state survives unmount/remount and daemon restart (criterion 26). A later
//! refinement moves the namespace to a single transactional log/DB and routes
//! all writes through the daemon (§32.1); the per-entry-atomic shape already
//! gives single-entry durability.
//!
//! This module owns only working-tree **bytes** — never Git state (§8).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, GitMode, ObjectId, RepoPath, Result};
use serde::{Deserialize, Serialize};

static CONTENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// What an overlay entry is. Content-bearing files reference a native content
/// file by id; symlinks carry their (small) target inline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayEntry {
    /// A created or modified regular file; bytes are in content file `content`.
    File {
        /// Opaque native content-file id.
        content: String,
        /// Executable mode bit.
        executable: bool,
    },
    /// A symlink; its raw target bytes are stored inline.
    Symlink {
        /// Raw target bytes (§30.1).
        target: Vec<u8>,
    },
    /// An explicit directory (e.g. a created empty dir; persisted so it survives
    /// remount — §4.9).
    Dir,
    /// A deletion of a baseline path (the path reads as absent).
    Tombstone,
    /// A clean-rename target: the bytes are an existing Git blob, recorded
    /// without fetching or copying content (§29).
    BaseRef {
        /// The blob/tree object id.
        oid: ObjectId,
        /// The Git mode.
        mode: GitMode,
    },
}

#[derive(Serialize, Deserialize)]
struct Sidecar {
    path: Vec<u8>,
    entry: OverlayEntry,
}

/// The durable overlay store for one workspace.
pub struct Overlay {
    meta_dir: PathBuf,
    content_dir: PathBuf,
    // In-memory caches rebuilt from the sidecars on open (disposable; §7).
    index: Mutex<Index>,
}

#[derive(Default)]
struct Index {
    /// path -> entry
    entries: HashMap<RepoPath, OverlayEntry>,
    /// parent path -> {child name -> ()} for O(direct children) listing (§15).
    children: HashMap<RepoPath, HashMap<Vec<u8>, ()>>,
}

impl Index {
    fn insert(&mut self, path: RepoPath, entry: OverlayEntry) {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            self.children
                .entry(parent)
                .or_default()
                .insert(name.to_vec(), ());
        }
        self.entries.insert(path, entry);
    }
    fn remove(&mut self, path: &RepoPath) {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            if let Some(set) = self.children.get_mut(&parent) {
                set.remove(name);
                if set.is_empty() {
                    self.children.remove(&parent);
                }
            }
        }
        self.entries.remove(path);
    }
}

impl Overlay {
    /// Open (creating if absent) the overlay rooted at `root`, replaying any
    /// persisted entries into the in-memory index.
    pub fn open(root: impl Into<PathBuf>) -> Result<Overlay> {
        let root = root.into();
        let meta_dir = root.join("meta");
        let content_dir = root.join("content");
        std::fs::create_dir_all(&meta_dir).map_err(io("create overlay meta dir"))?;
        std::fs::create_dir_all(&content_dir).map_err(io("create overlay content dir"))?;

        let mut index = Index::default();
        for ent in std::fs::read_dir(&meta_dir).map_err(io("scan overlay meta"))? {
            let ent = ent.map_err(io("read overlay meta entry"))?;
            let bytes = std::fs::read(ent.path()).map_err(io("read overlay sidecar"))?;
            let Ok(sc) = serde_json::from_slice::<Sidecar>(&bytes) else {
                // Skip a corrupt sidecar rather than fail the whole mount; the
                // path simply falls through to the baseline (recovery quarantines
                // it in a later pass, §32.2).
                continue;
            };
            if let Ok(path) = RepoPath::from_bytes(sc.path) {
                index.insert(path, sc.entry);
            }
        }
        Ok(Overlay {
            meta_dir,
            content_dir,
            index: Mutex::new(index),
        })
    }

    fn sidecar_path(&self, path: &RepoPath) -> PathBuf {
        self.meta_dir.join(id_for(path.as_bytes()))
    }

    fn content_path(&self, content: &str) -> PathBuf {
        self.content_dir.join(content)
    }

    fn persist(&self, path: &RepoPath, entry: &OverlayEntry) -> Result<()> {
        let sc = Sidecar {
            path: path.as_bytes().to_vec(),
            entry: entry.clone(),
        };
        let bytes = serde_json::to_vec(&sc)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("encode sidecar: {e}")))?;
        atomic_write(&self.sidecar_path(path), &bytes)
    }

    /// The overlay entry at `path`, if any.
    pub fn lookup(&self, path: &RepoPath) -> Option<OverlayEntry> {
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get(path)
            .cloned()
    }

    /// Direct children present in the overlay under `parent` (names + entries).
    /// O(direct children), independent of total dirty paths (§15).
    pub fn children(&self, parent: &RepoPath) -> Vec<(Vec<u8>, OverlayEntry)> {
        let idx = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match idx.children.get(parent) {
            None => Vec::new(),
            Some(names) => names
                .keys()
                .filter_map(|name| {
                    let child = parent.join(name).ok()?;
                    idx.entries.get(&child).map(|e| (name.clone(), e.clone()))
                })
                .collect(),
        }
    }

    /// Create or replace a regular file at `path` and return a writable FD
    /// positioned at 0. If `seed` is given, its bytes are copied in (copy-up for
    /// a partial overwrite); otherwise the file starts empty (`O_TRUNC`/create —
    /// **no baseline fetch**, §17.2). Content streams through the FS; nothing is
    /// buffered whole here.
    pub fn create_file(
        &self,
        path: &RepoPath,
        executable: bool,
        seed: Option<&Path>,
    ) -> Result<File> {
        let content = new_content_id();
        let cpath = self.content_path(&content);
        if let Some(seed) = seed {
            std::fs::copy(seed, &cpath).map_err(io("copy-up seed"))?;
        } else {
            File::create(&cpath).map_err(io("create overlay content"))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cpath)
            .map_err(io("open overlay content"))?;
        let entry = OverlayEntry::File {
            content,
            executable,
        };
        self.persist(path, &entry)?;
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.clone(), entry);
        Ok(file)
    }

    /// The byte size of the overlay content file at `path`.
    pub fn content_size(&self, path: &RepoPath) -> Result<u64> {
        match self.lookup(path) {
            Some(OverlayEntry::File { content, .. }) => {
                std::fs::metadata(self.content_path(&content))
                    .map(|m| m.len())
                    .map_err(io("stat overlay content"))
            }
            _ => Err(Error::new(ErrorCode::Internal, "not an overlay file")),
        }
    }

    /// Open the existing overlay content file at `path` for read+write.
    pub fn open_content(&self, path: &RepoPath) -> Result<File> {
        let content = match self.lookup(path) {
            Some(OverlayEntry::File { content, .. }) => content,
            _ => return Err(Error::new(ErrorCode::Internal, "not an overlay file")),
        };
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.content_path(&content))
            .map_err(io("open overlay content"))
    }

    /// Record a symlink (overwriting any prior entry).
    pub fn put_symlink(&self, path: &RepoPath, target: &[u8]) -> Result<()> {
        let entry = OverlayEntry::Symlink {
            target: target.to_vec(),
        };
        self.persist(path, &entry)?;
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.clone(), entry);
        Ok(())
    }

    /// Record an explicit (e.g. empty) directory.
    pub fn put_dir(&self, path: &RepoPath) -> Result<()> {
        self.persist(path, &OverlayEntry::Dir)?;
        self.index
            .lock()
            .unwrap()
            .insert(path.clone(), OverlayEntry::Dir);
        Ok(())
    }

    /// Record a clean-rename base-ref (no content copy/fetch; §29).
    pub fn put_base_ref(&self, path: &RepoPath, oid: ObjectId, mode: GitMode) -> Result<()> {
        let entry = OverlayEntry::BaseRef { oid, mode };
        self.persist(path, &entry)?;
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.clone(), entry);
        Ok(())
    }

    /// Tombstone `path` (mask a baseline entry as deleted), dropping any content.
    pub fn tombstone(&self, path: &RepoPath) -> Result<()> {
        self.drop_content(path);
        self.persist(path, &OverlayEntry::Tombstone)?;
        self.index
            .lock()
            .unwrap()
            .insert(path.clone(), OverlayEntry::Tombstone);
        Ok(())
    }

    /// Remove the overlay entry at `path` entirely (revert to baseline).
    pub fn clear(&self, path: &RepoPath) -> Result<()> {
        self.drop_content(path);
        let _ = std::fs::remove_file(self.sidecar_path(path));
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
        Ok(())
    }

    /// Re-key the overlay entry from `src` to `dst`, **keeping** the content file
    /// (it is referenced by `dst` afterward). The caller decides how to mask any
    /// baseline at `src` (e.g. a tombstone). No-op if `src` has no entry.
    pub fn rename(&self, src: &RepoPath, dst: &RepoPath) -> Result<()> {
        let Some(entry) = self.lookup(src) else {
            return Ok(());
        };
        self.persist(dst, &entry)?;
        let _ = std::fs::remove_file(self.sidecar_path(src));
        let mut idx = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idx.insert(dst.clone(), entry);
        idx.remove(src);
        Ok(())
    }

    /// Set the executable bit of an existing overlay File entry (re-persist).
    pub fn set_executable(&self, path: &RepoPath, exec: bool) -> Result<()> {
        let Some(OverlayEntry::File { content, .. }) = self.lookup(path) else {
            return Err(Error::new(ErrorCode::Internal, "not an overlay file"));
        };
        let entry = OverlayEntry::File {
            content,
            executable: exec,
        };
        self.persist(path, &entry)?;
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.clone(), entry);
        Ok(())
    }

    fn drop_content(&self, path: &RepoPath) {
        if let Some(OverlayEntry::File { content, .. }) = self.lookup(path) {
            let _ = std::fs::remove_file(self.content_path(&content));
        }
    }
}

fn new_content_id() -> String {
    let seq = CONTENT_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("c{}-{}", std::process::id(), seq)
}

fn id_for(path_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path_bytes);
    format!("{:x}.json", h.finalize())
}

fn atomic_write(dst: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = dst.with_extension("tmp");
    {
        let mut f = File::create(&tmp).map_err(io("create sidecar tmp"))?;
        f.write_all(bytes).map_err(io("write sidecar"))?;
        f.sync_all().map_err(io("fsync sidecar"))?;
    }
    std::fs::rename(&tmp, dst).map_err(io("publish sidecar"))?;
    // fsync the parent dir so the rename itself is durable — otherwise a crash
    // can lose an acknowledged create/rename even though the file was fsynced
    // (redesign.md §32). Best-effort: a dir that can't be synced must not fail
    // the write.
    if let Some(parent) = dst.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::new(ErrorCode::Internal, format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn create_write_read_via_fd() {
        let tmp = tempfile::tempdir().unwrap();
        let ov = Overlay::open(tmp.path()).unwrap();
        let mut f = ov.create_file(&p("a/b.txt"), false, None).unwrap();
        f.write_all(b"hello").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        assert!(matches!(
            ov.lookup(&p("a/b.txt")),
            Some(OverlayEntry::File {
                executable: false,
                ..
            })
        ));
        // child listing is parent-indexed
        let kids = ov.children(&p("a"));
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].0, b"b.txt");
    }

    #[test]
    fn copy_up_seed_then_extend() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed");
        std::fs::write(&seed, b"BASE").unwrap();
        let ov = Overlay::open(tmp.path().join("ov")).unwrap();
        let mut f = ov.create_file(&p("f"), false, Some(&seed)).unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"+more").unwrap();
        let mut g = ov.open_content(&p("f")).unwrap();
        let mut s = String::new();
        g.read_to_string(&mut s).unwrap();
        assert_eq!(s, "BASE+more");
    }

    #[test]
    fn tombstone_symlink_dir_and_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let ov = Overlay::open(tmp.path()).unwrap();
        ov.tombstone(&p("gone")).unwrap();
        ov.put_symlink(&p("link"), b"target/path").unwrap();
        ov.put_dir(&p("emptydir")).unwrap();
        assert_eq!(ov.lookup(&p("gone")), Some(OverlayEntry::Tombstone));
        assert!(matches!(
            ov.lookup(&p("link")),
            Some(OverlayEntry::Symlink { .. })
        ));
        assert_eq!(ov.lookup(&p("emptydir")), Some(OverlayEntry::Dir));
        ov.clear(&p("gone")).unwrap();
        assert_eq!(ov.lookup(&p("gone")), None);
    }

    #[test]
    fn dirty_state_survives_reopen() {
        // criterion 26: dirty overlay state survives unmount/remount.
        let tmp = tempfile::tempdir().unwrap();
        {
            let ov = Overlay::open(tmp.path()).unwrap();
            ov.create_file(&p("keep.txt"), true, None)
                .unwrap()
                .write_all(b"durable")
                .unwrap();
            ov.tombstone(&p("deleted")).unwrap();
            ov.put_symlink(&p("s"), b"t").unwrap();
        }
        let ov2 = Overlay::open(tmp.path()).unwrap();
        assert!(matches!(
            ov2.lookup(&p("keep.txt")),
            Some(OverlayEntry::File {
                executable: true,
                ..
            })
        ));
        let mut s = String::new();
        ov2.open_content(&p("keep.txt"))
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "durable");
        assert_eq!(ov2.lookup(&p("deleted")), Some(OverlayEntry::Tombstone));
        assert!(matches!(
            ov2.lookup(&p("s")),
            Some(OverlayEntry::Symlink { .. })
        ));
    }
}
