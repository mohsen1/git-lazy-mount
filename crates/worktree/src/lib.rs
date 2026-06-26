//! Virtual working-tree projection.
//!
//! The projected working tree is the durable [`overlay`] layered over the HEAD
//! commit's tree (the *baseline*), plus a protected synthetic `.git` gitfile at
//! the root. Path resolution order:
//! 1. synthetic `.git` (root) — shadows any entry, fails-safe
//! 2. overlay file / symlink / dir / clean-rename base-ref
//! 3. overlay tombstone (incl. a tombstoned ancestor) → absent
//! 4. baseline Git tree entry
//! 5. absent
//!
//! Invariants enforced here (each covered by a test):
//! * `readdir` returns names + kind only — it **never** reads blob contents or
//!   resolves exact sizes; it merges baseline + overlay children.
//! * a repo `.git` tree entry never shadows the synthetic one.
//! * writes copy up once then write in place (no full rewrite);
//!   `O_TRUNC`/create and clean renames fetch **no** blob.
//! * resolution + listing cost is O(direct children), independent of repo size.

#![forbid(unsafe_code)]

pub mod journal;
pub mod overlay;
pub use overlay::{Overlay, OverlayEntry};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use glm_core::{Error, ErrorCode, GitMode, ObjectId, RepoPath, Result};
use glm_fs_common::{InodeTable, Pool, ROOT_INO};
use glm_git_repo::AdminRepo;

/// Uniquifier for temporary cache files during atomic publish.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Worker threads for the speculative sibling size-prefetch. Bounded and separate
/// from the FUSE callback pools, so prefetch never consumes a callback thread.
/// (A foreground read can still briefly wait on a prefetch of the *same* oid via
/// the shared per-oid `inflight` single-flight — one `cat-file -s` — which is
/// desired: it coalesces the size-fault and the content-fault into one fetch.)
const PREFETCH_THREADS: usize = 8;
/// Stats observed in one directory before its sibling prefetch is armed. Keeps an
/// isolated `cat`/`stat` of a single file from faulting the whole directory,
/// while a real stat-walk (`git add -A`, the untracked scan) crosses it quickly.
const PREFETCH_DIR_THRESHOLD: u32 = 4;
/// Cap on siblings warmed per directory, bounding the speculative fetch.
const PREFETCH_DIR_CAP: usize = 512;
/// Total bytes the speculative prefetch may fault over a mount's life. `git
/// add -A`/`status` walk the *whole* tree, so without this every directory would
/// arm and the mount would eagerly materialize the entire repo — defeating
/// laziness. The budget caps the speculative over-fetch (the hot first stretch of
/// a walk is parallelized; past it, git falls back to its own serial faults),
/// keeping a big-repo mount far smaller than a clone.
const PREFETCH_BYTE_BUDGET: u64 = 32 * 1024 * 1024;

/// The reserved name of the synthetic gitfile at the projection root.
pub const GITFILE_NAME: &[u8] = b".git";

/// `RENAME_NOREPLACE` rename flag — fail if the destination exists (Linux value).
pub const RENAME_NOREPLACE: u32 = 1;
/// `RENAME_EXCHANGE` rename flag — atomic swap (Linux value); not supported.
pub const RENAME_EXCHANGE: u32 = 2;

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

/// Neutral, stable attributes for a projected entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attr {
    /// Inode number.
    pub ino: u64,
    /// Inode generation.
    pub generation: u64,
    /// Exact byte size (0 for directories).
    pub size: u64,
    /// Last-modified time. Overlay files report their real on-disk mtime so git's
    /// stat cache / racy-clean logic detects in-place edits (including *same-size*
    /// edits); baseline entries and directories report a stable epoch.
    pub mtime: SystemTime,
    /// Entry kind.
    pub kind: Kind,
}

/// One directory listing entry — name + kind + inode only (no size).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Exact recorded name bytes.
    pub name: Vec<u8>,
    /// Entry kind.
    pub kind: Kind,
    /// Stable inode.
    pub ino: u64,
}

/// What a path resolves to after layering the overlay over the baseline.
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
    /// The durable writable overlay layered over the baseline.
    overlay: Overlay,
    /// Content-addressed cache directory for materialized blob bytes.
    cache_dir: PathBuf,
    /// Count of content hydrations (cache-miss blob materializations) — the
    /// signal behind the budget assertions (`ls` = 0, `cat` ≥ 1).
    hydrations: AtomicU64,
    /// Whether `getattr` may fault an object in to learn its exact size. INTERIM: this uses
    /// git's lazy fetch; once the bounded fetch scheduler exists it must route
    /// through it so that "only the scheduler causes network" holds.
    metadata_fetch: bool,
    /// Single-flight locks per object id so N concurrent faults of one missing
    /// blob cause exactly one retrieval — the first holder fetches,
    /// the rest wait and reuse the published cache file.
    inflight: Mutex<HashMap<ObjectId, Arc<Mutex<()>>>>,
    /// Memoized exact size per baseline blob oid. A blob's size is immutable, so
    /// this never goes stale; populated by `getattr` size-faults and by the
    /// speculative sibling prefetch so later stats in a walk are cache hits.
    size_cache: Mutex<HashMap<ObjectId, u64>>,
    /// Count of baseline size-faults (each a `cat-file -s` that pulls the blob
    /// under tree:0). Distinct from `hydrations` (content materializations),
    /// which stays ~0 during a stat-walk — this is the signal behind the
    /// prefetch's effect.
    size_faults: AtomicU64,
    /// Directories whose sibling sizes have already been prefetched (arm once).
    prefetched_dirs: Mutex<HashSet<RepoPath>>,
    /// Per-directory stat counter; the prefetch arms once it crosses
    /// `PREFETCH_DIR_THRESHOLD` (evidence of a stat-walk, not a one-off stat).
    dir_stat_counts: Mutex<HashMap<RepoPath, u32>>,
    /// Bounded pool running the off-callback sibling size-faults in parallel,
    /// so git's serial lstat walk hits warm sizes instead of faulting per file.
    prefetch_pool: Pool,
    /// Total bytes faulted by the speculative prefetch so far, capped at
    /// `PREFETCH_BYTE_BUDGET` so a whole-tree walk cannot eagerly fetch the repo.
    prefetch_bytes: AtomicU64,
    /// Optional FSMonitor change journal. When present, every worktree
    /// mutation is recorded **synchronously** (before the FUSE reply) so the
    /// `git-lazy-mount-fsmonitor` hook, reading the same durable log, always sees
    /// every acknowledged change — no false negatives.
    journal: Option<journal::ChangeJournal>,
    // No global lock: the InodeTable, Overlay, and the content cache are each
    // internally synchronized, so callbacks never serialize behind a coarse mutex
    // held across a blocking `git` subprocess.
}

/// An open read handle backing a `read` callback. Content is served from a file
/// descriptor (a cache file) or, for the tiny synthetic `.git`, from memory —
/// **never** by allocating the whole blob.
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
    /// size), never proportional to the file size.
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

/// Positional read (`pread`). git-lazy-mount targets Linux only.
fn pread(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, offset)
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
            inflight: Mutex::new(HashMap::new()),
            size_cache: Mutex::new(HashMap::new()),
            size_faults: AtomicU64::new(0),
            prefetched_dirs: Mutex::new(HashSet::new()),
            dir_stat_counts: Mutex::new(HashMap::new()),
            prefetch_pool: Pool::new(PREFETCH_THREADS),
            prefetch_bytes: AtomicU64::new(0),
            journal: None,
        })
    }

    /// Attach an FSMonitor change journal so worktree mutations are recorded for
    /// the `core.fsmonitor` hook. Call before wrapping in an `Arc`/mounting.
    pub fn with_journal(mut self, journal: journal::ChangeJournal) -> Projection {
        self.journal = Some(journal);
        self
    }

    /// Record a worktree mutation in the journal (if attached): the path plus its
    /// parent directory. Inclusive — an extra
    /// path only costs git an `lstat`; a missing one would corrupt `status`.
    ///
    /// Propagates the journal write error. Every mutation handler calls this
    /// **before** applying its mutation (after any validation/early-return), so a
    /// journal failure fails the FUSE op rather than applying an un-journaled
    /// (false-negative) mutation. Over-reporting is safe — if the record succeeds
    /// but the mutation then fails, git just `lstat`s an unchanged path — so
    /// record-before-mutate is correct.
    fn record_change(&self, path: &RepoPath) -> Result<()> {
        if let Some(j) = &self.journal {
            j.record(path.as_bytes())?;
            if let Some(parent) = path.parent() {
                if !parent.is_root() {
                    j.record(parent.as_bytes())?;
                }
            }
        }
        Ok(())
    }

    /// Number of content hydrations so far (cache-miss blob materializations).
    pub fn hydrations(&self) -> u64 {
        self.hydrations.load(Ordering::Relaxed)
    }

    /// Release `n` kernel lookup references on `ino`.
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
    ///: synthetic `.git` → overlay entry → overlay tombstone (incl. an
    /// ancestor) → baseline tree → absent. Reads only trees, never blob contents
    ///.
    fn resolve(&self, path: &RepoPath) -> Result<Option<Resolved>> {
        if path.is_root() {
            return Ok(Some(Resolved::Dir {
                baseline_tree: Some(self.baseline_tree.clone()),
            }));
        }
        let comps: Vec<&[u8]> = path.components().collect();
        // The synthetic root `.git` shadows any entry of the same name.
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

    /// Memoized exact size of a baseline blob. The size of an object id is
    /// immutable, so the cache is always sound. Single-flighted through the same
    /// per-oid `inflight` lock as content materialization, so a real `getattr`
    /// and a speculative sibling prefetch of the same oid fault the blob exactly
    /// once (and that one `cat-file -s` also warms the blob into the promisor
    /// store, so a following read finds it local).
    fn cached_size(&self, oid: &ObjectId) -> Result<u64> {
        if let Some(sz) = self
            .size_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(oid)
            .copied()
        {
            return Ok(sz);
        }
        let lock = {
            let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(map.entry(oid.clone()).or_default())
        };
        let _g = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sz) = self
            .size_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(oid)
            .copied()
        {
            return Ok(sz); // another caller faulted it while we waited
        }
        let sz = self.repo.store().object_size(oid, self.metadata_fetch)?;
        self.size_faults.fetch_add(1, Ordering::Relaxed);
        self.size_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(oid.clone(), sz);
        Ok(sz)
    }

    /// Count of baseline size-faults so far (for measurement/tests).
    pub fn size_faults(&self) -> u64 {
        self.size_faults.load(Ordering::Relaxed)
    }

    /// Speculatively warm the sizes of `ino`'s sibling baseline files in
    /// parallel. git's working-tree/untracked walk (`git add -A`, `git status`)
    /// `lstat`s files **serially**, and on a `tree:0` mount each `lstat` faults
    /// that blob's size one at a time. Triggered from a `getattr`/`lookup` of a
    /// File/Symlink, this gets the mount **ahead** of that serial walk: once a
    /// few stats land in a directory (evidence of a walk, not a one-off `cat`),
    /// it fans out the rest of the directory's size-faults across the prefetch
    /// pool, so git's subsequent `lstat`s are `size_cache` hits.
    ///
    /// It is deliberately *not* hooked into `readdir` (which by invariant returns
    /// names only): a plain `ls` does no `getattr`, so it never triggers this and
    /// never over-fetches; and the per-directory threshold keeps a single
    /// `cat`/`stat` below the arming point.
    pub fn prefetch_siblings(self: &Arc<Self>, ino: u64) {
        let Ok(path) = self.path_of(ino) else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        {
            let mut counts = self
                .dir_stat_counts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let c = counts.entry(parent.clone()).or_insert(0);
            *c += 1;
            if *c < PREFETCH_DIR_THRESHOLD {
                return;
            }
        }
        if self.prefetch_bytes.load(Ordering::Relaxed) >= PREFETCH_BYTE_BUDGET {
            return; // speculative budget spent; let git fault the rest itself
        }
        if !self
            .prefetched_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(parent.clone())
        {
            return; // already prefetched this directory
        }
        // Capture a Weak, not a strong Arc: a prefetch job must never be the last
        // owner of the Projection (that would drop it — and its prefetch pool —
        // from a pool worker thread). It also lets queued work no-op promptly
        // once the mount is torn down.
        let weak = Arc::downgrade(self);
        self.prefetch_pool.spawn(move || {
            if let Some(me) = weak.upgrade() {
                me.warm_dir_siblings(&parent);
            }
        });
    }

    /// Read `parent`'s baseline tree (already local — no fetch) and fan out a
    /// bounded, deduplicated parallel size-fault for each sibling blob not yet
    /// cached. Best-effort: every job ignores its error.
    fn warm_dir_siblings(self: &Arc<Self>, parent: &RepoPath) {
        let tree = match self.resolve(parent) {
            Ok(Some(Resolved::Dir {
                baseline_tree: Some(t),
            })) => t,
            _ => return,
        };
        let Ok(obj) = self.repo.store().read_tree(&tree, false) else {
            return;
        };
        let mut n = 0usize;
        for e in obj.entries {
            if n >= PREFETCH_DIR_CAP
                || self.prefetch_bytes.load(Ordering::Relaxed) >= PREFETCH_BYTE_BUDGET
            {
                break;
            }
            // Only blobs pay a size-fault; trees/gitlinks report size 0.
            match e.mode {
                GitMode::Regular | GitMode::Executable | GitMode::Symlink => {}
                _ => continue,
            }
            let Ok(child) = parent.join(&e.name) else {
                continue;
            };
            if self.overlay.lookup(&child).is_some() {
                continue; // an overlay entry decides this name
            }
            let oid = e.object_id.clone();
            if self
                .size_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&oid)
            {
                continue;
            }
            n += 1;
            let weak = Arc::downgrade(self);
            self.prefetch_pool.spawn(move || {
                let Some(me) = weak.upgrade() else { return };
                // Re-check the budget at run time: the enqueue-time check reads a
                // counter that lags the in-flight faults, so this caps the
                // overshoot to the concurrent batch instead of a whole wide
                // directory's worth of blobs.
                if me.prefetch_bytes.load(Ordering::Relaxed) >= PREFETCH_BYTE_BUDGET {
                    return;
                }
                if let Ok(sz) = me.cached_size(&oid) {
                    me.prefetch_bytes.fetch_add(sz, Ordering::Relaxed);
                }
            });
        }
    }

    fn attr_of(&self, ino: u64, generation: u64, r: &Resolved) -> Result<Attr> {
        let (kind, size, mtime) = match r {
            Resolved::Dir { .. } => (Kind::Dir, 0, UNIX_EPOCH),
            Resolved::Gitfile => (
                Kind::File { executable: false },
                self.gitfile.len() as u64,
                UNIX_EPOCH,
            ),
            Resolved::File { source, executable } => {
                let (size, mtime) = match source {
                    // Overlay files carry their real mtime so git detects in-place
                    // edits — including same-size ones — via its racy-clean logic.
                    FileSource::Overlay(p) => (
                        self.overlay.content_size(p)?,
                        self.overlay.content_mtime(p)?,
                    ),
                    FileSource::Baseline(oid) => (self.cached_size(oid)?, UNIX_EPOCH),
                };
                (
                    Kind::File {
                        executable: *executable,
                    },
                    size,
                    mtime,
                )
            }
            Resolved::Symlink { source } => {
                let size = match source {
                    SymSource::Overlay(t) => t.len() as u64,
                    SymSource::Baseline(oid) => self.cached_size(oid)?,
                };
                (Kind::Symlink, size, UNIX_EPOCH)
            }
        };
        Ok(Attr {
            ino,
            generation,
            size,
            mtime,
            kind,
        })
    }

    /// `lookup(parent, name)` — resolve a child by name. Allocates a stable
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

    /// `getattr(ino)`. May fault an object in for its exact size.
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
    /// resolves **no** sizes. Cost is O(direct children).
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
        // the root `.git` (shadowed by the synthetic one).
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

    /// Open content for reading. A clean tracked blob is materialized
    /// once into a content-addressed cache file (atomic publish) and then served
    /// by `pread` from its FD; the synthetic `.git` is served from memory.
    /// Faults the blob in on first access (a later refinement routes this through
    /// the bounded fetch scheduler so "only the scheduler causes network").
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
        let cached = self.materialize_path(oid)?;
        std::fs::File::open(&cached)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("open cache file: {e}")))
    }

    /// Ensure the blob is present in the content-addressed cache and return its
    /// path (used to seed a copy-up). The cache is keyed by oid.
    fn materialize_path(&self, oid: &ObjectId) -> Result<PathBuf> {
        let final_path = self.cache_dir.join(oid.to_hex());
        if final_path.exists() {
            return Ok(final_path); // fast path: already published
        }
        // Single-flight per oid: the first caller fetches under the
        // per-oid lock; concurrent callers block, then find the published file.
        let lock = {
            let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(map.entry(oid.clone()).or_default())
        };
        let _g = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if final_path.exists() {
            return Ok(final_path); // another caller published it while we waited
        }
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .cache_dir
            .join(format!(".{}.{}.tmp", oid.to_hex(), seq));
        self.hydrations.fetch_add(1, Ordering::Relaxed);
        self.repo.store().blob_to_file(oid, true, &tmp)?;
        if let Err(e) = std::fs::rename(&tmp, &final_path) {
            let _ = std::fs::remove_file(&tmp);
            if !final_path.exists() {
                return Err(Error::new(
                    ErrorCode::Internal,
                    format!("publish cache file: {e}"),
                ));
            }
        }
        Ok(final_path)
    }

    /// `readlink(ino)` — the symlink's raw target bytes. Targets are
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

    // ---- write path ---------------------------------------

    fn child_path(&self, parent_ino: u64, name: &[u8]) -> Result<RepoPath> {
        let path = self
            .path_of(parent_ino)?
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        // The synthetic root `.git` is protected from creation/replacement/
        // deletion/rename; reads always resolve to the gitfile.
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
        // Record before mutating: a journal failure fails the op rather than
        // creating an un-journaled file (a false negative for `git status`).
        self.record_change(&path)?;
        let file = self.overlay.create_file(&path, executable, None)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok((
            Attr {
                ino,
                generation,
                size: 0,
                mtime: self.overlay.content_mtime(&path).unwrap_or(UNIX_EPOCH),
                kind: Kind::File { executable },
            },
            file,
        ))
    }

    /// Open an existing file for writing (FUSE `open` with write intent). Copies
    /// the baseline up once; `truncate` seeds an empty file with **no**
    /// baseline fetch.
    pub fn open_write(&self, ino: u64, truncate: bool) -> Result<std::fs::File> {
        let path = self.path_of(ino)?;
        self.record_change(&path)?; // write intent — inclusive
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
    /// needed. `size == 0` never fetches the old blob.
    pub fn truncate(&self, ino: u64, size: u64) -> Result<()> {
        let path = self.path_of(ino)?;
        let is_overlay_file = matches!(self.overlay.lookup(&path), Some(OverlayEntry::File { .. }));
        // Validate before any mutation: a non-file is rejected here, so the record
        // below never over-reports on a rejected op.
        let baseline = if is_overlay_file {
            None
        } else {
            match self.resolve(&path)? {
                Some(Resolved::File {
                    source: FileSource::Baseline(oid),
                    executable,
                }) => Some((oid, executable)),
                _ => return Err(Error::new(ErrorCode::Internal, "not a file")),
            }
        };
        // Record before mutating (copy-up / set_len). Over-reporting is safe.
        self.record_change(&path)?;
        let f = if let Some((oid, executable)) = baseline {
            let seed = if size == 0 {
                None
            } else {
                Some(self.materialize_path(&oid)?)
            };
            self.overlay
                .create_file(&path, executable, seed.as_deref())?
        } else {
            self.overlay.open_content(&path)?
        };
        f.set_len(size)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("set_len: {e}")))?;
        Ok(())
    }

    /// Remove a file/symlink (FUSE `unlink`): tombstone a baseline path, else
    /// drop the overlay-only entry.
    pub fn unlink(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let path = self.child_path(parent_ino, name)?;
        // Validate existence before any mutation so the record below never
        // over-reports on a rejected (no-such-file) op.
        let on_baseline = self.baseline_resolve(&path)?.is_some();
        if !on_baseline && self.overlay.lookup(&path).is_none() {
            return Err(Error::new(ErrorCode::NotFound, "no such file"));
        }
        // Record before mutating. Over-reporting is safe.
        self.record_change(&path)?;
        if on_baseline {
            self.overlay.tombstone(&path)?;
        } else {
            self.overlay.clear(&path)?;
        }
        // Drop the name→inode mapping so a later recreate gets a fresh inode and
        // the unlinked inode enters open-unlinked retention.
        self.inodes.unlink(&path);
        Ok(())
    }

    /// Create a directory (FUSE `mkdir`); persisted so empty dirs survive.
    pub fn mkdir(&self, parent_ino: u64, name: &[u8]) -> Result<Attr> {
        let path = self.child_path(parent_ino, name)?;
        if self.resolve(&path)?.is_some() {
            return Err(Error::new(ErrorCode::AlreadyExists, "exists"));
        }
        // Record before mutating (after the exists-check rejection). Over-
        // reporting is safe; a journal failure must fail the op, not leave an
        // un-journaled directory.
        self.record_change(&path)?;
        self.overlay.put_dir(&path)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok(Attr {
            ino,
            generation,
            size: 0,
            mtime: UNIX_EPOCH,
            kind: Kind::Dir,
        })
    }

    /// Remove a directory if empty (FUSE `rmdir`).
    pub fn rmdir(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let path = self.child_path(parent_ino, name)?;
        if !self.dir_is_empty(&path)? {
            return Err(Error::new(ErrorCode::DirtyWorkspaceConflict, "not empty"));
        }
        // Validate existence before any mutation so the record below never
        // over-reports on a rejected (no-such-directory) op.
        let on_baseline = self.baseline_tree_at(&path)?.is_some();
        if !on_baseline && self.overlay.lookup(&path).is_none() {
            return Err(Error::new(ErrorCode::NotFound, "no such directory"));
        }
        // Record before mutating. Over-reporting is safe.
        self.record_change(&path)?;
        if on_baseline {
            self.overlay.tombstone(&path)?;
        } else {
            self.overlay.clear(&path)?;
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
        // Record before mutating: a journal failure fails the op rather than
        // creating an un-journaled symlink (a false negative for `git status`).
        self.record_change(&path)?;
        self.overlay.put_symlink(&path, target)?;
        let (ino, generation) = self.inodes.lookup(&path);
        Ok(Attr {
            ino,
            generation,
            size: target.len() as u64,
            mtime: UNIX_EPOCH,
            kind: Kind::Symlink,
        })
    }

    /// Rename a file, symlink, or directory (FUSE `rename`). A clean baseline file
    /// moves as a metadata-only base-ref (no fetch); an overlay file re-keys
    /// its content; a directory moves its whole subtree (overlay descendants
    /// re-keyed, baseline descendants re-pointed as base-refs, the source subtree
    /// tombstoned) — all metadata-only, no blob fetch. `flags` honors
    /// `RENAME_NOREPLACE` (fail if the destination exists) and rejects
    /// `RENAME_EXCHANGE` as unsupported.
    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        newparent_ino: u64,
        newname: &[u8],
        flags: u32,
    ) -> Result<()> {
        let src = self.child_path(parent_ino, name)?;
        let dst = self.child_path(newparent_ino, newname)?;
        if flags & RENAME_EXCHANGE != 0 {
            return Err(Error::new(
                ErrorCode::UnsupportedOperation,
                "RENAME_EXCHANGE is not supported",
            ));
        }
        if flags & RENAME_NOREPLACE != 0 && self.resolve(&dst)?.is_some() {
            return Err(Error::new(ErrorCode::AlreadyExists, "destination exists"));
        }
        self.record_change(&src)?;
        self.record_change(&dst)?;
        // A directory moves its whole subtree.
        if matches!(self.resolve(&src)?, Some(Resolved::Dir { .. })) {
            return self.rename_dir(&src, &dst);
        }
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
            _ => return Err(Error::new(ErrorCode::NotFound, "no such file")),
        }
        self.inodes.rename(&src, &dst);
        Ok(())
    }

    /// Move a directory `src` to `dst`, recursively (helper for [`rename`]).
    /// Overlay descendants re-key (content moves with them); baseline descendants
    /// become base-refs at the destination (no blob fetch); the source subtree is
    /// then tombstoned so the baseline beneath it is hidden. Metadata-only.
    fn rename_dir(&self, src: &RepoPath, dst: &RepoPath) -> Result<()> {
        self.overlay.put_dir(dst)?;
        for (name, resolved) in self.effective_children(src)? {
            let cs = self.join(src, &name)?;
            let cd = self.join(dst, &name)?;
            match resolved {
                Resolved::Dir { .. } => {
                    self.rename_dir(&cs, &cd)?;
                    continue; // recursion re-keys inodes + tombstones `cs`
                }
                Resolved::File {
                    source: FileSource::Overlay(_),
                    ..
                }
                | Resolved::Symlink {
                    source: SymSource::Overlay(_),
                } => {
                    self.overlay.rename(&cs, &cd)?;
                }
                Resolved::File {
                    source: FileSource::Baseline(oid),
                    executable,
                } => {
                    let mode = if executable {
                        GitMode::Executable
                    } else {
                        GitMode::Regular
                    };
                    self.overlay.put_base_ref(&cd, oid, mode)?;
                }
                Resolved::Symlink {
                    source: SymSource::Baseline(oid),
                } => {
                    self.overlay.put_base_ref(&cd, oid, GitMode::Symlink)?;
                }
                Resolved::Gitfile => continue, // cannot occur below the root
            }
            self.inodes.rename(&cs, &cd);
        }
        self.overlay.tombstone(src)?;
        self.inodes.rename(src, dst);
        Ok(())
    }

    /// The effective (overlay-over-baseline) children of a directory path, each
    /// with its resolved source. Mirrors [`readdir`](Self::readdir) but returns
    /// [`Resolved`] for recursive subtree walks.
    fn effective_children(&self, dir: &RepoPath) -> Result<Vec<(Vec<u8>, Resolved)>> {
        let Some(Resolved::Dir { baseline_tree }) = self.resolve(dir)? else {
            return Err(Error::new(ErrorCode::Internal, "not a directory"));
        };
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        if let Some(tree) = baseline_tree {
            for e in self.repo.store().read_tree(&tree, false)?.entries {
                let child = self.join(dir, &e.name)?;
                if self.overlay.lookup(&child).is_some() {
                    continue; // an overlay entry decides this name below
                }
                if seen.insert(e.name.clone()) {
                    names.push(e.name);
                }
            }
        }
        for (name, entry) in self.overlay.children(dir) {
            if matches!(entry, OverlayEntry::Tombstone) {
                continue;
            }
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let child = self.join(dir, &name)?;
            if let Some(r) = self.resolve(&child)? {
                out.push((name, r));
            }
        }
        Ok(out)
    }

    /// Join a child name onto a directory path, mapping a path error uniformly.
    fn join(&self, dir: &RepoPath, name: &[u8]) -> Result<RepoPath> {
        dir.join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))
    }

    /// Set/clear the executable bit (FUSE `setattr` mode), copying up if needed.
    pub fn set_executable(&self, ino: u64, exec: bool) -> Result<()> {
        let path = self.path_of(ino)?;
        self.record_change(&path)?;
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

/// A clean-rename base-ref resolves to a baseline blob at a new path.
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
        // tree:0 clone fetches no trees; fault the HEAD trees in before projecting.
        repo.build_index().unwrap();
        let proj =
            Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap();
        (tmp, remote, proj)
    }

    // Like `projection_of` but with an FSMonitor change journal attached, so
    // worktree mutations are recorded (and a record failure can be injected).
    fn projection_with_journal_of(
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
        // tree:0 clone fetches no trees; fault the HEAD trees in before projecting.
        repo.build_index().unwrap();
        let j = journal::ChangeJournal::open(tmp.path().join("journal"), "ws", 1, 0).unwrap();
        let proj = Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay"))
            .unwrap()
            .with_journal(j);
        (tmp, remote, proj)
    }

    // A directory of `n` baseline files named dir/fNN.txt.
    fn dir_of(n: usize) -> Vec<(String, Vec<u8>)> {
        (0..n)
            .map(|i| {
                (
                    format!("dir/f{i:02}.txt"),
                    format!("contents-of-file-{i}\n").into_bytes(),
                )
            })
            .collect()
    }

    // Wait until at least `target` size-faults have landed (the prefetch is
    // async), bounded by a ~10s timeout. Polling to a known target — not to
    // "stable" — is deterministic: a momentary stall mid-prefetch can't make it
    // return early.
    fn await_faults(p: &Projection, target: u64) -> u64 {
        for _ in 0..500 {
            if p.size_faults() >= target {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        p.size_faults()
    }

    #[test]
    fn stat_walk_warms_siblings_without_hydrating() {
        // Stat-ing past the per-directory threshold arms a speculative prefetch
        // that size-faults the REST of the directory in parallel (so git's serial
        // lstat walk hits warm sizes), via object_size — never content
        // materialization, so hydrations() stays 0.
        const N: usize = 40;
        let files = dir_of(N);
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(s, c)| (s.as_str(), c.as_slice()))
            .collect();
        let (_t, _r, p) = projection_of(&refs);
        let p = Arc::new(p);
        let dir = p.lookup(p.root_ino(), b"dir").unwrap().unwrap().ino;

        // Stat exactly THRESHOLD files; the mount calls prefetch_siblings after
        // each getattr. The last one crosses the threshold and arms the prefetch.
        for i in 0..PREFETCH_DIR_THRESHOLD {
            let name = format!("f{i:02}.txt");
            let ino = p.lookup(dir, name.as_bytes()).unwrap().unwrap().ino;
            p.getattr(ino).unwrap();
            p.prefetch_siblings(ino);
        }

        let faults = await_faults(&p, N as u64);
        assert_eq!(
            faults, N as u64,
            "prefetch warmed every sibling exactly once"
        );
        assert_eq!(
            p.hydrations(),
            0,
            "size-prefetch must not materialize content"
        );

        // A later stat of a warmed sibling is a size_cache hit — no new fault.
        let before = p.size_faults();
        let ino = p.lookup(dir, b"f39.txt").unwrap().unwrap().ino;
        p.getattr(ino).unwrap();
        assert_eq!(
            p.size_faults(),
            before,
            "warmed sibling stat is a cache hit"
        );
    }

    #[test]
    fn single_stat_does_not_arm_prefetch() {
        // A one-off `cat`/`stat` of a single file in a big directory must fault
        // exactly its own size and NOT speculatively pull the whole directory
        // (the readdir-no-hydrate / no-over-fetch guarantee).
        let files = dir_of(40);
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(s, c)| (s.as_str(), c.as_slice()))
            .collect();
        let (_t, _r, p) = projection_of(&refs);
        let p = Arc::new(p);
        let dir = p.lookup(p.root_ino(), b"dir").unwrap().unwrap().ino;

        let ino = p.lookup(dir, b"f00.txt").unwrap().unwrap().ino;
        p.getattr(ino).unwrap();
        p.prefetch_siblings(ino);

        // Give any (incorrect) speculative prefetch ample time to run, then
        // confirm it never armed: only the one stat faulted.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            p.size_faults(),
            1,
            "a single stay-below-threshold stat faults only itself"
        );
        assert_eq!(p.hydrations(), 0, "no content materialized");
    }

    #[test]
    fn journal_record_failure_fails_the_mutation_not_silently_succeeds() {
        // Every mutation handler records BEFORE it mutates, so a journal write
        // failure must FAIL the FUSE op — the change is never applied un-journaled
        // (which would be a false negative for the seeded FSMonitor and a missed
        // `git status`). Covers three reordered handlers: `open_write`, `create`,
        // and `unlink`. Each checks the un-armed success path returns `Ok` and the
        // armed path returns `Err` without leaving the mutation behind.
        let (_t, _r, p) = projection_with_journal_of(&[("f.txt", b"BASE\n"), ("gone.txt", b"x\n")]);
        let root = p.root_ino();
        let arm = || p.journal.as_ref().unwrap().fail_next_record();

        // open_write (records write intent, then copies up / truncates).
        let ino = p.lookup(root, b"f.txt").unwrap().unwrap().ino;
        assert!(p.open_write(ino, false).is_ok(), "normal open_write is ok");
        arm();
        assert!(
            p.open_write(ino, true).is_err(),
            "open_write must fail when the journal record fails, not succeed un-journaled"
        );

        // create (records, then writes the overlay file). The armed create must
        // fail AND leave no file behind.
        let (ok, _f) = p.create(root, b"made-ok.txt", false).unwrap();
        assert!(p.getattr(ok.ino).is_ok(), "normal create is ok");
        arm();
        assert!(
            p.create(root, b"never.txt", false).is_err(),
            "create must fail when the journal record fails, not create un-journaled"
        );
        assert!(
            p.lookup(root, b"never.txt").unwrap().is_none(),
            "a failed create must not leave the file behind"
        );

        // unlink (records, then tombstones / clears). The armed unlink must fail
        // AND leave the file present.
        arm();
        assert!(
            p.unlink(root, b"gone.txt").is_err(),
            "unlink must fail when the journal record fails, not remove un-journaled"
        );
        assert!(
            p.lookup(root, b"gone.txt").unwrap().is_some(),
            "a failed unlink must not remove the file"
        );
        assert!(p.unlink(root, b"gone.txt").is_ok(), "normal unlink is ok");
        assert!(p.lookup(root, b"gone.txt").unwrap().is_none());
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
        // listed exactly once. The malicious case — a repo tree
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
        assert_eq!(main.size, 13); // exact size, not faked
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
        // tree:0 clone fetches no trees; fault the HEAD trees in before projecting.
        repo.build_index().unwrap();
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
        p.rename(p.root_ino(), b"orig.txt", p.root_ino(), b"renamed.txt", 0)
            .unwrap();
        assert!(p.lookup(p.root_ino(), b"orig.txt").unwrap().is_none());
        let r = p.lookup(p.root_ino(), b"renamed.txt").unwrap().unwrap();
        assert_eq!(r.kind, Kind::File { executable: false });
        assert_eq!(p.hydrations(), before, "clean rename fetched no blob");
        // content still readable through the renamed path
        assert_eq!(
            p.open_content(r.ino).unwrap().read_at(0, 100).unwrap(),
            b"hi\n"
        );
    }

    #[test]
    fn ancestor_tombstone_masks_an_untombstoned_child() {
        // A tombstoned ancestor directory masks everything beneath it, even a
        // child that has no tombstone of its own.
        let (_t, _r, p) = projection_of(&[("d/sub/deep.txt", b"deep\n"), ("keep.txt", b"k\n")]);
        let d = p.lookup(p.root_ino(), b"d").unwrap().unwrap();
        let sub = p.lookup(d.ino, b"sub").unwrap().unwrap();
        let _deep = p.lookup(sub.ino, b"deep.txt").unwrap().unwrap();
        // Tombstone the ANCESTOR `d` directly (a non-empty dir tombstone isn't
        // reachable via rmdir, so drive the overlay directly).
        let d_path = RepoPath::from_bytes(b"d".to_vec()).unwrap();
        p.overlay.tombstone(&d_path).unwrap();
        assert!(p.lookup(p.root_ino(), b"d").unwrap().is_none());
        assert!(
            p.lookup(sub.ino, b"deep.txt").unwrap().is_none(),
            "ancestor tombstone did not mask the un-tombstoned child"
        );
        assert!(p.lookup(p.root_ino(), b"keep.txt").unwrap().is_some());
    }

    #[test]
    fn rename_rekeys_overlay_content_and_preserves_inode_identity() {
        use std::io::Write;
        let (_t, _r, p) = projection_of(&[("base.txt", b"base\n")]);
        let (a, mut f) = p.create(p.root_ino(), b"src.txt", false).unwrap();
        f.write_all(b"payload-v1").unwrap();
        drop(f);
        let before = p.lookup(p.root_ino(), b"src.txt").unwrap().unwrap();
        assert_eq!(before.ino, a.ino, "create + lookup agree on the inode");
        let hyd = p.hydrations();
        p.rename(p.root_ino(), b"src.txt", p.root_ino(), b"dst.txt", 0)
            .unwrap();
        assert_eq!(p.hydrations(), hyd, "overlay re-key must fetch nothing");
        assert!(p.lookup(p.root_ino(), b"src.txt").unwrap().is_none());
        let after = p.lookup(p.root_ino(), b"dst.txt").unwrap().unwrap();
        assert_eq!(after.ino, before.ino, "rename did not preserve inode");
        assert_eq!(
            p.open_content(after.ino).unwrap().read_at(0, 100).unwrap(),
            b"payload-v1"
        );
    }

    #[test]
    fn set_executable_on_baseline_file_copies_up() {
        let (_t, _r, p) = projection_of(&[("run.sh", b"#!/bin/sh\necho hi\n")]);
        let a = p.lookup(p.root_ino(), b"run.sh").unwrap().unwrap();
        assert_eq!(a.kind, Kind::File { executable: false });
        let body = p.open_content(a.ino).unwrap().read_at(0, 4096).unwrap();
        p.set_executable(a.ino, true).unwrap();
        let path = RepoPath::from_bytes(b"run.sh".to_vec()).unwrap();
        assert!(matches!(
            p.overlay.lookup(&path),
            Some(OverlayEntry::File {
                executable: true,
                ..
            })
        ));
        let a2 = p.getattr(a.ino).unwrap();
        assert_eq!(a2.kind, Kind::File { executable: true });
        assert_eq!(a2.ino, a.ino, "copy-up preserved the inode");
        assert_eq!(
            p.open_content(a.ino).unwrap().read_at(0, 4096).unwrap(),
            body,
            "copy-up changed the content"
        );
    }

    #[test]
    fn property_overlay_matches_a_reference_model() {
        // model-based test: drive the projection with a deterministic
        // pseudo-random sequence of write/delete/rename ops and assert it always
        // agrees with a simple reference model of the working tree.
        use std::collections::BTreeMap;
        use std::io::Write as _;

        let (_t, _r, p) = projection_of(&[("p0", b"base0\n"), ("p1", b"base1\n")]);
        let root = p.root_ino();
        let paths = ["p0", "p1", "p2", "p3", "p4"];
        let mut model: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
        model.insert("p0", b"base0\n".to_vec());
        model.insert("p1", b"base1\n".to_vec());

        let mut s: u64 = 0x9e3779b97f4a7c15;
        let mut rng = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s
        };

        let read_back = |p: &Projection, ino: u64| -> Vec<u8> {
            p.open_content(ino).unwrap().read_at(0, 1 << 16).unwrap()
        };

        for step in 0..250u64 {
            let r = rng();
            let path = paths[(r % paths.len() as u64) as usize];
            match (r >> 8) % 3 {
                0 => {
                    // write (create or truncate-overwrite)
                    let content = format!("step{step}-{path}").into_bytes();
                    let mut f = match p.lookup(root, path.as_bytes()).unwrap() {
                        Some(a) => p.open_write(a.ino, true).unwrap(),
                        None => p.create(root, path.as_bytes(), false).unwrap().1,
                    };
                    f.write_all(&content).unwrap();
                    drop(f);
                    model.insert(path, content);
                }
                1 => {
                    // delete
                    if p.lookup(root, path.as_bytes()).unwrap().is_some() {
                        p.unlink(root, path.as_bytes()).unwrap();
                    }
                    model.remove(path);
                }
                _ => {
                    // rename to a distinct path
                    let to = paths[((r >> 16) % paths.len() as u64) as usize];
                    if to != path && p.lookup(root, path.as_bytes()).unwrap().is_some() {
                        p.rename(root, path.as_bytes(), root, to.as_bytes(), 0)
                            .unwrap();
                        if let Some(c) = model.remove(path) {
                            model.insert(to, c);
                        }
                    }
                }
            }

            // Invariant: the projection agrees with the model on every path.
            for &q in &paths {
                match (model.get(q), p.lookup(root, q.as_bytes()).unwrap()) {
                    (Some(want), Some(a)) => {
                        assert_eq!(
                            a.kind,
                            Kind::File { executable: false },
                            "{q} kind @ {step}"
                        );
                        assert_eq!(&read_back(&p, a.ino), want, "{q} content @ step {step}");
                    }
                    (None, None) => {}
                    (want, got) => panic!(
                        "step {step}: {q} disagrees — model={:?} projection={:?}",
                        want.map(|w| w.len()),
                        got.map(|a| a.size)
                    ),
                }
            }
        }
    }

    #[test]
    fn pathological_names_roundtrip() {
        //: paths are raw bytes — newlines, tabs, leading dashes, quotes,
        // and invalid UTF-8 must create, resolve, read back, and list correctly.
        use std::io::Write as _;
        let (_t, _r, p) = projection_of(&[("normal.txt", b"n\n")]);
        let root = p.root_ino();
        let names: &[&[u8]] = &[
            b"with\nnewline",
            b"with\ttab",
            b"-leading-dash",
            b"quote\"name",
            b"back\\slash",
            b"\xff\xfe-invalid-utf8",
        ];
        for &name in names {
            let (_a, mut f) = p.create(root, name, false).unwrap();
            f.write_all(name).unwrap(); // content == the raw name bytes
            drop(f);
        }
        for &name in names {
            let a = p
                .lookup(root, name)
                .unwrap()
                .unwrap_or_else(|| panic!("name {name:?} should resolve"));
            assert_eq!(
                p.open_content(a.ino).unwrap().read_at(0, 4096).unwrap(),
                name,
                "content roundtrip for {name:?}"
            );
        }
        let listed: Vec<Vec<u8>> = p
            .readdir(root)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        for &name in names {
            assert!(
                listed.iter().any(|n| n.as_slice() == name),
                "readdir missing {name:?}"
            );
        }
    }

    #[test]
    fn large_directory_readdir_fetches_zero_blobs() {
        // listing a directory with many
        // entries reads only the tree object — zero blobs, O(direct children).
        let files: Vec<(String, Vec<u8>)> = (0..1000)
            .map(|i| {
                (
                    format!("big/f{i:04}.txt"),
                    format!("file {i}\n").into_bytes(),
                )
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        let (_t, _r, p) = projection_of(&refs);
        let big = p.lookup(p.root_ino(), b"big").unwrap().unwrap();
        assert_eq!(big.kind, Kind::Dir);
        let before = p.hydrations();
        let entries = p.readdir(big.ino).unwrap();
        assert_eq!(entries.len(), 1000, "all entries listed");
        assert_eq!(
            p.hydrations(),
            before,
            "readdir of 1000 files fetched a blob"
        );
    }

    #[test]
    fn directory_rename_moves_subtree_without_fetch() {
        //: renaming a directory moves its whole subtree (baseline + overlay,
        // nested) with no blob fetch — metadata-only.
        use std::io::Write as _;
        let (_t, _r, p) = projection_of(&[
            ("dir/a.txt", b"alpha\n"),
            ("dir/sub/b.txt", b"beta\n"),
            ("keep.txt", b"k\n"),
        ]);
        let root = p.root_ino();
        let dir_ino = p.lookup(root, b"dir").unwrap().unwrap().ino;
        {
            let (_a, mut f) = p.create(dir_ino, b"c.txt", false).unwrap();
            f.write_all(b"gamma\n").unwrap();
        }

        let before = p.hydrations();
        p.rename(root, b"dir", root, b"dir2", 0).unwrap();
        assert_eq!(p.hydrations(), before, "directory rename fetched a blob");

        assert!(
            p.lookup(root, b"dir").unwrap().is_none(),
            "source directory should be gone"
        );
        let read = |parent: u64, name: &[u8]| -> Vec<u8> {
            let a = p.lookup(parent, name).unwrap().unwrap();
            p.open_content(a.ino).unwrap().read_at(0, 4096).unwrap()
        };
        let dir2 = p.lookup(root, b"dir2").unwrap().expect("dir2 exists");
        assert_eq!(dir2.kind, Kind::Dir);
        assert_eq!(read(dir2.ino, b"a.txt"), b"alpha\n", "baseline file moved");
        assert_eq!(read(dir2.ino, b"c.txt"), b"gamma\n", "overlay file moved");
        let sub = p
            .lookup(dir2.ino, b"sub")
            .unwrap()
            .expect("nested dir moved");
        assert_eq!(sub.kind, Kind::Dir);
        assert_eq!(
            read(sub.ino, b"b.txt"),
            b"beta\n",
            "nested baseline file moved"
        );
        assert_eq!(read(root, b"keep.txt"), b"k\n", "sibling untouched");
    }
}
