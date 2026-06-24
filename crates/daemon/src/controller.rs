//! In-process control logic: clone, open, and resolve mounts (spec §6, §7, §39).
//!
//! The CLI calls these directly. A future socketed daemon process (spec §39)
//! would expose the same operations over the versioned control protocol; the
//! transport is intentionally separated from this logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glm_core::{Error, ErrorCode, RepoId, Result};
use glm_git_store::{FetchOptions, GitStore, Identity};
use glm_object_provider::{GitObjectProvider, ObjectProvider};
use glm_platform::{DataRoots, Layout};
use glm_workspace::{Workspace, WorkspaceConfig};
use sha2::{Digest, Sha256};

use crate::registry::{MountSpec, MountState, Registry};

/// Options for [`Controller::clone_repo`] (spec §7).
#[derive(Clone, Debug)]
pub struct CloneOptions {
    /// Partial-clone filter; `None` only when `allow_full_object_clone`.
    pub filter: Option<String>,
    /// Branch to attach to (defaults tried: `main`, then `master`).
    pub branch: Option<String>,
    /// Shallow depth, if any.
    pub depth: Option<u32>,
    /// Permit a full-object clone if the remote rejects the filter.
    pub allow_full_object_clone: bool,
    /// Identity for commits made in this workspace.
    pub identity: Option<Identity>,
}

impl Default for CloneOptions {
    fn default() -> Self {
        CloneOptions {
            filter: Some("blob:none".into()),
            branch: None,
            depth: None,
            allow_full_object_clone: false,
            identity: None,
        }
    }
}

/// An opened mount: the store, provider, and workspace wired together.
pub struct OpenMount {
    /// The shared bare store.
    pub store: GitStore,
    /// The transactional workspace.
    pub workspace: Workspace,
    /// The mount's registry record.
    pub spec: MountSpec,
}

/// Drives mount lifecycle operations against the per-user data roots.
pub struct Controller {
    roots: DataRoots,
}

impl Controller {
    /// Build a controller over the given data roots.
    pub fn new(roots: DataRoots) -> Controller {
        Controller { roots }
    }

    /// Build a controller over the default per-user data roots.
    pub fn for_user() -> Controller {
        Controller::new(DataRoots::for_user())
    }

    fn layout(&self) -> Layout<'_> {
        self.roots.layout()
    }

    fn registry(&self) -> Result<Registry> {
        Registry::open(self.layout().daemon_dir().join("mounts.json"))
    }

    /// Clone `url` into `mountpoint` as a lazily-populated workspace (spec §7).
    /// Does NOT check out files; sets up the shared store and the workspace.
    pub fn clone_repo(
        &self,
        url: &str,
        mountpoint: &Path,
        opts: &CloneOptions,
    ) -> Result<MountSpec> {
        let repo_id = glm_platform::repo_id(url);
        let store_dir = self.layout().git_store_dir(&repo_id);

        // Initialize (or reuse) the shared bare store.
        let store = if store_dir.join("HEAD").exists() {
            GitStore::open(&store_dir)?
        } else {
            let store = GitStore::init_bare(&store_dir, None)?;
            store.add_remote("origin", url)?;
            if is_local_url(url) {
                store.set_config("protocol.file.allow", "always")?;
            }
            store
        };

        // Fetch with the requested filter (or full objects if explicitly allowed).
        let filter = if opts.allow_full_object_clone {
            None
        } else {
            opts.filter.clone()
        };
        let fetch = FetchOptions {
            filter: filter.clone(),
            depth: opts.depth,
            tags: false,
        };
        // Fetch only the branch we attach to. Huge repos have hundreds of
        // branches and fetching every ref dominates clone time (the difference
        // between ~2s and minutes on microsoft/TypeScript). Resolve the target
        // branch name — explicit `--branch`, else the remote's default `HEAD`,
        // else `main` — and fetch just its ref into the remote-tracking namespace.
        let target_branch = match opts.branch.clone() {
            Some(b) => b,
            None => store
                .remote_head_branch("origin")
                .ok()
                .flatten()
                .unwrap_or_else(|| "main".to_string()),
        };
        let refspec = format!("+refs/heads/{target_branch}:refs/remotes/origin/{target_branch}");
        if let Err(e) = store.fetch("origin", &[refspec.as_str()], &fetch) {
            if e.code == ErrorCode::UnsupportedRemoteCapability && filter.is_some() {
                return Err(e.context(
                    "the remote does not support the requested partial-clone filter; \
                     re-run with --allow-full-object-clone to clone all objects \
                     (this still does not check out files)",
                ));
            }
            return Err(e);
        }

        // Resolve the attached branch tip (just fetched into refs/remotes/origin).
        let (branch_name, base) = self.resolve_branch(&store, Some(&target_branch))?;
        let attached_branch = format!("refs/heads/{branch_name}");
        if store.resolve_ref(&attached_branch)?.is_none() {
            store.update_ref_cas(&attached_branch, &base, None)?;
        }

        // Create the workspace.
        let ws_id = workspace_id(mountpoint, &repo_id);
        let ws_dir = self
            .layout()
            .workspace_dir(&glm_core::WorkspaceId(ws_id.clone()));
        let workspace_head_ref = format!("refs/lazy-mount/workspaces/{ws_id}/head");
        let provider: Arc<dyn ObjectProvider> =
            Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
        let cfg = WorkspaceConfig {
            workspace_head_ref: workspace_head_ref.clone(),
            attached_branch: Some(attached_branch.clone()),
            remote: Some("origin".into()),
            identity: opts.identity.clone(),
        };
        Workspace::open_or_create(store.clone(), provider, &ws_dir, cfg, Some(base))?;

        // Create the mountpoint directory (a real kernel mount is the FUSE
        // backend's job; without it this is the workspace's anchor on disk).
        std::fs::create_dir_all(mountpoint)?;

        let spec = MountSpec {
            id: ws_id,
            mountpoint: canonicalize(mountpoint),
            repo_id: repo_id.0,
            store_dir,
            ws_dir,
            remote: Some("origin".into()),
            attached_branch: Some(attached_branch),
            workspace_head_ref,
            filter,
            state: MountState::Mounted,
        };
        self.registry()?.upsert(spec.clone())?;
        Ok(spec)
    }

    /// Open an already-registered mount.
    pub fn open(&self, spec: &MountSpec, identity: Option<Identity>) -> Result<OpenMount> {
        let store = GitStore::open(&spec.store_dir)?;
        let provider: Arc<dyn ObjectProvider> =
            Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
        let cfg = WorkspaceConfig {
            workspace_head_ref: spec.workspace_head_ref.clone(),
            attached_branch: spec.attached_branch.clone(),
            remote: spec.remote.clone(),
            identity,
        };
        let workspace =
            Workspace::open_or_create(store.clone(), provider, &spec.ws_dir, cfg, None)?;
        Ok(OpenMount {
            store,
            workspace,
            spec: spec.clone(),
        })
    }

    /// Resolve which mount a command targets: an explicit mountpoint, else the
    /// mount containing `cwd`, else the sole mount.
    pub fn resolve_mount(&self, explicit: Option<&Path>, cwd: &Path) -> Result<MountSpec> {
        let reg = self.registry()?;
        if let Some(path) = explicit {
            let canon = canonicalize(path);
            return reg.find_for_path(&canon)?.ok_or_else(|| {
                Error::new(
                    ErrorCode::MountLifecycle,
                    format!("no mount at {}", canon.display()),
                )
            });
        }
        if let Some(spec) = reg.find_for_path(&canonicalize(cwd))? {
            return Ok(spec);
        }
        let all = reg.list()?;
        match all.len() {
            1 => Ok(all.into_iter().next().unwrap()),
            0 => Err(
                Error::new(ErrorCode::MountLifecycle, "no mounts registered")
                    .with_action("run `git lazy-mount clone <url> <mountpoint>` first"),
            ),
            _ => Err(Error::new(
                ErrorCode::MountLifecycle,
                "multiple mounts; specify one with --mount <path> or run inside it",
            )),
        }
    }

    /// List all registered mounts.
    pub fn list(&self) -> Result<Vec<MountSpec>> {
        self.registry()?.list()
    }

    /// Unregister a mount (idempotent).
    pub fn unmount(&self, mountpoint: &Path) -> Result<bool> {
        self.registry()?.remove(&canonicalize(mountpoint))
    }

    fn resolve_branch(
        &self,
        store: &GitStore,
        requested: Option<&str>,
    ) -> Result<(String, glm_core::ObjectId)> {
        let candidates: Vec<String> = match requested {
            Some(b) => vec![b.to_string()],
            None => vec!["main".to_string(), "master".to_string()],
        };
        for b in &candidates {
            if let Some(oid) = store.resolve_ref(&format!("refs/remotes/origin/{b}"))? {
                return Ok((b.clone(), oid));
            }
        }
        Err(Error::new(
            ErrorCode::Configuration,
            format!(
                "could not resolve branch ({}) on the remote",
                candidates.join(" or ")
            ),
        )
        .with_action("pass --branch <name>"))
    }
}

fn is_local_url(url: &str) -> bool {
    url.starts_with("file://") || url.starts_with('/') || url.starts_with("./")
}

fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn workspace_id(mountpoint: &Path, repo: &RepoId) -> String {
    let mut h = Sha256::new();
    h.update(canonicalize(mountpoint).to_string_lossy().as_bytes());
    h.update([0]);
    h.update(repo.0.as_bytes());
    hex::encode(&h.finalize()[..8])
}
