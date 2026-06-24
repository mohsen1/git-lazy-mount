//! Virtual working-tree projection (redesign.md §8, §14–§17, §29).
//!
//! The projected working tree is the durable [`overlay`] layered over the HEAD
//! commit's tree (the *baseline*), plus a protected synthetic `.git` gitfile at
//! the root. Path resolution order (§8):
//! 1. synthetic `.git` (root) — shadows any entry, fails-safe (§6)
//! 2. overlay file / symlink / dir / clean-rename base-ref
//! 3. overlay tombstone (incl. a tombstoned ancestor) → absent
//! 4. baseline Git tree entry
//! 5. absent
//!
//! Invariants enforced here (each covered by a test):
//! * `readdir` returns names + kind only — it **never** reads blob contents or
//!   resolves exact sizes (§4.5, §38.2); it merges baseline + overlay children.
//! * a repo `.git` tree entry never shadows the synthetic one (§6).
//! * writes copy up once then write in place (no full rewrite, §17.2/§38.8);
//!   `O_TRUNC`/create and clean renames fetch **no** blob (§29).
//! * resolution + listing cost is O(direct children), independent of repo size.

#![forbid(unsafe_code)]

pub mod journal;
pub mod overlay;
pub use overlay::{Overlay, OverlayEntry};

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glm_core::{Error, ErrorCode, GitMode, ObjectId, RepoPath, Result};
use glm_fs_common::{InodeTable, ROOT_INO};
use glm_git_repo::AdminRepo;

/// Uniquifier for temporary cache files during atomic publish (§17.1, §20.2).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The reserved name of the synthetic gitfile at the projection root (§6).
pub const GITFILE_NAME: &[u8] = b".git";

/// Projected entry kind (neutral; mapped to FUSE `d_type`/mode by the mount).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Directory (a Git tree, or a submodule gitlink projected as a dir).
    Dir,
    /// Regular file; `executable` is the Git mode bit.
    File {
        /// Whether the executable bit is set.
        executable: bool,
    },
    /// Symbolic link.
    Symlink,
}

/// Neutral, stable attributes for a projected entry (redesign.md §22).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    /// Inode number.
    pub ino: u64,
    /// Inode generation.
    pub generation: u64,
    /// Exact byte size (0 for directories).
    pub size: u64,
    /// Entry kind.
    pub kind: Kind,
}

/// One directory listing entry — name + kind + inode only (no size; §4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Exact recorded name bytes.
    pub name: Vec<u8>,
    /// Entry kind.
    pub kind: Kind,
    /// Stable inode.
    pub ino: u64,
}

/// What a path resolves to after layering the overlay over the baseline (§8).
enum Resolved {
    /// A directory; `baseline_tree` is the underlying Git tree (if any) whose
    /// children merge with the overlay's.
    Dir { baseline_tree: Option<ObjectId> },
    /// A regular file, backed by the overlay or a baseline blob.
    File {
        source: FileSource,
        executable: bool,
    },
    /// A symlink, backed by the overlay or a baseline blob.
    Symlink { source: SymSource },
    /// The synthetic `.git` gitfile.
    Gitfile,
}

/// Where a projected regular file's bytes come from.
enum FileSource {
    /// A native overlay content file at this repo path.
    Overlay(RepoPath),
    /// A baseline Git blob (lazily materialized).
    Baseline(ObjectId),
}

/// Where a projected symlink's target comes from.
enum SymSource {
    /// Inline overlay target bytes.
    Overlay(Vec<u8>),
    /// A baseline Git blob holding the target.
    Baseline(ObjectId),
}

/// A read-only projection of one workspace's working tree.
pub struct Projection {
    repo: AdminRepo,
    inodes: InodeTable,
    baseline_tree: ObjectId,
    gitfile: Vec<u8>,
    /// The durable writable overlay layered over the baseline (§8).
    overlay: Overlay,
    /// Content-addressed cache directory for materialized blob bytes (§20.2).
    cache_dir: PathBuf,
    /// Count of content hydrations (cache-miss blob materializations) — the
    /// signal behind the §38.2/§38.5 budget assertions (`ls` = 0, `cat` ≥ 1).
    hydrations: AtomicU64,
    /// Whether `getattr` may fault an object in to learn its exact size (§21:
    /// metadata-triggered hydration, never a faked size). INTERIM: this uses
    /// git's lazy fetch; once the bounded fetch scheduler exists it must route
    /// through it so that "only the scheduler causes network" holds (§18–§20).
    metadata_fetch: bool,
    // No global lock: the InodeTable, Overlay, and the content cache are each
    // internally synchronized, so callbacks never serialize behind a coarse mutex
    // held across a blocking `git` subprocess (§18/§19).
}

/// An open read handle backing a `read` callback. Content is served from a file
/// descriptor (a cache file) or, for the tiny synthetic `.git`, from memory —
/// **never** by allocating the whole blob (redesign.md §4.6, §17).
pub struct ContentHandle {
    inner: ContentInner,
}

enum ContentInner {
    /// The synthetic `.git` gitfile bytes (tens of bytes).
    Bytes(Vec<u8>),
    /// A materialized blob, served by `pread` from the cache file.
    File(std::fs::File),
}

impl ContentHandle {
    /// The total content size in bytes.
    pub fn size(&self) -> Result<u64> {
        match &self.inner {
            ContentInner::Bytes(b) => Ok(b.len() as u64),
            ContentInner::File(f) => f
                .metadata()
                .map(|m| m.len())
                .map_err(|e| Error::new(ErrorCode::Internal, format!("stat cache file: {e}"))),
        }
    }

    /// Read up to `len` bytes at `offset` — bounded by `len` (the FUSE request
    /// size), never proportional to the file size (§4.6, §38.8).
    pub fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        match &self.inner {
            ContentInner::Bytes(b) => {
                let start = (offset as usize).min(b.len());
                let end = start.saturating_add(len).min(b.len());
                Ok(b[start..end].to_vec())
            }
            ContentInner::File(f) => {
                let mut buf = vec![0u8; len];
                let n = pread(f, &mut buf, offset)
                    .map_err(|e| Error::new(ErrorCode::Internal, format!("pread: {e}")))?;
                buf.truncate(n);
                Ok(buf)
            }
        }
    }
}

/// Positional read, cross-platform (`pread` on unix, `seek_read` on Windows).
/// The mount is Linux-only, but the projection stays portable so the workspace
/// `check` matrix builds everywhere.
#[cfg(unix)]
fn pread(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, offset)
}

#[cfg(windows)]
fn pread(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, offset)
}

impl Projection {
    /// Open a projection over `repo`, baselined at its HEAD tree. `cache_dir`
    /// holds materialized blob content; `overlay_dir` holds the durable writable
    /// overlay (both created if absent).
    pub fn open(repo: AdminRepo, cache_dir: PathBuf, overlay_dir: PathBuf) -> Result<Projection> {
        let baseline_tree = repo.head_tree()?.ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "cannot project an unborn HEAD (no baseline tree)",
            )
        })?;
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("create cache dir: {e}")))?;
        let overlay = Overlay::open(overlay_dir)?;
        let gitfile = repo.synthetic_gitfile();
        Ok(Projection {
            repo,
            inodes: InodeTable::new(),
            baseline_tree,
            gitfile,
            overlay,
            cache_dir,
            hydrations: AtomicU64::new(0),
            metadata_fetch: true,
        })
    }

    /// Number of content hydrations so far (cache-miss blob materializations).
    pub fn hydrations(&self) -> u64 {
        self.hydrations.load(Ordering::Relaxed)
    }

    /// Release `n` kernel lookup references on `ino` (redesign.md §14 forget).
    pub fn forget(&self, ino: u64, n: u64) {
        if ino != ROOT_INO {
            self.inodes.forget(ino, n);
        }
    }

    /// The reserved root inode.
    pub fn root_ino(&self) -> u64 {
        ROOT_INO
    }

    /// The synthetic gitfile bytes (`gitdir: …`).
    pub fn gitfile_bytes(&self) -> &[u8] {
        &self.gitfile
    }

    /// The admin repo (for the mount layer to read the gitdir, etc.).
    pub fn repo(&self) -> &AdminRepo {
        &self.repo
    }

    fn path_of(&self, ino: u64) -> Result<RepoPath> {
        if ino == ROOT_INO {
            return Ok(RepoPath::root());
        }
        self.inodes
            .path_of(ino)
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("stale inode {ino}")))
    }

    /// Resolve a repo-relative path by layering the overlay over the baseline
    /// (§8): synthetic `.git` → overlay entry → overlay tombstone (incl. an
    /// ancestor) → baseline tree → absent. Reads only trees, never blob contents
    /// (§4.5).
    fn resolve(&self, path: &RepoPath) -> Result<Option<Resolved>> {
        if path.is_root() {
            return Ok(Some(Resolved::Dir {
                baseline_tree: Some(self.baseline_tree.clone()),
            }));
        }
        let comps: Vec<&[u8]> = path.components().collect();
        // The synthetic root `.git` shadows any entry of the same name (§6).
        if comps.len() == 1 && comps[0] == GITFILE_NAME {
            return Ok(Some(Resolved::Gitfile));
        }
        // 1. An overlay entry at the exact path wins over the baseline.
        if let Some(e) = self.overlay.lookup(path) {
            return Ok(match e {
                OverlayEntry::File { executable, .. } => Some(Resolved::File {
                    source: FileSource::Overlay(path.clone()),
                    executable,
                }),
                OverlayEntry::Symlink { target } => Some(Resolved::Symlink {
                    source: SymSource::Overlay(target),
                }),
                OverlayEntry::Dir => Some(Resolved::Dir {
                    baseline_tree: self.baseline_tree_at(path)?,
                }),
                OverlayEntry::Tombstone => None,
                OverlayEntry::BaseRef { oid, mode } => Some(baseref_resolved(oid, mode)),
            });
        }
        // 2. A tombstoned ancestor masks everything beneath it.
        if self.ancestor_tombstoned(path) {
            return Ok(None);
        }
        // 3. Fall through to the baseline tree.
        self.baseline_resolve(path)
    }

    /// Resolve strictly against the baseline Git tree (no overlay).
    fn baseline_resolve(&self, path: &RepoPath) -> Result<Option<Resolved>> {
        let comps: Vec<&[u8]> = path.components().collect();
        let mut tree = self.baseline_tree.clone();
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let obj = self.repo.store().read_tree(&tree, false)?;
            let Some(entry) = obj.entries.iter().find(|e| e.name == *comp) else {
                return Ok(None);
            };
            match entry.mode {
                GitMode::Tree | GitMode::Gitlink => {
                    if last {
                        return Ok(Some(Resolved::Dir {
                            baseline_tree: Some(entry.object_id.clone()),
                        }));
                    }
                    if entry.mode == GitMode::Gitlink {
                        return Ok(None);
                    }
                    tree = entry.object_id.clone();
                }
                GitMode::Regular | GitMode::Executable => {
                    if last {
                        return Ok(Some(Resolved::File {
                            source: FileSource::Baseline(entry.object_id.clone()),
                            executable: entry.mode == GitMode::Executable,
                        }));
                    }
                    return Ok(None);
                }
                GitMode::Symlink => {
                    if last {
                        return Ok(Some(Resolved::Symlink {
                            source: SymSource::Baseline(entry.object_id.clone()),
                        }));
                    }
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// The baseline Git tree at `path`, if it is a directory there.
    fn baseline_tree_at(&self, path: &RepoPath) -> Result<Option<ObjectId>> {
        match self.baseline_resolve(path)? {
            Some(Resolved::Dir { baseline_tree }) => Ok(baseline_tree),
            _ => Ok(None),
        }
    }

    /// Whether any proper ancestor of `path` is tombstoned in the overlay.
    fn ancestor_tombstoned(&self, path: &RepoPath) -> bool {
        let mut cur = path.parent();
        while let Some(p) = cur {
            if p.is_root() {
                break;
            }
            if matches!(self.overlay.lookup(&p), Some(OverlayEntry::Tombstone)) {
                return true;
            }
            cur = p.parent();
        }
        false
    }

    fn attr_of(&self, ino: u64, generation: u64, r: &Resolved) -> Result<Attr> {
        let (kind, size) = match r {
            Resolved::Dir { .. } => (Kind::Dir, 0),
            Resolved::Gitfile => (Kind::File { executable: false }, self.gitfile.len() as u64),
            Resolved::File { source, executable } => {
                let size = match source {
                    FileSource::Overlay(p) => self.overlay.content_size(p)?,
                    FileSource::Baseline(oid) => {
                        self.repo.store().object_size(oid, self.metadata_fetch)?
                    }
                };
                (
                    Kind::File {
                        executable: *executable,
                    },
                    size,
                )
            }
            Resolved::Symlink { source } => {
                let size = match source {
                    SymSource::Overlay(t) => t.len() as u64,
                    SymSource::Baseline(oid) => {
                        self.repo.store().object_size(oid, self.metadata_fetch)?
                    }
                };
                (Kind::Symlink, size)
            }
        };
        Ok(Attr {
            ino,
            generation,
            size,
            kind,
        })
    }

    /// `lookup(parent, name)` — resolve a child by name (§16). Allocates a stable
    /// inode for the child path.
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Option<Attr>> {
        let parent = self.path_of(parent_ino)?;
        let child = parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        match self.resolve(&child)? {
            Some(r) => {
                let (ino, generation) = self.inodes.lookup(&child);
                Ok(Some(self.attr_of(ino, generation, &r)?))
            }
            None => Ok(None),
        }
    }

    /// `getattr(ino)` (§16, §21). May fault an object in for its exact size.
    pub fn getattr(&self, ino: u64) -> Result<Attr> {
        let path = self.path_of(ino)?;
        let r = self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?;
        // Resolve the generation for the (already-known) inode. Never assert
        // equality here: a live FUSE callback must not panic (a panic drops the
        // reply and would wedge the mount).
        let (_i, generation) = self.inodes.lookup(&path);
        self.attr_of(ino, generation, &r)
    }

    /// `readdir(ino)` — names + kind + inode only; reads **no** blob contents and
    /// resolves **no** sizes (§4.5, §38.2). Cost is O(direct children).
    pub fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let path = self.path_of(ino)?;
        let Some(Resolved::Dir { baseline_tree }) = self.resolve(&path)? else {
            return Err(Error::new(ErrorCode::Internal, "not a directory"));
        };
        let at_root = path.is_root();
        let mut out = Vec::new();
        let push = |out: &mut Vec<DirEntry>, name: Vec<u8>, kind: Kind| -> Result<()> {
            let child = path
                .join(&name)
                .map_err(|err| Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}")))?;
            let (cino, _gen) = self.inodes.lookup(&child);
            out.push(DirEntry {
                name,
                kind,
                ino: cino,
            });
            Ok(())
        };

        // Baseline children, except those an overlay entry overrides/hides and
        // the root `.git` (shadowed by the synthetic one, §6).
        if let Some(tree) = baseline_tree {
            for e in self.repo.store().read_tree(&tree, false)?.entries {
                if at_root && e.name == GITFILE_NAME {
                    continue;
                }
                let child = path.join(&e.name).map_err(|err| {
                    Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}"))
                })?;
                if self.overlay.lookup(&child).is_some() {
                    continue; // the overlay loop below decides this name
                }
                let kind = match e.mode {
                    GitMode::Tree | GitMode::Gitlink => Kind::Dir,
                    GitMode::Regular => Kind::File { executable: false },
                    GitMode::Executable => Kind::File { executable: true },
                    GitMode::Symlink => Kind::Symlink,
                };
                push(&mut out, e.name, kind)?;
            }
        }

        // Overlay children: created/modified entries appear; tombstones hide.
        for (name, entry) in self.overlay.children(&path) {
            let kind = match entry {
                OverlayEntry::Tombstone => continue,
                OverlayEntry::File { executable, .. } => Kind::File { executable },
                OverlayEntry::Symlink { .. } => Kind::Symlink,
                OverlayEntry::Dir => Kind::Dir,
                OverlayEntry::BaseRef { mode, .. } => match mode {
                    GitMode::Symlink => Kind::Symlink,
                    GitMode::Tree | GitMode::Gitlink => Kind::Dir,
                    GitMode::Executable => Kind::File { executable: true },
                    GitMode::Regular => Kind::File { executable: false },
                },
            };
            push(&mut out, name, kind)?;
        }

        if at_root {
            push(
                &mut out,
                GITFILE_NAME.to_vec(),
                Kind::File { executable: false },
            )?;
        }
        Ok(out)
    }

    /// Open content for reading (§17.1). A clean tracked blob is materialized
    /// once into a content-addressed cache file (atomic publish) and then served
    /// by `pread` from its FD; the synthetic `.git` is served from memory.
    /// Faults the blob in on first access (a later refinement routes this through
    /// the bounded fetch scheduler so "only the scheduler causes network", §20).
    pub fn open_content(&self, ino: u64) -> Result<ContentHandle> {
        let path = self.path_of(ino)?;
        match self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?
        {
            Resolved::Gitfile => Ok(ContentHandle {
                inner: ContentInner::Bytes(self.gitfile.clone()),
            }),
            Resolved::File { source, .. } => {
                let file = match source {
                    FileSource::Overlay(p) => self.overlay.open_content(&p)?,
                    FileSource::Baseline(oid) => self.materialize(&oid)?,
                };
                Ok(ContentHandle {
                    inner: ContentInner::File(file),
                })
            }
            Resolved::Symlink { .. } => Err(Error::new(
                ErrorCode::Internal,
                "open_content on a symlink; use readlink",
            )),
            Resolved::Dir { .. } => Err(Error::new(ErrorCode::Internal, "is a directory")),
        }
    }

    /// Materialize a blob's working-tree bytes into the content-addressed cache
    /// and return an open read FD. Streams via `cat-file` (no in-process buffer)
    /// and publishes atomically (temp + rename) so a partial file is never used.
    fn materialize(&self, oid: &ObjectId) -> Result<std::fs::File> {
        let path = self.materialize_path(oid)?;
        std::fs::File::open(&path)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("open cache file: {e}")))
    }

    /// Ensure the blob is present in the content-addressed cache and return its
    /// path (used to seed a copy-up; §17.2).
    fn materialize_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        let final_path = self.cache_dir.join(oid.to_hex());
        if !final_path.exists() {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = self
                .cache_dir
                .join(format!(".{}.{}.tmp", oid.to_hex(), seq));
            // TODO(§38.6): coalesce concurrent faults of the same oid through the
            // fetch scheduler so 100 readers cause one retrieval. For now each
            // first-open may fetch; the rename keeps the published file correct.
            self.hydrations.fetch_add(1, Ordering::Relaxed);
            self.repo.store().blob_to_file(oid, true, &tmp)?;
            if let Err(e) = std::fs::rename(&tmp, &final_path) {
                let _ = std::fs::remove_file(&tmp);
                // A racing open may have published it already.
                if !final_path.exists() {
                    return Err(Error::new(
                        ErrorCode::Internal,
                        format!("publish cache file: {e}"),
                    ));
                }
            }
        }
        Ok(final_path)
    }

    /// `readlink(ino)` — the symlink's raw target bytes (§30.1). Targets are
    /// small, so the blob is read whole here (this is not a content stream).
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let path = self.path_of(ino)?;
        match self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?
        {
            Resolved::Symlink { source } => match source {
                SymSource::Overlay(target) => Ok(target),
                SymSource::Baseline(oid) => {
                    self.repo.store().read_blob_raw(&oid, self.metadata_fetch)
                }
            },
            _ => Err(Error::new(ErrorCode::Internal, "not a symlink")),
        }
    }

    // ---- write path (§8, §17, §29) ---------------------------------------

    fn child_path(&self, parent_ino: u64, name: &[u8]) -> Result<RepoPath> {
        let path = self
            .path_of(parent_ino)?
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        // The synthetic root `.git` is protected from creation/replacement/
        // deletion/rename (§6); reads always resolve to the gitfile.
        let protected = {
            let mut comps = path.components();
            comps.next() == Some(GITFILE_NAME) && comps.next().is_none()
        };
        if protected {
            return Err(Error::new(
                ErrorCode::Authentication,
                "the .git entry is protected",
            ));
        }
        Ok(path)
    }

    /// Create a new empty file under `parent_ino` (FUSE `create`) and return its
    /// attr plus a writable FD — no baseline fetch.
    pub fn create(
        &self,
        parent_ino: u64,
        name: &[u8],
        executable: bool,
    ) -> Result<(Attr, std::fs::File)> {
        let path = self.child_path(parent_ino, name)?;
        let file = self.overlay.create_file(&path, executable, None)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok((
            Attr {
                ino,
                generation,
                size: 0,
                kind: Kind::File { executable },
            },
            file,
        ))
    }

    /// Open an existing file for writing (FUSE `open` with write intent). Copies
    /// the baseline up once (§17.2); `truncate` seeds an empty file with **no**
    /// baseline fetch.
    pub fn open_write(&self, ino: u64, truncate: bool) -> Result<std::fs::File> {
        let path = self.path_of(ino)?;
        if matches!(self.overlay.lookup(&path), Some(OverlayEntry::File { .. })) {
            let f = self.overlay.open_content(&path)?;
            if truncate {
                f.set_len(0)
                    .map_err(|e| Error::new(ErrorCode::Internal, format!("truncate: {e}")))?;
            }
            return Ok(f);
        }
        match self.resolve(&path)? {
            Some(Resolved::File {
                source: FileSource::Baseline(oid),
                executable,
            }) => {
                let seed = if truncate {
                    None
                } else {
                    Some(self.materialize_path(&oid)?)
                };
                self.overlay.create_file(&path, executable, seed.as_deref())
            }
            _ => Err(Error::new(ErrorCode::Internal, "not a writable file")),
        }
    }

    /// Truncate/extend a file to `size` (FUSE `setattr` size); copies up if
    /// needed. `size == 0` never fetches the old blob (§38.7).
    pub fn truncate(&self, ino: u64, size: u64) -> Result<()> {
        let path = self.path_of(ino)?;
        let f = if matches!(self.overlay.lookup(&path), Some(OverlayEntry::File { .. })) {
            self.overlay.open_content(&path)?
        } else {
            match self.resolve(&path)? {
                Some(Resolved::File {
                    source: FileSource::Baseline(oid),
                    executable,
                }) => {
                    let seed = if size == 0 {
                        None
                    } else {
                        Some(self.materialize_path(&oid)?)
                    };
                    self.overlay
                        .create_file(&path, executable, seed.as_deref())?
                }
                _ => return Err(Error::new(ErrorCode::Internal, "not a file")),
            }
        };
        f.set_len(size)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("set_len: {e}")))
    }

    /// Remove a file/symlink (FUSE `unlink`): tombstone a baseline path, else
    /// drop the overlay-only entry.
    pub fn unlink(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let path = self.child_path(parent_ino, name)?;
        if self.baseline_resolve(&path)?.is_some() {
            self.overlay.tombstone(&path)?;
        } else if self.overlay.lookup(&path).is_some() {
            self.overlay.clear(&path)?;
        } else {
            return Err(Error::new(ErrorCode::NotFound, "no such file"));
        }
        // Drop the name→inode mapping so a later recreate gets a fresh inode and
        // the unlinked inode enters open-unlinked retention (§14, §17.4).
        self.inodes.unlink(&path);
        Ok(())
    }

    /// Create a directory (FUSE `mkdir`); persisted so empty dirs survive (§4.9).
    pub fn mkdir(&self, parent_ino: u64, name: &[u8]) -> Result<Attr> {
        let path = self.child_path(parent_ino, name)?;
        if self.resolve(&path)?.is_some() {
            return Err(Error::new(ErrorCode::AlreadyExists, "exists"));
        }
        self.overlay.put_dir(&path)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok(Attr {
            ino,
            generation,
            size: 0,
            kind: Kind::Dir,
        })
    }

    /// Remove a directory if empty (FUSE `rmdir`).
    pub fn rmdir(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let path = self.child_path(parent_ino, name)?;
        if !self.dir_is_empty(&path)? {
            return Err(Error::new(ErrorCode::DirtyWorkspaceConflict, "not empty"));
        }
        if self.baseline_tree_at(&path)?.is_some() {
            self.overlay.tombstone(&path)?;
        } else if self.overlay.lookup(&path).is_some() {
            self.overlay.clear(&path)?;
        } else {
            return Err(Error::new(ErrorCode::NotFound, "no such directory"));
        }
        self.inodes.unlink(&path);
        Ok(())
    }

    fn dir_is_empty(&self, path: &RepoPath) -> Result<bool> {
        if let Some(tree) = self.baseline_tree_at(path)? {
            for e in self.repo.store().read_tree(&tree, false)?.entries {
                let child = path.join(&e.name).map_err(|err| {
                    Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}"))
                })?;
                if !matches!(self.overlay.lookup(&child), Some(OverlayEntry::Tombstone)) {
                    return Ok(false);
                }
            }
        }
        for (_, e) in self.overlay.children(path) {
            if !matches!(e, OverlayEntry::Tombstone) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Create a symlink (FUSE `symlink`).
    pub fn symlink(&self, parent_ino: u64, name: &[u8], target: &[u8]) -> Result<Attr> {
        let path = self.child_path(parent_ino, name)?;
        self.overlay.put_symlink(&path, target)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok(Attr {
            ino,
            generation,
            size: target.len() as u64,
            kind: Kind::Symlink,
        })
    }

    /// Rename a file or symlink (FUSE `rename`). A clean baseline file moves as a
    /// metadata-only base-ref (no fetch, §29); an overlay file re-keys its
    /// content. Directory/subtree rename is a later refinement.
    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        newparent_ino: u64,
        newname: &[u8],
    ) -> Result<()> {
        let src = self.child_path(parent_ino, name)?;
        let dst = self.child_path(newparent_ino, newname)?;
        if self.overlay.lookup(&src).is_some() {
            self.overlay.rename(&src, &dst)?;
            if self.baseline_resolve(&src)?.is_some() {
                self.overlay.tombstone(&src)?;
            }
            self.inodes.rename(&src, &dst);
            return Ok(());
        }
        match self.baseline_resolve(&src)? {
            Some(Resolved::File {
                source: FileSource::Baseline(oid),
                executable,
            }) => {
                let mode = if executable {
                    GitMode::Executable
                } else {
                    GitMode::Regular
                };
                self.overlay.put_base_ref(&dst, oid, mode)?;
                self.overlay.tombstone(&src)?;
            }
            Some(Resolved::Symlink {
                source: SymSource::Baseline(oid),
            }) => {
                self.overlay.put_base_ref(&dst, oid, GitMode::Symlink)?;
                self.overlay.tombstone(&src)?;
            }
            Some(Resolved::Dir { .. }) => {
                return Err(Error::new(
                    ErrorCode::UnsupportedOperation,
                    "directory rename not yet supported",
                ));
            }
            _ => return Err(Error::new(ErrorCode::NotFound, "no such file")),
        }
        self.inodes.rename(&src, &dst);
        Ok(())
    }

    /// Set/clear the executable bit (FUSE `setattr` mode), copying up if needed.
    pub fn set_executable(&self, ino: u64, exec: bool) -> Result<()> {
        let path = self.path_of(ino)?;
        if matches!(self.overlay.lookup(&path), Some(OverlayEntry::File { .. })) {
            return self.overlay.set_executable(&path, exec);
        }
        if let Some(Resolved::File {
            source: FileSource::Baseline(oid),
            ..
        }) = self.resolve(&path)?
        {
            let seed = self.materialize_path(&oid)?;
            self.overlay.create_file(&path, exec, Some(&seed))?;
            return Ok(());
        }
        Err(Error::new(ErrorCode::Internal, "not a file"))
    }
}

/// A clean-rename base-ref resolves to a baseline blob at a new path (§29).
fn baseref_resolved(oid: ObjectId, mode: GitMode) -> Resolved {
    match mode {
        GitMode::Symlink => Resolved::Symlink {
            source: SymSource::Baseline(oid),
        },
        GitMode::Executable => Resolved::File {
            source: FileSource::Baseline(oid),
            executable: true,
        },
        _ => Resolved::File {
            source: FileSource::Baseline(oid),
            executable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_git_repo::CloneOptions;

    // The `SeededRemote` owns the bare repo's tempdir; it must outlive the
    // projection so a lazy blob fetch (e.g. for an exact size) can still reach
    // the promisor.
    fn projection_of(
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, glm_testkit::SeededRemote, Projection) {
        let remote = glm_testkit::seed_remote(files);
        let tmp = tempfile::tempdir().unwrap();
        let repo = AdminRepo::clone(
            &remote.url,
            &tmp.path().join("git"),
            &tmp.path().join("mnt"),
            &tmp.path().join("anchor"),
            &CloneOptions::default(),
        )
        .unwrap();
        let proj =
            Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap();
        (tmp, remote, proj)
    }

    #[test]
    fn root_readdir_lists_tree_entries_plus_synthetic_git() {
        let (_t, _r, p) = projection_of(&[("README.md", b"hi\n"), ("src/main.rs", b"x\n")]);
        let names: Vec<_> = p
            .readdir(p.root_ino())
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), e.kind))
            .collect();
        assert!(names.contains(&("README.md".into(), Kind::File { executable: false })));
        assert!(names.contains(&("src".into(), Kind::Dir)));
        assert!(names.contains(&(".git".into(), Kind::File { executable: false })));
    }

    #[test]
    fn synthetic_git_is_a_single_protected_regular_file_at_root() {
        // The root `.git` is the synthetic gitfile (a regular file → our gitdir),
        // listed exactly once (redesign.md §6). The malicious case — a repo tree
        // that itself contains a `.git` entry shadowed by the synthetic one — is
        // covered by the mount integration tests (it requires a plumbing-built
        // tree, since `git add .git` is impossible in a normal working tree).
        let (_t, _r, p) = projection_of(&[("ok.txt", b"y\n")]);
        let git_entries: Vec<_> = p
            .readdir(p.root_ino())
            .unwrap()
            .into_iter()
            .filter(|e| e.name == b".git")
            .collect();
        assert_eq!(git_entries.len(), 1, "exactly one .git");
        assert_eq!(git_entries[0].kind, Kind::File { executable: false });
        let a = p.lookup(p.root_ino(), b".git").unwrap().unwrap();
        assert_eq!(a.kind, Kind::File { executable: false });
        assert_eq!(a.size, p.gitfile_bytes().len() as u64);
        assert!(p.gitfile_bytes().starts_with(b"gitdir: "));
    }

    #[test]
    fn lookup_resolves_nested_paths_and_getattr_is_stable() {
        let (_t, _r, p) = projection_of(&[("src/main.rs", b"fn main() {}\n")]);
        let src = p.lookup(p.root_ino(), b"src").unwrap().unwrap();
        assert_eq!(src.kind, Kind::Dir);
        let main = p.lookup(src.ino, b"main.rs").unwrap().unwrap();
        assert_eq!(main.kind, Kind::File { executable: false });
        assert_eq!(main.size, 13); // exact size, not faked (§21)
                                   // getattr returns the same stable identity.
        let again = p.getattr(main.ino).unwrap();
        assert_eq!(again.ino, main.ino);
        assert_eq!(again.generation, main.generation);
        assert_eq!(again.size, 13);
        // A missing path resolves to None, not an error.
        assert!(p.lookup(p.root_ino(), b"nope").unwrap().is_none());
    }

    #[test]
    fn open_content_serves_blob_bytes_from_a_cache_fd() {
        let body: &[u8] = b"line one\nline two\nthe quick brown fox\n";
        let (_t, _r, p) = projection_of(&[("doc.txt", body)]);
        let a = p.lookup(p.root_ino(), b"doc.txt").unwrap().unwrap();
        let h = p.open_content(a.ino).unwrap();
        assert_eq!(h.size().unwrap(), body.len() as u64);
        // full read
        assert_eq!(h.read_at(0, body.len()).unwrap(), body);
        // a bounded range read (offset + len), not the whole file
        assert_eq!(h.read_at(9, 8).unwrap(), b"line two");
        // read past EOF returns a short read, not an error
        assert!(h.read_at(body.len() as u64, 16).unwrap().is_empty());
        // a second open hits the published cache file (idempotent identity)
        let h2 = p.open_content(a.ino).unwrap();
        assert_eq!(h2.read_at(0, body.len()).unwrap(), body);
    }

    #[test]
    fn synthetic_git_content_is_the_gitfile() {
        let (_t, _r, p) = projection_of(&[("ok.txt", b"y\n")]);
        let a = p.lookup(p.root_ino(), b".git").unwrap().unwrap();
        let h = p.open_content(a.ino).unwrap();
        let bytes = h.read_at(0, 4096).unwrap();
        assert_eq!(bytes, p.gitfile_bytes());
        assert!(bytes.starts_with(b"gitdir: "));
    }

    #[test]
    #[cfg(unix)]
    fn readlink_returns_raw_symlink_target() {
        // seed a symlink via a tree the normal way: testkit writes files, but a
        // symlink needs git to record mode 120000 — so write the link in the
        // seed working tree through a path that git stores as a symlink.
        let remote = glm_testkit::seed_remote_symlink("link", "target/path");
        let tmp = tempfile::tempdir().unwrap();
        let repo = AdminRepo::clone(
            &remote.url,
            &tmp.path().join("git"),
            &tmp.path().join("mnt"),
            &tmp.path().join("anchor"),
            &CloneOptions::default(),
        )
        .unwrap();
        let p =
            Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap();
        let a = p.lookup(p.root_ino(), b"link").unwrap().unwrap();
        assert_eq!(a.kind, Kind::Symlink);
        assert_eq!(p.readlink(a.ino).unwrap(), b"target/path");
    }

    #[test]
    fn overlay_create_appears_in_resolve_and_readdir() {
        use std::io::Write;
        let (_t, _r, p) = projection_of(&[("README.md", b"base\n")]);
        let (a, mut f) = p.create(p.root_ino(), b"new.txt", false).unwrap();
        f.write_all(b"created").unwrap();
        drop(f);
        let look = p.lookup(p.root_ino(), b"new.txt").unwrap().unwrap();
        assert_eq!(look.kind, Kind::File { executable: false });
        assert_eq!(look.size, 7);
        assert_eq!(
            p.open_content(a.ino).unwrap().read_at(0, 100).unwrap(),
            b"created"
        );
        let names: Vec<_> = p
            .readdir(p.root_ino())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == b"README.md"));
        assert!(names.iter().any(|n| n == b"new.txt"));
    }

    #[test]
    fn unlink_baseline_tombstones_and_hides_from_readdir() {
        let (_t, _r, p) = projection_of(&[("a.txt", b"x\n"), ("b.txt", b"y\n")]);
        p.unlink(p.root_ino(), b"a.txt").unwrap();
        assert!(p.lookup(p.root_ino(), b"a.txt").unwrap().is_none());
        let names: Vec<_> = p
            .readdir(p.root_ino())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(
            !names.iter().any(|n| n == b"a.txt"),
            "tombstoned file hidden"
        );
        assert!(names.iter().any(|n| n == b"b.txt"));
        // unlink of a missing file is ENOENT.
        assert_eq!(p.unlink(p.root_ino(), b"nope").unwrap_err().errno(), 2);
    }

    #[test]
    fn cow_edit_of_a_baseline_file_reads_back_merged() {
        use std::io::{Seek, SeekFrom, Write};
        let (_t, _r, p) = projection_of(&[("f.txt", b"BASE\n")]);
        let ino = p.lookup(p.root_ino(), b"f.txt").unwrap().unwrap().ino;
        let mut f = p.open_write(ino, false).unwrap(); // no truncate → copy-up
        f.seek(SeekFrom::Start(5)).unwrap();
        f.write_all(b"MORE").unwrap();
        drop(f);
        assert_eq!(
            p.open_content(ino).unwrap().read_at(0, 100).unwrap(),
            b"BASE\nMORE"
        );
        // a fresh status would see this as modified; the baseline is untouched.
        assert_eq!(p.getattr(ino).unwrap().size, 9);
    }

    #[test]
    fn mkdir_symlink_and_clean_rename_without_fetch() {
        let (_t, _r, p) = projection_of(&[("orig.txt", b"hi\n")]);
        let d = p.mkdir(p.root_ino(), b"newdir").unwrap();
        assert_eq!(d.kind, Kind::Dir);
        assert!(p.lookup(p.root_ino(), b"newdir").unwrap().is_some());
        assert_eq!(p.mkdir(p.root_ino(), b"newdir").unwrap_err().errno(), 17); // EEXIST

        let s = p.symlink(p.root_ino(), b"ln", b"orig.txt").unwrap();
        assert_eq!(s.kind, Kind::Symlink);
        assert_eq!(p.readlink(s.ino).unwrap(), b"orig.txt");

        let before = p.hydrations();
        p.rename(p.root_ino(), b"orig.txt", p.root_ino(), b"renamed.txt")
            .unwrap();
        assert!(p.lookup(p.root_ino(), b"orig.txt").unwrap().is_none());
        let r = p.lookup(p.root_ino(), b"renamed.txt").unwrap().unwrap();
        assert_eq!(r.kind, Kind::File { executable: false });
        assert_eq!(p.hydrations(), before, "clean rename fetched no blob (§29)");
        // content still readable through the renamed path
        assert_eq!(
            p.open_content(r.ino).unwrap().read_at(0, 100).unwrap(),
            b"hi\n"
        );
    }
}
