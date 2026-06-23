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
use glm_platform::metadata::{self, Disposition, MacMetadataKind};
use glm_platform::validate::AppleVolume;
use glm_workspace::{DirEntry, EntryKind, Workspace};

use crate::collision::{self, Collision, Resolve};

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
    volume: AppleVolume,
}

impl FskitOps {
    /// Wrap a workspace for the default (case-insensitive) APFS volume. Reads may
    /// hydrate non-interactively (spec §3.13): an FSKit callback never prompts.
    pub fn new(ws: Workspace) -> FskitOps {
        FskitOps::with_volume(ws, AppleVolume::CaseInsensitive)
    }

    /// Wrap a workspace, declaring the mounted volume's case behavior. The
    /// on-device adapter detects this from the APFS volume; the default APFS
    /// volume is case-insensitive (issue #7).
    pub fn with_volume(ws: Workspace, volume: AppleVolume) -> FskitOps {
        FskitOps {
            ws,
            inodes: InodeTable::new(),
            policy: FetchPolicy::AllowNetwork,
            volume,
        }
    }

    /// The mounted volume's case behavior.
    pub fn volume(&self) -> AppleVolume {
        self.volume
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

    /// The exact recorded sibling names directly under `dir`.
    fn sibling_names(&self, dir: &RepoPath) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .ws
            .list_dir(dir, self.policy)?
            .into_iter()
            .map(|e| e.name)
            .collect())
    }

    /// Build the structured `PlatformPathCollision` error surfaced when distinct
    /// Git entries fold together on this volume (issue #7).
    fn collision_error(&self, name: &[u8], clashes: &[Vec<u8>]) -> Error {
        let volume = match self.volume {
            AppleVolume::CaseInsensitive => "case-insensitive",
            AppleVolume::CaseSensitive => "case-sensitive",
        };
        let names = clashes
            .iter()
            .map(|c| format!("{:?}", String::from_utf8_lossy(c)))
            .collect::<Vec<_>>()
            .join(", ");
        Error::new(
            ErrorCode::PlatformPathCollision,
            format!(
                "{:?} collides with {names} on this {volume} APFS volume; the entries are \
                 distinct in Git but fold to the same name here",
                String::from_utf8_lossy(name)
            ),
        )
        .with_action(
            "rename one of the colliding entries so the names differ under the volume's folding",
        )
    }

    /// Reject introducing `name` into `parent` when it would collide with an
    /// existing sibling under the volume's folding (issue #7). `exclude` is a
    /// sibling to ignore — the source of a rename, about to disappear.
    fn check_new_name(&self, parent: &RepoPath, name: &[u8], exclude: Option<&[u8]>) -> Result<()> {
        let siblings: Vec<Vec<u8>> = self
            .sibling_names(parent)?
            .into_iter()
            .filter(|s| Some(s.as_slice()) != exclude)
            .collect();
        let clashes = collision::colliding_with(name, &siblings, self.volume);
        if clashes.is_empty() {
            Ok(())
        } else {
            Err(self.collision_error(name, &clashes))
        }
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
    ///
    /// macOS volumes are normalization- (and usually case-) insensitive, so the
    /// kernel may present a name whose bytes differ from the recorded entry
    /// (NFC vs NFD, `readme` vs `README`). We fold-match against the directory's
    /// siblings and resolve to the **single** matching entry's exact recorded
    /// bytes (preserving identity). When two distinct Git entries fold together,
    /// we surface a `PlatformPathCollision` instead of silently picking one
    /// (issue #7).
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<FileAttr> {
        let parent = self.path_for(parent_ino)?;
        let siblings = self.sibling_names(&parent)?;
        let actual = match collision::resolve(name, &siblings, self.volume) {
            Resolve::NotFound => {
                return Err(Error::new(ErrorCode::RemoteMissingObject, "no such entry"))
            }
            Resolve::Collision(names) => return Err(self.collision_error(name, &names)),
            Resolve::Unique(bytes) => bytes,
        };
        let child = parent
            .join(&actual)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        let kind = self
            .ws
            .lookup(&child, self.policy)?
            .ok_or_else(|| Error::new(ErrorCode::RemoteMissingObject, "no such entry"))?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, kind)
    }

    /// Surface the APFS collisions in directory `ino` (issue #7): the sets of
    /// sibling names that are distinct in Git but fold to the same name on this
    /// volume. `enumerate` still returns every entry's exact bytes; this reports
    /// which of them the volume cannot tell apart, so the adapter (and `doctor`)
    /// never present a silently-merged directory.
    pub fn directory_collisions(&self, ino: u64) -> Result<Vec<Collision>> {
        let dir = self.path_for(ino)?;
        let names = self.sibling_names(&dir)?;
        Ok(collision::detect(&names, self.volume))
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
    /// return its attributes. A new name that would collide with an existing
    /// sibling under the volume's folding is rejected (issue #7).
    pub fn create(&self, parent_ino: u64, name: &[u8], executable: bool) -> Result<FileAttr> {
        let parent = self.path_for(parent_ino)?;
        self.check_new_name(&parent, name, None)?;
        let child = parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        self.ws.write_full(&child, &[], executable)?;
        let (ino, generation) = self.inodes.lookup(&child);
        self.attr_for(ino, generation, &child, EntryKind::File { executable })
    }

    /// FSKit `createItem` (symlink): create/replace a symlink.
    pub fn symlink(&self, parent_ino: u64, name: &[u8], target: &[u8]) -> Result<FileAttr> {
        let parent = self.path_for(parent_ino)?;
        self.check_new_name(&parent, name, None)?;
        let child = parent
            .join(name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
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
    ///
    /// A **case-only rename** (`a.txt` → `A.txt`) targets a name that "already
    /// exists" by a case-insensitive volume's comparison, yet identity is
    /// preserved via the inode table, so it is legal and must not be rejected
    /// (issue #7). Any *other* collision at the destination is surfaced.
    pub fn rename(
        &self,
        parent_ino: u64,
        name: &[u8],
        new_parent_ino: u64,
        new_name: &[u8],
    ) -> Result<()> {
        let from = self.child_of(parent_ino, name)?;
        let to_parent = self.path_for(new_parent_ino)?;
        let same_parent = parent_ino == new_parent_ino;
        let case_only = same_parent && collision::is_case_only_rename(name, new_name, self.volume);
        if !case_only {
            let exclude = if same_parent { Some(name) } else { None };
            self.check_new_name(&to_parent, new_name, exclude)?;
        }
        let to = to_parent
            .join(new_name)
            .map_err(|e| Error::new(ErrorCode::InvalidRepositoryPath, format!("{e}")))?;
        self.ws.rename(&from, &to, self.policy)?;
        self.inodes.rename(&from, &to);
        Ok(())
    }

    // ---- macOS metadata commit policy (issue #8, spec §41) ----

    /// The commit-policy disposition of a path that is macOS-injected metadata
    /// (`.DS_Store`, `._*`), if it is one; `None` for ordinary files. These paths
    /// are `Ignored`: the workspace staging path screens them so they can never
    /// reach a commit, on any commit channel.
    pub fn metadata_disposition(&self, path: &RepoPath) -> Option<(MacMetadataKind, Disposition)> {
        metadata::classify_path(path)
    }

    /// The commit-policy disposition of an extended attribute. Always
    /// `OverlayOnly`: Git has no channel to commit xattrs, so resource forks,
    /// Finder info, and quarantine flags are persisted locally but never appear
    /// in a staged tree or commit.
    pub fn xattr_disposition(&self, name: &str) -> (MacMetadataKind, Disposition) {
        metadata::classify_xattr(name)
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

    fn pp(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
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

    #[test]
    fn apfs_collision_surfaced_not_silently_merged() {
        let (_tmp, _remote, ops) = ops_with(&[("LICENSE", b"x\n")]);
        // Two entries Git treats as distinct that a case-insensitive APFS volume
        // folds to the same name (issue #7). The overlay keys by path hash, so it
        // can hold both even on a case-insensitive host.
        ops.workspace()
            .write_full(&pp("README"), b"upper\n", false)
            .unwrap();
        ops.workspace()
            .write_full(&pp("readme"), b"lower\n", false)
            .unwrap();

        // enumerate returns BOTH, with exact bytes — nothing is silently dropped.
        let names: Vec<_> = ops
            .enumerate(ROOT_INO)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&b"README".to_vec()) && names.contains(&b"readme".to_vec()));

        // The collision is surfaced explicitly.
        let cols = ops.directory_collisions(ROOT_INO).unwrap();
        assert!(cols
            .iter()
            .any(|c| c.names == vec![b"README".to_vec(), b"readme".to_vec()]));

        // Looking up either name is an explicit PlatformPathCollision, not a
        // silent pick of one of them.
        let err = ops.lookup(ROOT_INO, b"README").unwrap_err();
        assert_eq!(err.code, ErrorCode::PlatformPathCollision);
        let err = ops.lookup(ROOT_INO, b"readme").unwrap_err();
        assert_eq!(err.code, ErrorCode::PlatformPathCollision);
    }

    #[test]
    fn case_insensitive_lookup_resolves_to_recorded_bytes() {
        let (_tmp, _remote, ops) = ops_with(&[("ReadMe.md", b"hi\n")]);
        // Look up by a different case: resolves to the single recorded entry,
        // preserving its exact recorded bytes for identity.
        let attr = ops.lookup(ROOT_INO, b"readme.md").unwrap();
        assert_eq!(ops.inodes().path_of(attr.ino), Some(pp("ReadMe.md")));
    }

    #[test]
    fn case_only_rename_preserves_identity_and_content() {
        let (_tmp, _remote, ops) = ops_with(&[("a.txt", b"hi\n")]);
        let attr = ops.lookup(ROOT_INO, b"a.txt").unwrap();
        // a.txt -> A.txt on a case-insensitive volume: legal, identity preserved.
        ops.rename(ROOT_INO, b"a.txt", ROOT_INO, b"A.txt").unwrap();
        assert_eq!(ops.inodes().path_of(attr.ino), Some(pp("A.txt")));
        assert_eq!(
            ops.workspace()
                .read_file(&pp("A.txt"), FetchPolicy::AllowNetwork)
                .unwrap(),
            b"hi\n"
        );
    }

    #[test]
    fn enumerate_preserves_exact_recorded_bytes() {
        let (_tmp, _remote, ops) = ops_with(&[("base.txt", b"x\n")]);
        let nfd = "cafe\u{301}.txt".as_bytes().to_vec(); // e + combining acute
        ops.workspace()
            .write_full(&RepoPath::from_bytes(nfd.clone()).unwrap(), b"y\n", false)
            .unwrap();
        let names: Vec<_> = ops
            .enumerate(ROOT_INO)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        // The exact NFD bytes are returned, never NFC-normalized.
        assert!(
            names.contains(&nfd),
            "enumerate must return exact recorded bytes, got {names:?}"
        );
    }

    #[test]
    fn metadata_policy_and_new_name_collision() {
        let (_tmp, _remote, ops) = ops_with(&[("README", b"x\n")]);
        // macOS metadata commit policy (issue #8).
        assert_eq!(
            ops.metadata_disposition(&pp(".DS_Store")).map(|(_, d)| d),
            Some(Disposition::Ignored)
        );
        assert!(ops.metadata_disposition(&pp("real.txt")).is_none());
        assert_eq!(
            ops.xattr_disposition("com.apple.quarantine").1,
            Disposition::OverlayOnly
        );

        // Creating a name that folds onto an existing sibling is rejected (#7).
        let err = ops.create(ROOT_INO, b"readme", false).unwrap_err();
        assert_eq!(err.code, ErrorCode::PlatformPathCollision);
    }
}
