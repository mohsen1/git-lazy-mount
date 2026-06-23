//! `glm-fs-fuse` — the Linux FUSE backend logic (spec §40).
//!
//! [`FuseOps`] maps the low-level FUSE callbacks (`lookup`, `getattr`,
//! `readdir`, `read`, `readlink`, `forget`, …) onto the transactional workspace
//! engine and the stable [`InodeTable`]. It is deliberately free of any libfuse
//! FFI so it builds and is unit-tested on every platform; a thin
//! `fuser::Filesystem` adapter (gated behind a `fuse` feature, requiring
//! libfuse3 and a privileged/loopback runner) calls straight into these methods
//! and is the only remaining piece for real kernel mounting.
//!
//! Read callbacks resolve content through the object provider with a
//! network-permitted but **non-interactive** policy: a read may lazily hydrate,
//! but a filesystem callback never prompts for credentials (spec §3.13) — Git is
//! invoked with `GIT_TERMINAL_PROMPT=0` throughout `glm-git-store`.

#![forbid(unsafe_code)]

use glm_core::{Error, ErrorCode, FetchPolicy, RepoPath, Result};
use glm_fs_common::{FileAttr, InodeTable, ROOT_INO};
use glm_workspace::{DirEntry, EntryKind, Workspace};

/// A resolved directory entry with its assigned inode and attributes.
pub struct ReaddirEntry {
    /// Inode number.
    pub ino: u64,
    /// Entry name (raw bytes).
    pub name: Vec<u8>,
    /// Attributes.
    pub attr: FileAttr,
}

/// The FUSE callback logic over a [`Workspace`] (spec §40).
pub struct FuseOps {
    ws: Workspace,
    inodes: InodeTable,
    policy: FetchPolicy,
}

impl FuseOps {
    /// Wrap a workspace. Reads may hydrate non-interactively.
    pub fn new(ws: Workspace) -> FuseOps {
        FuseOps {
            ws,
            inodes: InodeTable::new(),
            policy: FetchPolicy::AllowNetwork,
        }
    }

    /// Access the inode table (used by the FFI adapter for `forget`).
    pub fn inodes(&self) -> &InodeTable {
        &self.inodes
    }

    fn path_for(&self, ino: u64) -> Result<RepoPath> {
        if ino == ROOT_INO {
            return Ok(RepoPath::root());
        }
        self.inodes
            .path_of(ino)
            .ok_or_else(|| Error::new(ErrorCode::StaleWorkspace, format!("stale inode {ino}")))
    }

    fn attr_for(
        &self,
        ino: u64,
        generation: u64,
        path: &RepoPath,
        kind: EntryKind,
    ) -> Result<FileAttr> {
        let size = match kind {
            EntryKind::File { .. } | EntryKind::Symlink => self.ws.file_size(path, self.policy)?,
            EntryKind::Dir | EntryKind::Gitlink => 0,
        };
        Ok(FileAttr::new(ino, generation, kind, size))
    }

    /// FUSE `lookup`: resolve `name` within `parent_ino`.
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr> {
        let parent = self.path_for(parent_ino)?;
        let child = parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        let kind = self
            .ws
            .lookup(&child, self.policy)?
            .ok_or_else(|| Error::new(ErrorCode::RemoteMissingObject, "no such entry"))?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, kind)
    }

    /// FUSE `getattr`.
    pub fn getattr(&self, ino: u64) -> Result<FileAttr> {
        let path = self.path_for(ino)?;
        let kind = if ino == ROOT_INO {
            EntryKind::Dir
        } else {
            self.ws
                .lookup(&path, self.policy)?
                .ok_or_else(|| Error::new(ErrorCode::RemoteMissingObject, "no such entry"))?
        };
        let generation = 1; // generation is carried per-inode by the FFI adapter
        self.attr_for(ino, generation, &path, kind)
    }

    /// FUSE `readdir`: only this directory's tree is read (spec §18).
    pub fn readdir(&self, ino: u64) -> Result<Vec<ReaddirEntry>> {
        let dir = self.path_for(ino)?;
        let entries: Vec<DirEntry> = self.ws.list_dir(&dir, self.policy)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let child = dir
                .join(&e.name)
                .map_err(|err| Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}")))?;
            let (cino, generation) = self.inodes.lookup(&child);
            let attr = self.attr_for(cino, generation, &child, e.kind)?;
            out.push(ReaddirEntry {
                ino: cino,
                name: e.name,
                attr,
            });
        }
        Ok(out)
    }

    /// FUSE `read`: return `size` bytes from `offset` (may hydrate).
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let path = self.path_for(ino)?;
        let bytes = self.ws.read_file(&path, self.policy)?;
        let start = (offset as usize).min(bytes.len());
        let end = (start + size as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    /// FUSE `readlink`: the symlink target bytes.
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let path = self.path_for(ino)?;
        self.ws.read_file(&path, self.policy)
    }

    /// FUSE `forget`: drop kernel references for an inode.
    pub fn forget(&self, ino: u64, nlookup: u64) {
        self.inodes.forget(ino, nlookup);
    }

    /// The underlying workspace (for write callbacks, which mutate the overlay).
    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    fn child_of(&self, parent_ino: u64, name: &[u8]) -> Result<RepoPath> {
        let parent = self.path_for(parent_ino)?;
        parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))
    }

    // ---- write-side callbacks: all route through `Workspace` (spec §21) ----

    /// FUSE `create` (regular file): create/replace an empty file, return attrs.
    pub fn create(&self, parent_ino: u64, name: &[u8], executable: bool) -> Result<FileAttr> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.write_full(&child, &[], executable)?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, EntryKind::File { executable })
    }

    /// FUSE `symlink`: create/replace a symlink.
    pub fn symlink(&self, parent_ino: u64, name: &[u8], target: &[u8]) -> Result<FileAttr> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.write_symlink(&child, target)?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, EntryKind::Symlink)
    }

    /// FUSE `write`: overwrite `data` at `offset`, preserving untouched bytes.
    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let path = self.path_for(ino)?;
        self.ws.write_at(&path, offset, data, self.policy)?;
        Ok(data.len() as u32)
    }

    /// FUSE `setattr` (size): truncate or extend `ino` to `len`.
    pub fn truncate(&self, ino: u64, len: u64) -> Result<()> {
        let path = self.path_for(ino)?;
        self.ws.truncate(&path, len, self.policy)
    }

    /// FUSE `setattr` (mode): set/clear the executable bit.
    pub fn set_executable(&self, ino: u64, executable: bool) -> Result<()> {
        let path = self.path_for(ino)?;
        self.ws.set_executable(&path, executable, self.policy)
    }

    /// FUSE `unlink`: tombstone `name`; the inode survives open handles (§19).
    pub fn remove(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.delete(&child, self.policy)?;
        self.inodes.unlink(&child);
        Ok(())
    }

    /// FUSE `rename`: rename, preserving inode identity (spec §19, §22).
    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
    ) -> Result<()> {
        let from = self.child_of(parent_ino, name)?;
        let to = self.child_of(new_parent_ino, new_name)?;
        self.ws.rename(&from, &to, self.policy)?;
        self.inodes.rename(&from, &to);
        Ok(())
    }
}

/// Mount a workspace at `mountpoint` via FUSE.
///
/// Without the `fuse` feature, the libfuse-backed adapter is not compiled in;
/// this returns a clear, structured error so callers degrade gracefully rather
/// than appearing to mount.
#[cfg(not(feature = "fuse"))]
pub fn mount(_ops: FuseOps, mountpoint: &std::path::Path) -> Result<()> {
    Err(Error::new(
        ErrorCode::FilesystemBackendUnavailable,
        format!(
            "the libfuse FUSE adapter is not built in; cannot kernel-mount at {}",
            mountpoint.display()
        ),
    )
    .with_action("use the headless CLI (ls/cat/status/...) or build with `--features fuse` on Linux with libfuse3"))
}

/// The libfuse `fuser::Filesystem` adapter (the real kernel mount), compiled only
/// with the `fuse` feature on a host that has libfuse3 (Linux). See
/// `docs/platform-linux.md` and the manual `linux-mount` CI job.
#[cfg(feature = "fuse")]
mod adapter;
#[cfg(feature = "fuse")]
pub use adapter::{mount, spawn_mount, BackgroundMount};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use glm_git_store::{FetchOptions, GitStore};
    use glm_object_provider::{GitObjectProvider, ObjectProvider};
    use glm_workspace::WorkspaceConfig;

    fn ops_with(
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, glm_testkit::SeededRemote, FuseOps) {
        let remote = glm_testkit::seed_remote(files);
        let tmp = tempfile::tempdir().unwrap();
        let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
        store.set_config("protocol.file.allow", "always").unwrap();
        // Deterministic line endings regardless of the host's Git config
        // (Git for Windows ships core.autocrlf=true in system config).
        store.set_config("core.autocrlf", "false").unwrap();
        store.add_remote("origin", &remote.url).unwrap();
        store
            .fetch(
                "origin",
                &[],
                &FetchOptions {
                    filter: Some("blob:none".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let base = store
            .resolve_ref("refs/remotes/origin/main")
            .unwrap()
            .unwrap();
        let provider: Arc<dyn ObjectProvider> =
            Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
        let cfg = WorkspaceConfig {
            workspace_head_ref: "refs/lazy-mount/workspaces/t/head".into(),
            attached_branch: None,
            remote: Some("origin".into()),
            identity: None,
        };
        let ws = Workspace::open_or_create(store, provider, tmp.path(), cfg, Some(base)).unwrap();
        (tmp, remote, FuseOps::new(ws))
    }

    #[test]
    fn lookup_readdir_and_read() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hello\n"), ("src/lib.rs", b"x\n")]);

        // readdir root lists entries from Git trees.
        let root = ops.readdir(ROOT_INO).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&b"a.txt".to_vec()));
        assert!(names.contains(&b"src".to_vec()));

        // lookup + getattr report the exact size.
        let attr = ops.lookup(ROOT_INO, b"a.txt").unwrap();
        assert_eq!(attr.size, 6);
        assert!(matches!(attr.kind, EntryKind::File { .. }));
        let again = ops.getattr(attr.ino).unwrap();
        assert_eq!(again.ino, attr.ino);

        // read returns the (lazily hydrated) content; ranged reads work.
        let bytes = ops.read(attr.ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"hello\n");
        assert_eq!(ops.read(attr.ino, 2, 2).unwrap(), b"ll");
    }

    #[cfg(not(feature = "fuse"))]
    #[test]
    fn mount_is_unavailable_without_adapter() {
        let (_tmp, _remote, ops) = ops_with(&[("a", b"b")]);
        let err = mount(ops, std::path::Path::new("/tmp/x")).unwrap_err();
        assert_eq!(err.code, ErrorCode::FilesystemBackendUnavailable);
    }
}
