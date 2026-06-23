//! `glm-workspace` — the transactional working-copy engine (spec §11–§14, §24,
//! §35–§36).
//!
//! A workspace resolves the **working tree** from, in order (spec §11):
//! conflict → overlay entry → tombstone → rename mapping (base-ref) → base
//! committed tree → missing. It computes a three-tree status (`X` = staged vs
//! HEAD, `Y` = working vs staged) in `O(staged + overlay)` — never scanning the
//! full tree (spec §49). Commits are ordinary Git commits built from the staged
//! delta with unchanged subtrees reused, sealed through the operation log and
//! published to refs with compare-and-swap (spec §14, §24).

#![forbid(unsafe_code)]

mod status;
mod tree_build;

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use glm_core::{
    Error, ErrorCode, FetchPolicy, GitMode, ObjectId, OperationId, RepoPath, Result, TreeEntry,
};
use glm_git_store::{CommitParams, GitStore, Identity};
use glm_object_provider::ObjectProvider;
use glm_oplog::{Cause, ExternalSideEffect, NewOperation, OpLog, WorkspaceView};
use glm_overlay::{Overlay, OverlayKind};
use glm_stage::{Stage, StagedChange};

pub use status::{StatusCode, StatusEntry};
pub use tree_build::{build_tree, TreeChange};

/// The kind of a resolved directory/file entry, for projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file; `executable` is the Git exec bit.
    File {
        /// Whether the Git executable bit is set.
        executable: bool,
    },
    /// Symbolic link.
    Symlink,
    /// Directory (tree).
    Dir,
    /// Submodule gitlink.
    Gitlink,
}

impl EntryKind {
    fn of_mode(m: GitMode) -> EntryKind {
        match m {
            GitMode::Regular => EntryKind::File { executable: false },
            GitMode::Executable => EntryKind::File { executable: true },
            GitMode::Symlink => EntryKind::Symlink,
            GitMode::Tree => EntryKind::Dir,
            GitMode::Gitlink => EntryKind::Gitlink,
        }
    }
}

/// A single directory listing entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The component name (raw bytes).
    pub name: Vec<u8>,
    /// What kind of entry it is.
    pub kind: EntryKind,
}

/// Static configuration for a workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    /// The private workspace head ref (spec §14), e.g.
    /// `refs/lazy-mount/workspaces/<id>/head`.
    pub workspace_head_ref: String,
    /// The attached public branch, if any (e.g. `refs/heads/main`).
    pub attached_branch: Option<String>,
    /// The remote to push to (e.g. `origin`).
    pub remote: Option<String>,
    /// Author/committer identity, or `None` to use Git config.
    pub identity: Option<Identity>,
}

/// The transactional working-copy engine.
pub struct Workspace {
    store: GitStore,
    provider: Arc<dyn ObjectProvider>,
    overlay: Overlay,
    stage: Stage,
    oplog: OpLog,
    cfg: WorkspaceConfig,
    base: Mutex<Option<ObjectId>>,
    attached_expected: Mutex<Option<ObjectId>>,
    generation: Mutex<u64>,
}

type EntryRef = (ObjectId, GitMode);

impl Workspace {
    /// Open an existing workspace or create one at `base_commit`.
    pub fn open_or_create(
        store: GitStore,
        provider: Arc<dyn ObjectProvider>,
        ws_dir: &Path,
        cfg: WorkspaceConfig,
        base_commit: Option<ObjectId>,
    ) -> Result<Workspace> {
        let overlay = Overlay::open(ws_dir.join("overlay"))?;
        let stage = Stage::open(ws_dir.join("stage"))?;
        let oplog = OpLog::open(ws_dir.join("journal"))?;

        let (base, attached_expected, generation) = match oplog.current_view()? {
            Some(view) => (
                view.base_commit.clone(),
                view.attached_branch_expected.clone(),
                view.mount_generation,
            ),
            None => {
                // Fresh workspace: protect the base with the private head ref and
                // seal the root view.
                if let Some(base) = &base_commit {
                    // Create the keep-ref (ignore "already exists" by reading first).
                    if store.resolve_ref(&cfg.workspace_head_ref)?.is_none() {
                        store.update_ref_cas(&cfg.workspace_head_ref, base, None)?;
                    }
                }
                let attached_expected = match &cfg.attached_branch {
                    Some(b) => store.resolve_ref(b)?,
                    None => None,
                };
                let mut view =
                    WorkspaceView::root(glm_core::WorkspaceViewId(vec![]), base_commit.clone());
                view.attached_branch = cfg.attached_branch.clone();
                view.attached_branch_expected = attached_expected.clone();
                oplog.commit(
                    view,
                    NewOperation {
                        cause: Cause::Command("mount".into()),
                        description: "create workspace".into(),
                        durability: glm_core::Durability::OperationSealed,
                        external_effects: vec![],
                    },
                )?;
                oplog.mark_applied(0)?;
                (base_commit, attached_expected, 0)
            }
        };

        Ok(Workspace {
            store,
            provider,
            overlay,
            stage,
            oplog,
            cfg,
            base: Mutex::new(base),
            attached_expected: Mutex::new(attached_expected),
            generation: Mutex::new(generation),
        })
    }

    /// The current base commit.
    pub fn base_commit(&self) -> Option<ObjectId> {
        self.base.lock().unwrap().clone()
    }

    /// The operation log (for `op log`/inspection).
    pub fn oplog(&self) -> &OpLog {
        &self.oplog
    }

    /// The object provider (for metrics/inspection).
    pub fn provider(&self) -> &Arc<dyn ObjectProvider> {
        &self.provider
    }

    fn base_tree(&self, _policy: FetchPolicy) -> Result<Option<ObjectId>> {
        match self.base_commit() {
            Some(commit) => self
                .store
                .rev_parse(&format!("{}^{{tree}}", commit.to_hex())),
            None => Ok(None),
        }
    }

    /// Resolve a base committed-tree entry by walking trees component by
    /// component. Returns `None` if absent in the base tree.
    pub fn resolve_base_entry(
        &self,
        path: &RepoPath,
        policy: FetchPolicy,
    ) -> Result<Option<TreeEntry>> {
        if path.is_root() {
            return Ok(None);
        }
        let mut current = match self.base_tree(policy)? {
            Some(t) => t,
            None => return Ok(None),
        };
        let comps: Vec<&[u8]> = path.components().collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = self.provider.tree(&current, policy)?;
            let entry = match tree.entries.into_iter().find(|e| e.name == *comp) {
                Some(e) => e,
                None => return Ok(None),
            };
            if i + 1 == comps.len() {
                return Ok(Some(entry));
            }
            if !matches!(entry.mode, GitMode::Tree) {
                return Ok(None); // a non-directory in the middle of the path
            }
            current = entry.object_id;
        }
        Ok(None)
    }

    fn base_entry_ref(&self, path: &RepoPath, policy: FetchPolicy) -> Result<Option<EntryRef>> {
        Ok(self
            .resolve_base_entry(path, policy)?
            .map(|e| (e.object_id, e.mode)))
    }

    /// Look up the kind of entry at `path` in the working tree (spec §11 order).
    pub fn lookup(&self, path: &RepoPath, policy: FetchPolicy) -> Result<Option<EntryKind>> {
        if path.is_root() {
            return Ok(Some(EntryKind::Dir));
        }
        match self.overlay.entry(path) {
            Some(OverlayKind::Tombstone) => return Ok(None),
            Some(OverlayKind::File { executable }) => {
                return Ok(Some(EntryKind::File { executable }))
            }
            Some(OverlayKind::Symlink) => return Ok(Some(EntryKind::Symlink)),
            Some(OverlayKind::BaseRef { mode, .. }) => return Ok(Some(EntryKind::of_mode(mode))),
            None => {}
        }
        if let Some(entry) = self.resolve_base_entry(path, policy)? {
            return Ok(Some(EntryKind::of_mode(entry.mode)));
        }
        // A directory implied solely by overlay additions beneath it.
        if self.overlay_has_descendant(path) {
            return Ok(Some(EntryKind::Dir));
        }
        Ok(None)
    }

    fn overlay_has_descendant(&self, dir: &RepoPath) -> bool {
        self.overlay
            .entries()
            .into_iter()
            .any(|(p, k)| !matches!(k, OverlayKind::Tombstone) && dir.is_prefix_of(&p) && &p != dir)
    }

    /// List a directory's entries: the base tree at `dir` merged with overlay
    /// additions/tombstones. Reads only this directory's tree (spec §18, §49).
    pub fn list_dir(&self, dir: &RepoPath, policy: FetchPolicy) -> Result<Vec<DirEntry>> {
        use std::collections::BTreeMap;
        let mut names: BTreeMap<Vec<u8>, EntryKind> = BTreeMap::new();

        // Base entries directly in this directory.
        let dir_tree = if dir.is_root() {
            self.base_tree(policy)?
        } else {
            match self.resolve_base_entry(dir, policy)? {
                Some(e) if matches!(e.mode, GitMode::Tree) => Some(e.object_id),
                _ => None,
            }
        };
        if let Some(t) = dir_tree {
            for e in self.provider.tree(&t, policy)?.entries {
                names.insert(e.name, EntryKind::of_mode(e.mode));
            }
        }

        // Overlay overrides for immediate children of `dir`.
        for (p, kind) in self.overlay.entries() {
            let rel = match rel_after(dir, &p) {
                Some(r) if !r.is_empty() => r,
                _ => continue,
            };
            let first = rel[0].clone();
            if rel.len() == 1 {
                match kind {
                    OverlayKind::Tombstone => {
                        names.remove(&first);
                    }
                    OverlayKind::File { executable } => {
                        names.insert(first, EntryKind::File { executable });
                    }
                    OverlayKind::Symlink => {
                        names.insert(first, EntryKind::Symlink);
                    }
                    OverlayKind::BaseRef { mode, .. } => {
                        names.insert(first, EntryKind::of_mode(mode));
                    }
                }
            } else if !matches!(kind, OverlayKind::Tombstone) {
                // A deeper overlay path implies an intermediate directory.
                names.entry(first).or_insert(EntryKind::Dir);
            }
        }

        Ok(names
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect())
    }

    /// Read the working-tree bytes at `path` (overlay or filtered base).
    pub fn read_file(&self, path: &RepoPath, policy: FetchPolicy) -> Result<Vec<u8>> {
        match self.overlay.entry(path) {
            Some(OverlayKind::Tombstone) => Err(not_found(path)),
            Some(OverlayKind::File { .. }) | Some(OverlayKind::Symlink) => {
                Ok(self.overlay.read_content(path)?.unwrap_or_default())
            }
            Some(OverlayKind::BaseRef { oid, mode }) => {
                self.read_blob_for_mode(&oid, path, mode, policy)
            }
            None => match self.resolve_base_entry(path, policy)? {
                Some(e) if e.mode.is_file() => {
                    self.provider.filtered_blob(&e.object_id, path, policy)
                }
                Some(e) if matches!(e.mode, GitMode::Symlink) => {
                    self.provider.raw_blob(&e.object_id, policy)
                }
                _ => Err(not_found(path)),
            },
        }
    }

    fn read_blob_for_mode(
        &self,
        oid: &ObjectId,
        path: &RepoPath,
        mode: GitMode,
        policy: FetchPolicy,
    ) -> Result<Vec<u8>> {
        if matches!(mode, GitMode::Symlink) {
            self.provider.raw_blob(oid, policy)
        } else {
            self.provider.filtered_blob(oid, path, policy)
        }
    }

    /// Exact working-tree size for `path`. May fetch+filter content when the
    /// size cannot be known otherwise — recorded as metadata-triggered
    /// hydration (spec §5.1). Never returns a fake zero.
    pub fn file_size(&self, path: &RepoPath, policy: FetchPolicy) -> Result<u64> {
        match self.overlay.entry(path) {
            Some(OverlayKind::Tombstone) => Err(not_found(path)),
            Some(OverlayKind::File { .. }) | Some(OverlayKind::Symlink) => {
                Ok(self.overlay.content_len(path)?.unwrap_or(0))
            }
            Some(OverlayKind::BaseRef { oid, mode }) => {
                Ok(self.read_blob_for_mode(&oid, path, mode, policy)?.len() as u64)
            }
            None => match self.resolve_base_entry(path, policy)? {
                Some(e) if e.mode.is_file() => Ok(self
                    .provider
                    .filtered_blob(&e.object_id, path, policy)?
                    .len() as u64),
                Some(e) if matches!(e.mode, GitMode::Symlink) => {
                    Ok(self.provider.raw_blob(&e.object_id, policy)?.len() as u64)
                }
                _ => Err(not_found(path)),
            },
        }
    }

    // ---- write primitives (copy-on-write into the overlay; spec §21) ----

    /// Replace a file's full content (e.g. `O_TRUNC` then write). Does NOT fetch
    /// the old content (spec §21 truncate / §53.7).
    pub fn write_full(&self, path: &RepoPath, bytes: &[u8], executable: bool) -> Result<()> {
        self.overlay.put_file(path, bytes, executable)
    }

    /// Truncate `path` to `new_len`. `new_len == 0` never fetches the old bytes.
    pub fn truncate(&self, path: &RepoPath, new_len: u64, policy: FetchPolicy) -> Result<()> {
        let executable = matches!(
            self.lookup(path, policy)?,
            Some(EntryKind::File { executable: true })
        );
        if new_len == 0 {
            return self.overlay.put_file(path, &[], executable);
        }
        let mut content = if self.lookup(path, policy)?.is_some() {
            self.read_file(path, policy)?
        } else {
            Vec::new()
        };
        content.resize(new_len as usize, 0);
        self.overlay.put_file(path, &content, executable)
    }

    /// Partially overwrite `path` at `offset`, preserving untouched bytes (spec
    /// §21 partial overwrite). Materializes base content if needed.
    pub fn write_at(
        &self,
        path: &RepoPath,
        offset: u64,
        data: &[u8],
        policy: FetchPolicy,
    ) -> Result<()> {
        let executable = matches!(
            self.lookup(path, policy)?,
            Some(EntryKind::File { executable: true })
        );
        let mut content = match self.overlay.read_content(path)? {
            Some(c) => c,
            None if self.lookup(path, policy)?.is_some() => self.read_file(path, policy)?,
            None => Vec::new(),
        };
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(data);
        self.overlay.put_file(path, &content, executable)
    }

    /// Create or replace a symlink with `target` bytes.
    pub fn write_symlink(&self, path: &RepoPath, target: &[u8]) -> Result<()> {
        self.overlay.put_symlink(path, target)
    }

    /// Set the executable bit on a clean base file without fetching content.
    pub fn set_executable(
        &self,
        path: &RepoPath,
        executable: bool,
        policy: FetchPolicy,
    ) -> Result<()> {
        if let Some(OverlayKind::File { .. }) = self.overlay.entry(path) {
            let bytes = self.overlay.read_content(path)?.unwrap_or_default();
            return self.overlay.put_file(path, &bytes, executable);
        }
        if let Some((oid, _)) = self.base_entry_ref(path, policy)? {
            let mode = if executable {
                GitMode::Executable
            } else {
                GitMode::Regular
            };
            return self.overlay.put_base_ref(path, oid, mode);
        }
        Err(not_found(path))
    }

    /// Delete `path` (tombstone if it exists in base, else drop the overlay
    /// entry). Does not touch shared Git objects (spec §21 delete).
    pub fn delete(&self, path: &RepoPath, policy: FetchPolicy) -> Result<()> {
        if self.base_entry_ref(path, policy)?.is_some() {
            self.overlay.tombstone(path)
        } else {
            self.overlay.clear(path)
        }
    }

    /// Rename `from` to `to`. A clean file (or base-ref) rename references the
    /// existing blob — **no content fetch** (spec §22, §53.10).
    pub fn rename(&self, from: &RepoPath, to: &RepoPath, policy: FetchPolicy) -> Result<()> {
        match self.overlay.entry(from) {
            Some(OverlayKind::File { executable }) => {
                let bytes = self.overlay.read_content(from)?.unwrap_or_default();
                self.overlay.put_file(to, &bytes, executable)?;
            }
            Some(OverlayKind::Symlink) => {
                let bytes = self.overlay.read_content(from)?.unwrap_or_default();
                self.overlay.put_symlink(to, &bytes)?;
            }
            Some(OverlayKind::BaseRef { oid, mode }) => {
                self.overlay.put_base_ref(to, oid, mode)?;
            }
            Some(OverlayKind::Tombstone) => return Err(not_found(from)),
            None => {
                // Clean base entry: reference the blob at the new path (no fetch).
                match self.resolve_base_entry(from, policy)? {
                    Some(e) if e.mode.is_file() || matches!(e.mode, GitMode::Symlink) => {
                        self.overlay.put_base_ref(to, e.object_id, e.mode)?;
                    }
                    Some(e) if matches!(e.mode, GitMode::Tree) => {
                        return Err(Error::unsupported(
                            "directory rename is not yet implemented (file renames are)",
                        ));
                    }
                    _ => return Err(not_found(from)),
                }
            }
        }
        self.delete(from, policy)
    }

    // ---- staging (spec §23) ----

    /// Stage the working content at `path` (`git lazy-mount add`). Applies clean
    /// filters via Git plumbing and writes the blob, leaving the overlay
    /// untouched (spec §23).
    pub fn stage_path(&self, path: &RepoPath, policy: FetchPolicy) -> Result<()> {
        match self.overlay.entry(path) {
            Some(OverlayKind::Tombstone) => self.stage.remove(path.clone()),
            Some(OverlayKind::File { executable }) => {
                let bytes = self.overlay.read_content(path)?.unwrap_or_default();
                let oid = self.store.hash_blob_clean(path.as_bytes(), &bytes, true)?;
                let mode = if executable {
                    GitMode::Executable
                } else {
                    GitMode::Regular
                };
                self.stage.set(path.clone(), oid, mode)
            }
            Some(OverlayKind::Symlink) => {
                let bytes = self.overlay.read_content(path)?.unwrap_or_default();
                let oid = self.store.hash_blob_raw(&bytes, true)?;
                self.stage.set(path.clone(), oid, GitMode::Symlink)
            }
            Some(OverlayKind::BaseRef { oid, mode }) => self.stage.set(path.clone(), oid, mode),
            None => match self.base_entry_ref(path, policy)? {
                Some((oid, mode)) => self.stage.set(path.clone(), oid, mode),
                None => Err(not_found(path)),
            },
        }
    }

    /// Stage every working-tree change (`add -A`): all overlay entries and
    /// tombstones. `O(overlay)`.
    pub fn stage_all(&self, policy: FetchPolicy) -> Result<usize> {
        let entries = self.overlay.entries();
        let n = entries.len();
        for (p, _k) in entries {
            self.stage_path(&p, policy)?;
        }
        Ok(n)
    }

    /// Unstage a path (`git lazy-mount unstage` / `restore --staged`).
    pub fn unstage(&self, path: &RepoPath) -> Result<()> {
        self.stage.unstage(path)
    }

    /// Restore the working tree at `path` to match the base (drop overlay edit).
    pub fn restore_worktree(&self, path: &RepoPath) -> Result<()> {
        self.overlay.clear(path)
    }

    // ---- status & diff (pure; spec §2.7) ----

    /// Compute three-tree status, `O(staged + overlay)` (spec §11, §49). Pure:
    /// no fetches, no writes, no ref changes.
    pub fn status(&self, policy: FetchPolicy) -> Result<Vec<StatusEntry>> {
        use std::collections::BTreeSet;
        let mut paths: BTreeSet<RepoPath> = BTreeSet::new();
        for (p, _) in self.stage.entries() {
            paths.insert(p);
        }
        for (p, _) in self.overlay.entries() {
            paths.insert(p);
        }

        let mut out = Vec::new();
        for path in paths {
            let head = self.base_entry_ref(&path, policy)?;
            let staged = match self.stage.get(&path) {
                Some(StagedChange::Set { oid, mode }) => Some((oid, mode)),
                Some(StagedChange::Remove) => None,
                Some(StagedChange::IntentToAdd) | None => head.clone(),
            };
            let work = self.work_ref(&path, &head)?;

            let x = code(&head, &staged);
            let y = code(&staged, &work);
            let entry = StatusEntry {
                path,
                index: x,
                worktree: y,
            };
            if entry.is_changed() {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// The working-tree `(oid, mode)` for a path, hashing overlay content
    /// **without** writing it (status must not persist dirty blobs; spec §2.7).
    fn work_ref(&self, path: &RepoPath, head: &Option<EntryRef>) -> Result<Option<EntryRef>> {
        match self.overlay.entry(path) {
            Some(OverlayKind::Tombstone) => Ok(None),
            Some(OverlayKind::File { executable }) => {
                let bytes = self.overlay.read_content(path)?.unwrap_or_default();
                let oid = self.store.hash_blob_clean(path.as_bytes(), &bytes, false)?;
                let mode = if executable {
                    GitMode::Executable
                } else {
                    GitMode::Regular
                };
                Ok(Some((oid, mode)))
            }
            Some(OverlayKind::Symlink) => {
                let bytes = self.overlay.read_content(path)?.unwrap_or_default();
                let oid = self.store.hash_blob_raw(&bytes, false)?;
                Ok(Some((oid, GitMode::Symlink)))
            }
            Some(OverlayKind::BaseRef { oid, mode }) => Ok(Some((oid, mode))),
            None => Ok(head.clone()),
        }
    }

    // ---- commit (spec §24) ----

    /// Create an ordinary Git commit from the staged delta, advance the private
    /// head ref and (best-effort) the attached branch with compare-and-swap, and
    /// seal the transaction in the operation log.
    pub fn commit(&self, message: &str, policy: FetchPolicy) -> Result<CommitOutcome> {
        let staged = self.stage.entries();
        if staged.is_empty() {
            return Err(
                Error::new(ErrorCode::Configuration, "nothing staged to commit")
                    .with_action("stage changes with `git lazy-mount add` first"),
            );
        }

        let changes: Vec<(RepoPath, TreeChange)> = staged
            .iter()
            .filter_map(|(p, c)| match c {
                StagedChange::Set { oid, mode } => Some((
                    p.clone(),
                    TreeChange::Set {
                        oid: oid.clone(),
                        mode: *mode,
                    },
                )),
                StagedChange::Remove => Some((p.clone(), TreeChange::Remove)),
                StagedChange::IntentToAdd => None,
            })
            .collect();

        let base_commit = self.base_commit();
        let base_tree = self.base_tree(policy)?;
        let new_tree = build_tree(
            &self.store,
            self.provider.as_ref(),
            base_tree,
            changes,
            policy,
        )?;

        let parents: Vec<ObjectId> = base_commit.iter().cloned().collect();
        let commit = self.store.commit_tree(&CommitParams {
            tree: new_tree,
            parents,
            message: message.to_string(),
            author: self.cfg.identity.clone(),
            committer: self.cfg.identity.clone(),
            sign: false,
        })?;

        // Advance the private workspace head with CAS (must always succeed).
        self.store
            .update_ref_cas(&self.cfg.workspace_head_ref, &commit, base_commit.as_ref())?;

        // Best-effort advance the attached public branch with CAS (spec §14).
        let mut branch_advanced = false;
        let mut divergence = None;
        if let Some(branch) = &self.cfg.attached_branch {
            let expected = self.attached_expected.lock().unwrap().clone();
            match self
                .store
                .update_ref_cas(branch, &commit, expected.as_ref())
            {
                Ok(()) => {
                    branch_advanced = true;
                    *self.attached_expected.lock().unwrap() = Some(commit.clone());
                }
                Err(e) if e.code == ErrorCode::ConcurrentBranchMovement => {
                    // The workspace commit remains reachable via the head ref.
                    divergence = Some(e);
                }
                Err(e) => return Err(e),
            }
        }

        // Reset stage; dematerialize entries that are now clean (spec §24 §10/§12).
        self.stage.clear()?;
        self.dematerialize_committed(&staged)?;

        *self.base.lock().unwrap() = Some(commit.clone());
        let new_gen = {
            let mut g = self.generation.lock().unwrap();
            *g += 1;
            *g
        };

        // Seal the operation.
        let mut view = WorkspaceView::root(glm_core::WorkspaceViewId(vec![]), Some(commit.clone()));
        view.attached_branch = self.cfg.attached_branch.clone();
        view.attached_branch_expected = self.attached_expected.lock().unwrap().clone();
        view.mount_generation = new_gen;
        let mut effects = vec![];
        if branch_advanced {
            effects.push(ExternalSideEffect {
                kind: "branch-advance".into(),
                target: self.cfg.attached_branch.clone().unwrap_or_default(),
                state: "acknowledged".into(),
            });
        }
        let op = self.oplog.commit(
            view,
            NewOperation {
                cause: Cause::Command("commit".into()),
                description: format!("commit: {}", first_line(message)),
                durability: glm_core::Durability::OperationSealed,
                external_effects: effects,
            },
        )?;
        self.oplog.mark_applied(new_gen)?;

        Ok(CommitOutcome {
            commit,
            operation: op,
            branch_advanced,
            divergence,
        })
    }

    fn dematerialize_committed(&self, staged: &[(RepoPath, StagedChange)]) -> Result<()> {
        for (p, c) in staged {
            match c {
                StagedChange::Set { oid, .. } => {
                    // If the working overlay still equals the committed content,
                    // it is now clean -> drop it. Otherwise preserve the edit.
                    let still_equal = match self.overlay.entry(p) {
                        Some(OverlayKind::BaseRef { oid: o, .. }) => &o == oid,
                        Some(OverlayKind::File { .. }) => {
                            let bytes = self.overlay.read_content(p)?.unwrap_or_default();
                            &self.store.hash_blob_clean(p.as_bytes(), &bytes, false)? == oid
                        }
                        Some(OverlayKind::Symlink) => {
                            let bytes = self.overlay.read_content(p)?.unwrap_or_default();
                            &self.store.hash_blob_raw(&bytes, false)? == oid
                        }
                        _ => false,
                    };
                    if still_equal {
                        self.overlay.clear(p)?;
                    }
                }
                StagedChange::Remove => {
                    // The deletion is now in the base; drop the tombstone.
                    if self.overlay.is_tombstone(p) {
                        self.overlay.clear(p)?;
                    }
                }
                StagedChange::IntentToAdd => {}
            }
        }
        Ok(())
    }

    /// Push the attached branch (or the workspace head) to the remote using a
    /// `--force-with-lease` compare-and-swap (spec §13 saga step).
    pub fn push(&self, policy: FetchPolicy) -> Result<()> {
        let _ = policy;
        let remote = self
            .cfg
            .remote
            .as_deref()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "no remote configured"))?;
        let branch =
            self.cfg.attached_branch.as_deref().ok_or_else(|| {
                Error::new(ErrorCode::Configuration, "no attached branch to push")
            })?;
        let commit = self
            .base_commit()
            .ok_or_else(|| Error::new(ErrorCode::Configuration, "nothing to push"))?;
        let refspec = format!("{}:{}", commit.to_hex(), branch);
        self.store.push(remote, &refspec, Some((branch, None)))
    }
}

/// The result of a [`Workspace::commit`].
#[derive(Debug)]
pub struct CommitOutcome {
    /// The created commit.
    pub commit: ObjectId,
    /// The sealed operation id.
    pub operation: OperationId,
    /// Whether the attached branch was advanced.
    pub branch_advanced: bool,
    /// If the attached branch had moved, the divergence error (commit is still
    /// reachable via the private head ref).
    pub divergence: Option<Error>,
}

fn code(from: &Option<EntryRef>, to: &Option<EntryRef>) -> StatusCode {
    match (from, to) {
        (None, None) => StatusCode::Unmodified,
        (None, Some(_)) => StatusCode::Added,
        (Some(_), None) => StatusCode::Deleted,
        (Some(a), Some(b)) => {
            if a.0 == b.0 && a.1 == b.1 {
                StatusCode::Unmodified
            } else if is_type_change(a.1, b.1) {
                StatusCode::TypeChanged
            } else {
                StatusCode::Modified
            }
        }
    }
}

fn is_type_change(a: GitMode, b: GitMode) -> bool {
    let class = |m: GitMode| match m {
        GitMode::Regular | GitMode::Executable => 0,
        GitMode::Symlink => 1,
        GitMode::Tree => 2,
        GitMode::Gitlink => 3,
    };
    class(a) != class(b)
}

fn not_found(path: &RepoPath) -> Error {
    Error::new(
        ErrorCode::RemoteMissingObject,
        format!("path not found: {}", path.escape()),
    )
}

fn rel_after(dir: &RepoPath, p: &RepoPath) -> Option<Vec<Vec<u8>>> {
    if dir.is_root() {
        return Some(p.components().map(|c| c.to_vec()).collect());
    }
    if !dir.is_prefix_of(p) || p == dir {
        return None;
    }
    let dlen = dir.as_bytes().len();
    // p == dir + "/" + rest
    let rest = &p.as_bytes()[dlen + 1..];
    Some(rest.split(|&b| b == b'/').map(|c| c.to_vec()).collect())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}
