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

use std::sync::Mutex;

use glm_core::{Error, ErrorCode, GitMode, ObjectId, RepoPath, Result};
use glm_fs_common::{InodeTable, ROOT_INO};
use glm_git_repo::AdminRepo;

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

impl Projection {
    /// Open a read-only projection over `repo`, baselined at its HEAD tree.
    pub fn open(repo: AdminRepo) -> Result<Projection> {
        let baseline_tree = repo.head_tree()?.ok_or_else(|| {
            Error::new(
                ErrorCode::Internal,
                "cannot project an unborn HEAD (no baseline tree)",
            )
        })?;
        let gitfile = repo.synthetic_gitfile();
        Ok(Projection {
            repo,
            inodes: InodeTable::new(),
            baseline_tree,
            gitfile,
            metadata_fetch: true,
            lock: Mutex::new(()),
        })
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
        let proj = Projection::open(repo).unwrap();
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
}
