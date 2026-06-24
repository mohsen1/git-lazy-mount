//! Transparent per-workspace admin Git repository (redesign.md §6, §10.2).
//!
//! `git clone --filter=blob:none --no-checkout --separate-git-dir=<gitdir> <url>
//! <anchor>`, then `core.worktree=<mountpoint>` — so stock Git resolves the
//! repository through a synthetic `.git` gitfile the FUSE projection serves at
//! the mount root, and operates on the mounted worktree using its normal index,
//! refs, locks, and hooks. The admin gitdir lives on a **native** filesystem,
//! never inside FUSE (redesign.md §6). This is the redesign's `git-repo`; Git is
//! authoritative for all repository state (redesign.md §7).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use glm_core::{Error, ErrorCode, ObjectId, Result};
use glm_git_store::GitStore;

/// Options for the transparent clone (redesign.md §10.2).
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Branch to attach to; `None` = the remote's default.
    pub branch: Option<String>,
    /// Shallow depth; `None` = full history (the default; §10.2).
    pub depth: Option<u32>,
    /// Partial-clone filter; defaults to `blob:none`.
    pub filter: Option<String>,
    /// Permit a full-object clone if the remote rejects the filter (§10.2).
    pub allow_full_object_clone: bool,
}

impl Default for CloneOptions {
    fn default() -> Self {
        CloneOptions {
            branch: None,
            depth: None,
            filter: Some("blob:none".into()),
            allow_full_object_clone: false,
        }
    }
}

/// A per-workspace admin Git repository: a real partial clone whose gitdir lives
/// outside the mount, with the mountpoint configured as its worktree.
pub struct AdminRepo {
    gitdir: PathBuf,
    worktree: PathBuf,
    store: GitStore,
}

impl AdminRepo {
    /// Transparent clone (redesign.md §6.1): create the admin gitdir and point it
    /// at `worktree`. `anchor` is a temporary clone anchor that is discarded after
    /// init — we do **not** depend on a physical checkout (§6.1). A full-object
    /// clone (filter rejected) still implies **no** checkout (§10.2).
    pub fn clone(
        url: &str,
        gitdir: &Path,
        worktree: &Path,
        anchor: &Path,
        opts: &CloneOptions,
    ) -> Result<AdminRepo> {
        let gitdir = absolute(gitdir)?;
        let worktree = absolute(worktree)?;

        let mut cmd = Command::new("git");
        // file:// / local-path remotes need this in modern Git.
        if is_local_url(url) {
            cmd.arg("-c").arg("protocol.file.allow=always");
        }
        cmd.arg("clone").arg("--no-checkout");
        cmd.arg(format!("--separate-git-dir={}", gitdir.display()));
        let filter = if opts.allow_full_object_clone {
            None
        } else {
            opts.filter.clone()
        };
        if let Some(f) = &filter {
            cmd.arg(format!("--filter={f}"));
        }
        if let Some(d) = opts.depth {
            cmd.arg(format!("--depth={d}"));
        }
        if let Some(b) = &opts.branch {
            cmd.arg("--branch").arg(b);
        }
        cmd.arg(url).arg(anchor);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        run(cmd, "clone")?;

        // Point the gitdir at the mountpoint as its worktree (stock Git then
        // resolves the repo via the synthetic `.git` gitfile + core.worktree).
        let store = GitStore::open(&gitdir)?;
        store.set_config("core.worktree", &worktree.to_string_lossy())?;
        store.set_config("core.bare", "false")?;
        // A `file://` promisor needs this for *lazy* fetches too (not just the
        // initial clone) — otherwise a later blob fault is refused.
        if is_local_url(url) {
            store.set_config("protocol.file.allow", "always")?;
        }

        // The anchor's own `.git` gitfile is discarded; the projection serves the
        // synthetic one. Do not depend on a temporary physical checkout (§6.1).
        let _ = std::fs::remove_dir_all(anchor);
        std::fs::create_dir_all(&worktree)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("create worktree: {e}")))?;

        Ok(AdminRepo {
            gitdir,
            worktree,
            store,
        })
    }

    /// Open an already-initialized admin repo (e.g. on daemon restart).
    pub fn open(gitdir: &Path, worktree: &Path) -> Result<AdminRepo> {
        Ok(AdminRepo {
            gitdir: absolute(gitdir)?,
            worktree: absolute(worktree)?,
            store: GitStore::open(gitdir)?,
        })
    }

    /// The native admin gitdir (outside FUSE).
    pub fn gitdir(&self) -> &Path {
        &self.gitdir
    }
    /// The mountpoint configured as `core.worktree`.
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }
    /// The Git CLI adapter over the admin gitdir (object reads, refs, config).
    pub fn store(&self) -> &GitStore {
        &self.store
    }

    /// The exact bytes of the synthetic root `.git` gitfile (redesign.md §6).
    /// Stock Git reads this and follows it to the admin gitdir.
    pub fn synthetic_gitfile(&self) -> Vec<u8> {
        format!("gitdir: {}\n", self.gitdir.display()).into_bytes()
    }

    /// The current HEAD commit, or `None` on an unborn branch.
    pub fn head_commit(&self) -> Result<Option<ObjectId>> {
        self.store.rev_parse("HEAD")
    }

    /// The HEAD commit's tree — the initial projection baseline (§8). `None` if
    /// HEAD is unborn.
    pub fn head_tree(&self) -> Result<Option<ObjectId>> {
        self.store.rev_parse("HEAD^{tree}")
    }
}

fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .map_err(|e| Error::new(ErrorCode::Internal, format!("cwd: {e}")))
    }
}

fn is_local_url(url: &str) -> bool {
    url.starts_with("file://")
        || url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn git {what}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().take(3).collect::<Vec<_>>().join("; ");
        Err(Error::new(
            ErrorCode::Internal,
            format!("git {what} failed: {first}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_clone_builds_a_worktree_repo_without_checkout() {
        let remote = glm_testkit::seed_remote(&[
            ("README.md", b"hello\n"),
            ("src/main.rs", b"fn main() {}\n"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join("git");
        let worktree = tmp.path().join("mnt");
        let anchor = tmp.path().join("anchor");

        let repo = AdminRepo::clone(
            &remote.url,
            &gitdir,
            &worktree,
            &anchor,
            &CloneOptions::default(),
        )
        .unwrap();

        // HEAD + baseline tree resolve from the admin gitdir.
        assert!(repo.head_commit().unwrap().is_some(), "HEAD resolves");
        let tree = repo.head_tree().unwrap().expect("baseline tree");
        // The baseline tree is readable without checkout (trees come down, blobs
        // do not under blob:none).
        let entries = repo.store().read_tree(&tree, false).unwrap();
        let names: Vec<_> = entries.entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.iter().any(|n| n == b"README.md"));
        assert!(names.iter().any(|n| n == b"src"));

        // No physical checkout happened (the worktree is empty — the projection
        // serves it), and the anchor was discarded.
        assert!(!anchor.exists(), "temporary anchor discarded");
        assert!(
            std::fs::read_dir(&worktree).unwrap().next().is_none(),
            "no physical checkout in the worktree"
        );

        // The synthetic gitfile points at the admin gitdir.
        let gf = repo.synthetic_gitfile();
        assert!(gf.starts_with(b"gitdir: "));
        assert!(gf.ends_with(b"\n"));
        assert!(String::from_utf8_lossy(&gf).contains(&*gitdir.to_string_lossy()));
    }
}
