//! Virtual working-tree projection (redesign.md §8, §14–§16).
//!
//! The read-only baseline slice of the model: the projected working tree is the
//! HEAD commit's tree (the *baseline*), plus a protected synthetic `.git`
//! gitfile at the root. The writable overlay (§8) layers on top in M2.
//!
//! Path resolution order (the read-only prefix of §8):
//! 1. synthetic `.git` (root) — shadows any tree entry, fails-safe (§6)
//! 2. baseline Git tree entry
//! 3. absent
//!
//! Invariants enforced here (each covered by a test):
//! * `readdir` returns names + kind only — it **never** reads blob contents or
//!   resolves exact sizes (redesign.md §4.5, §38.2).
//! * a repo `.git` tree entry never shadows the synthetic one (§6).
//! * resolution + listing cost is O(direct children), independent of repo size.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

/// What a path resolves to in the baseline projection.
enum Resolved {
    /// A directory backed by a tree object.
    Dir(ObjectId),
    /// A regular file blob.
    File { oid: ObjectId, executable: bool },
    /// A symlink blob (its content is the target).
    Symlink(ObjectId),
    /// The synthetic `.git` gitfile.
    Gitfile,
}

/// A read-only projection of one workspace's working tree.
pub struct Projection {
    repo: AdminRepo,
    inodes: InodeTable,
    baseline_tree: ObjectId,
    gitfile: Vec<u8>,
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
    // The object reader is the AdminRepo's GitStore; a Mutex guards the
    // single-process cat-file reuse across concurrent FUSE callbacks (§18; a
    // bounded pool is the later refinement).
    lock: Mutex<()>,
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
    /// Open a read-only projection over `repo`, baselined at its HEAD tree.
    /// `cache_dir` holds materialized blob content (created if absent).
    pub fn open(repo: AdminRepo, cache_dir: PathBuf) -> Result<Projection> {
        let baseline_tree = repo.head_tree()?.ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "cannot project an unborn HEAD (no baseline tree)",
            )
        })?;
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("create cache dir: {e}")))?;
        let gitfile = repo.synthetic_gitfile();
        Ok(Projection {
            repo,
            inodes: InodeTable::new(),
            baseline_tree,
            gitfile,
            cache_dir,
            hydrations: AtomicU64::new(0),
            metadata_fetch: true,
            lock: Mutex::new(()),
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

    /// Resolve a repo-relative path in the baseline projection. Reads only trees
    /// (present under blob:none) — never blob contents (§4.5).
    fn resolve(&self, path: &RepoPath) -> Result<Option<Resolved>> {
        if path.is_root() {
            return Ok(Some(Resolved::Dir(self.baseline_tree.clone())));
        }
        let comps: Vec<&[u8]> = path.components().collect();
        // The synthetic root `.git` shadows any tree entry of the same name (§6).
        if comps.len() == 1 && comps[0] == GITFILE_NAME {
            return Ok(Some(Resolved::Gitfile));
        }
        let mut tree = self.baseline_tree.clone();
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let obj = self.repo.store().read_tree(&tree, false)?;
            let Some(entry) = obj.entries.iter().find(|e| e.name == *comp) else {
                return Ok(None);
            };
            match entry.mode {
                GitMode::Tree => {
                    if last {
                        return Ok(Some(Resolved::Dir(entry.object_id.clone())));
                    }
                    tree = entry.object_id.clone();
                }
                GitMode::Gitlink => {
                    // Submodule: projects as a directory; not descended into here.
                    if last {
                        return Ok(Some(Resolved::Dir(entry.object_id.clone())));
                    }
                    return Ok(None);
                }
                GitMode::Regular | GitMode::Executable => {
                    if last {
                        return Ok(Some(Resolved::File {
                            oid: entry.object_id.clone(),
                            executable: entry.mode == GitMode::Executable,
                        }));
                    }
                    return Ok(None); // a path component under a file
                }
                GitMode::Symlink => {
                    if last {
                        return Ok(Some(Resolved::Symlink(entry.object_id.clone())));
                    }
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    fn attr_of(&self, ino: u64, generation: u64, r: &Resolved) -> Result<Attr> {
        let (kind, size) = match r {
            Resolved::Dir(_) => (Kind::Dir, 0),
            Resolved::Gitfile => (Kind::File { executable: false }, self.gitfile.len() as u64),
            Resolved::File { oid, executable } => (
                Kind::File {
                    executable: *executable,
                },
                self.repo.store().object_size(oid, self.metadata_fetch)?,
            ),
            Resolved::Symlink(oid) => (
                Kind::Symlink,
                self.repo.store().object_size(oid, self.metadata_fetch)?,
            ),
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
        let _g = self.lock.lock().unwrap();
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
        let _g = self.lock.lock().unwrap();
        let path = self.path_of(ino)?;
        let r = self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?;
        let (i, generation) = self.inodes.lookup(&path);
        debug_assert_eq!(i, ino);
        self.attr_of(ino, generation, &r)
    }

    /// `readdir(ino)` — names + kind + inode only; reads **no** blob contents and
    /// resolves **no** sizes (§4.5, §38.2). Cost is O(direct children).
    pub fn readdir(&self, ino: u64) -> Result<Vec<DirEntry>> {
        let _g = self.lock.lock().unwrap();
        let path = self.path_of(ino)?;
        let Some(Resolved::Dir(tree)) = self.resolve(&path)? else {
            return Err(Error::new(ErrorCode::Internal, "not a directory"));
        };
        let obj = self.repo.store().read_tree(&tree, false)?;
        let mut out = Vec::with_capacity(obj.entries.len() + 1);
        let at_root = path.is_root();
        for e in &obj.entries {
            // A repo `.git` tree entry at the root is shadowed by the synthetic
            // one (§6) and never listed twice.
            if at_root && e.name == GITFILE_NAME {
                continue;
            }
            let kind = match e.mode {
                GitMode::Tree | GitMode::Gitlink => Kind::Dir,
                GitMode::Regular => Kind::File { executable: false },
                GitMode::Executable => Kind::File { executable: true },
                GitMode::Symlink => Kind::Symlink,
            };
            let child = path
                .join(&e.name)
                .map_err(|err| Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}")))?;
            let (cino, _gen) = self.inodes.lookup(&child);
            out.push(DirEntry {
                name: e.name.clone(),
                kind,
                ino: cino,
            });
        }
        if at_root {
            let (gino, _gen) = self
                .inodes
                .lookup(&RepoPath::from_bytes(GITFILE_NAME).unwrap());
            out.push(DirEntry {
                name: GITFILE_NAME.to_vec(),
                kind: Kind::File { executable: false },
                ino: gino,
            });
        }
        Ok(out)
    }

    /// Open content for reading (§17.1). A clean tracked blob is materialized
    /// once into a content-addressed cache file (atomic publish) and then served
    /// by `pread` from its FD; the synthetic `.git` is served from memory.
    /// Faults the blob in on first access (a later refinement routes this through
    /// the bounded fetch scheduler so "only the scheduler causes network", §20).
    pub fn open_content(&self, ino: u64) -> Result<ContentHandle> {
        let _g = self.lock.lock().unwrap();
        let path = self.path_of(ino)?;
        match self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?
        {
            Resolved::Gitfile => Ok(ContentHandle {
                inner: ContentInner::Bytes(self.gitfile.clone()),
            }),
            Resolved::File { oid, .. } => {
                let file = self.materialize(&oid)?;
                Ok(ContentHandle {
                    inner: ContentInner::File(file),
                })
            }
            Resolved::Symlink(_) => Err(Error::new(
                ErrorCode::Internal,
                "open_content on a symlink; use readlink",
            )),
            Resolved::Dir(_) => Err(Error::new(ErrorCode::Internal, "is a directory")),
        }
    }

    /// Materialize a blob's working-tree bytes into the content-addressed cache
    /// and return an open read FD. Streams via `cat-file` (no in-process buffer)
    /// and publishes atomically (temp + rename) so a partial file is never used.
    fn materialize(&self, oid: &ObjectId) -> Result<std::fs::File> {
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
        std::fs::File::open(&final_path)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("open cache file: {e}")))
    }

    /// `readlink(ino)` — the symlink's raw target bytes (§30.1). Targets are
    /// small, so the blob is read whole here (this is not a content stream).
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let _g = self.lock.lock().unwrap();
        let path = self.path_of(ino)?;
        match self
            .resolve(&path)?
            .ok_or_else(|| Error::new(ErrorCode::Internal, format!("inode {ino} vanished")))?
        {
            Resolved::Symlink(oid) => self.repo.store().read_blob_raw(&oid, self.metadata_fetch),
            _ => Err(Error::new(ErrorCode::Internal, "not a symlink")),
        }
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
        let cache = tmp.path().join("cache");
        let proj = Projection::open(repo, cache).unwrap();
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
        let p = Projection::open(repo, tmp.path().join("cache")).unwrap();
        let a = p.lookup(p.root_ino(), b"link").unwrap().unwrap();
        assert_eq!(a.kind, Kind::Symlink);
        assert_eq!(p.readlink(a.ino).unwrap(), b"target/path");
    }
}
