//! `FskitOps` — the FSKit `FSVolume` callback logic over the shared workspace
//! engine (issue #5, spec §41).
//!
//! This is the macOS analog of [`glm_fs_fuse::FuseOps`]: it maps the operations
//! an `FSUnaryFileSystem` / `FSVolume` extension must serve
//! (`lookup`/`getattr`/enumerate/`read`/`readlink`/`forget` and the write
//! callbacks) onto the same [`Workspace`] and the stable [`InodeTable`]. It is
//! deliberately free of any FSKit FFI so it builds and is unit-tested on every
//! platform; a thin Swift `FSVolume` adapter (delivered on-device — see issue
//! #10 and the validation harness, issue #12) calls straight into these methods
//! and is the only remaining macOS-specific piece for a real kernel mount.
//!
//! Crucially, **every write routes through [`Workspace`]** exactly as the FUSE
//! backend's writes do — the copy-on-write overlay, then `add` → stage → commit
//! → operation log. There are no macOS-only write semantics (spec §41).

use glm_core::{Error, ErrorCode, FetchPolicy, RepoPath, Result};
use glm_fs_common::{FileAttr, InodeTable, ROOT_INO};
use glm_workspace::{DirEntry, EntryKind, Workspace};

/// A resolved directory entry with its assigned inode and attributes.
pub struct EnumerateEntry {
    /// Inode number.
    pub ino: u64,
    /// Entry name (raw bytes — the exact bytes Git recorded; spec §41).
    pub name: Vec<u8>,
    /// Attributes.
    pub attr: FileAttr,
}

/// The FSKit `FSVolume` callback logic over a [`Workspace`] (spec §41).
pub struct FskitOps {
    ws: Workspace,
    inodes: InodeTable,
    policy: FetchPolicy,
}

impl FskitOps {
    /// Wrap a workspace. Reads may hydrate non-interactively (spec §3.13): an
    /// FSKit callback never prompts for credentials.
    pub fn new(ws: Workspace) -> FskitOps {
        FskitOps {
            ws,
            inodes: InodeTable::new(),
            policy: FetchPolicy::AllowNetwork,
        }
    }

    /// Access the inode table (used by the FFI adapter for `forget`/identity).
    pub fn inodes(&self) -> &InodeTable {
        &self.inodes
    }

    /// The underlying workspace (shared transactional engine).
    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    fn path_for(&self, ino: u64) -> Result<RepoPath> {
        if ino == ROOT_INO {
            return Ok(RepoPath::root());
        }
        self.inodes
            .path_of(ino)
            .ok_or_else(|| Error::new(ErrorCode::StaleWorkspace, format!("stale inode {ino}")))
    }

    fn child_of(&self, parent_ino: u64, name: &[u8]) -> Result<RepoPath> {
        let parent = self.path_for(parent_ino)?;
        parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))
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

    // ---- read-side callbacks (mirror `FuseOps`) ----

    /// FSKit `lookupName`: resolve `name` within `parent_ino`.
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr> {
        let child = self.child_of(parent_ino, name)?;
        let kind = self
            .ws
            .lookup(&child, self.policy)?
            .ok_or_else(|| Error::new(ErrorCode::RemoteMissingObject, "no such entry"))?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, kind)
    }

    /// FSKit `getAttributes`.
    pub fn getattr(&self, ino: u64) -> Result<FileAttr> {
        let path = self.path_for(ino)?;
        let kind = if ino == ROOT_INO {
            EntryKind::Dir
        } else {
            self.ws
                .lookup(&path, self.policy)?
                .ok_or_else(|| Error::new(ErrorCode::RemoteMissingObject, "no such entry"))?
        };
        self.attr_for(ino, 1, &path, kind)
    }

    /// FSKit `enumerateDirectory`: only this directory's tree is read (spec §18).
    pub fn enumerate(&self, ino: u64) -> Result<Vec<EnumerateEntry>> {
        let dir = self.path_for(ino)?;
        let entries: Vec<DirEntry> = self.ws.list_dir(&dir, self.policy)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let child = dir
                .join(&e.name)
                .map_err(|err| Error::new(ErrorCode::InvalidRepositoryPath, format!("{err}")))?;
            let (cino, generation) = self.inodes.lookup(&child);
            let attr = self.attr_for(cino, generation, &child, e.kind)?;
            out.push(EnumerateEntry {
                ino: cino,
                name: e.name,
                attr,
            });
        }
        Ok(out)
    }

    /// FSKit `read`: return `size` bytes from `offset` (may hydrate).
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let path = self.path_for(ino)?;
        let bytes = self.ws.read_file(&path, self.policy)?;
        let start = (offset as usize).min(bytes.len());
        let end = (start + size as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    /// FSKit `readSymbolicLink`: the symlink target bytes.
    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let path = self.path_for(ino)?;
        self.ws.read_file(&path, self.policy)
    }

    /// FSKit `reclaim`: drop kernel references for an inode.
    pub fn forget(&self, ino: u64, nlookup: u64) {
        self.inodes.forget(ino, nlookup);
    }

    // ---- write-side callbacks: all route through `Workspace` (spec §41) ----

    /// FSKit `createItem` (regular file): create/replace an empty file and
    /// return its attributes.
    pub fn create(&self, parent_ino: u64, name: &[u8], executable: bool) -> Result<FileAttr> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.write_full(&child, &[], executable)?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, EntryKind::File { executable })
    }

    /// FSKit `createItem` (symlink): create/replace a symlink.
    pub fn symlink(&self, parent_ino: u64, name: &[u8], target: &[u8]) -> Result<FileAttr> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.write_symlink(&child, target)?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, EntryKind::Symlink)
    }

    /// FSKit `write`: overwrite `data` at `offset`, preserving untouched bytes.
    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let path = self.path_for(ino)?;
        self.ws.write_at(&path, offset, data, self.policy)?;
        Ok(data.len() as u32)
    }

    /// FSKit `setAttributes` (size): truncate or extend `ino` to `len`.
    pub fn truncate(&self, ino: u64, len: u64) -> Result<()> {
        let path = self.path_for(ino)?;
        self.ws.truncate(&path, len, self.policy)
    }

    /// FSKit `setAttributes` (mode): set/clear the executable bit.
    pub fn set_executable(&self, ino: u64, executable: bool) -> Result<()> {
        let path = self.path_for(ino)?;
        self.ws.set_executable(&path, executable, self.policy)
    }

    /// FSKit `removeItem`: unlink `name` from `parent_ino` (tombstone in the
    /// overlay; open handles survive until `forget`, spec §19).
    pub fn remove(&self, parent_ino: u64, name: &[u8]) -> Result<()> {
        let child = self.child_of(parent_ino, name)?;
        self.ws.delete(&child, self.policy)?;
        self.inodes.unlink(&child);
        Ok(())
    }

    /// FSKit `renameItem`: rename, preserving inode identity (spec §19, §22).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use glm_git_store::{FetchOptions, GitStore};
    use glm_object_provider::{GitObjectProvider, ObjectProvider};
    use glm_workspace::{Workspace, WorkspaceConfig};

    fn ops_with(
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, glm_testkit::SeededRemote, FskitOps) {
        let remote = glm_testkit::seed_remote(files);
        let tmp = tempfile::tempdir().unwrap();
        let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
        store.set_config("protocol.file.allow", "always").unwrap();
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
        (tmp, remote, FskitOps::new(ws))
    }

    #[test]
    fn lookup_enumerate_and_read() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hello\n"), ("src/lib.rs", b"x\n")]);

        let root = ops.enumerate(ROOT_INO).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&b"a.txt".to_vec()));
        assert!(names.contains(&b"src".to_vec()));

        let attr = ops.lookup(ROOT_INO, b"a.txt").unwrap();
        assert_eq!(attr.size, 6);
        assert!(matches!(attr.kind, EntryKind::File { .. }));
        let again = ops.getattr(attr.ino).unwrap();
        assert_eq!(again.ino, attr.ino);

        let bytes = ops.read(attr.ino, 0, 1024).unwrap();
        assert_eq!(bytes, b"hello\n");
        assert_eq!(ops.read(attr.ino, 2, 2).unwrap(), b"ll");
    }

    #[test]
    fn writes_route_through_the_workspace_overlay() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hello\n")]);

        // Create a new file through the FSKit write callback...
        let attr = ops.create(ROOT_INO, b"new.txt", false).unwrap();
        ops.write(attr.ino, 0, b"world\n").unwrap();

        // ...and observe it through the same engine the headless CLI uses.
        let p = glm_core::RepoPath::from_bytes(b"new.txt".to_vec()).unwrap();
        assert_eq!(
            ops.workspace()
                .read_file(&p, FetchPolicy::AllowNetwork)
                .unwrap(),
            b"world\n"
        );

        // It shows up in status as a working-tree change (overlay), proving the
        // write went through the shared staging path, not a macOS side channel.
        let status = ops.workspace().status(FetchPolicy::AllowNetwork).unwrap();
        assert!(status.iter().any(|e| e.path == p));
    }

    #[test]
    fn rename_preserves_inode_identity() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hi\n")]);
        let attr = ops.lookup(ROOT_INO, b"a.txt").unwrap();
        ops.rename(ROOT_INO, b"a.txt", ROOT_INO, b"b.txt").unwrap();
        // The same inode now answers to the new name (open handles stay valid).
        assert_eq!(
            ops.inodes().path_of(attr.ino),
            Some(glm_core::RepoPath::from_bytes(b"b.txt".to_vec()).unwrap())
        );
    }

    #[test]
    fn remove_tombstones_but_keeps_open_inode() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hi\n")]);
        let attr = ops.lookup(ROOT_INO, b"a.txt").unwrap(); // one open reference
        ops.remove(ROOT_INO, b"a.txt").unwrap();
        // Gone from the namespace...
        assert!(ops.lookup(ROOT_INO, b"a.txt").is_err());
        // ...but the inode survives for the open handle until forget.
        assert!(ops.inodes().is_live(attr.ino));
    }
}
