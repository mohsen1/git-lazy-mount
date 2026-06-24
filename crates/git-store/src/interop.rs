//! Git-interop bridge: run **stock** `git` against the shared lazy store so
//! native commands like `git status`, `git log`, `git diff`, and `git commit`
//! work without a kernel mount.
//!
//! The bridge stands up a throwaway *operational gitdir* whose object I/O is
//! routed into the shared store via `GIT_OBJECT_DIRECTORY`, pins a **detached
//! HEAD** at the workspace base, and synthesizes an index from the staged tree
//! with every entry marked **skip-worktree**. The skip-worktree bit (the same
//! mechanism sparse-checkout uses) means an empty/virtual worktree does not
//! manufacture spurious deletions, so `git status` reflects exactly the staged
//! delta and `git commit` records the synthesized index verbatim.
//!
//! The store's promisor remote is mirrored into the operational gitdir so lazy
//! fetch still faults blobs in on demand (e.g. for `git show <path>`). New
//! objects — notably the commit object created by `git commit` — land directly
//! in the shared store; the caller reads back the bridge HEAD to adopt that
//! commit into the workspace.
//!
//! This mechanism is validated end-to-end against real `git` by the crate tests.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use glm_core::{Error, ErrorCode, ObjectId, Result};

use crate::proc::{classify, run, run_checked};
use crate::store::GitStore;

/// The result of an interop bridge invocation.
pub struct InteropOutcome {
    /// The child `git` process's exit status. Propagate its code to the user.
    pub status: std::process::ExitStatus,
    /// The bridge HEAD after the run. Differs from the input `base` exactly when
    /// the command created a commit (e.g. `git commit`), letting the caller
    /// adopt the new commit into the workspace.
    pub head: Option<ObjectId>,
}

impl GitStore {
    /// Run stock `git <args>` against this store through the interop bridge.
    ///
    /// * `scratch` — a directory the bridge owns for its operational gitdir and
    ///   synthesized index (created if missing; safely reused across calls).
    /// * `base` — the commit to pin HEAD to.
    /// * `branch` — the workspace's attached branch (e.g. `refs/heads/main` or
    ///   `main`). When given, HEAD is attached to it so `git status`/`git log`
    ///   read natively ("On branch main"); otherwise HEAD is detached.
    /// * `index_tree` — the tree to synthesize the index from (the workspace
    ///   staged tree). `None` leaves an empty index.
    /// * `args` — the user's git arguments (verb and options).
    ///
    /// stdio is inherited so the user's editor and pager behave natively.
    /// Returns the child's exit status and the bridge HEAD afterwards.
    pub fn interop_run(
        &self,
        scratch: &Path,
        base: &ObjectId,
        branch: Option<&str>,
        index_tree: Option<&ObjectId>,
        args: &[OsString],
    ) -> Result<InteropOutcome> {
        let op_dir = scratch.join("op");
        let index_file = scratch.join("index");
        let objects_dir = self.git_dir().join("objects");
        std::fs::create_dir_all(&op_dir)?;

        // One-time initialization of the operational gitdir.
        if !op_dir.join(".git").exists() {
            self.bridge_init(&op_dir)?;
        }

        // Pin HEAD at the base commit (resettable across reuse). When the
        // workspace has an attached branch, point HEAD at a same-named branch in
        // the throwaway repo so native output reads "On branch <name>"; commits
        // advance that ref and we read it back as HEAD. Otherwise detach.
        let short = branch.map(|b| b.strip_prefix("refs/heads/").unwrap_or(b));
        match short {
            Some(name) if !name.is_empty() => {
                let refname = format!("refs/heads/{name}");
                let mut set = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
                set.args(["update-ref", &refname]).arg(base.to_hex());
                check(&run(set, None)?, "interop update-ref branch")?;
                let mut sym = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
                sym.args(["symbolic-ref", "HEAD", &refname]);
                check(&run(sym, None)?, "interop symbolic-ref HEAD")?;
            }
            _ => {
                let mut pin = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
                pin.args(["update-ref", "--no-deref", "HEAD"])
                    .arg(base.to_hex());
                check(&run(pin, None)?, "interop update-ref HEAD")?;
            }
        }

        // Synthesize the index from the staged tree (or empty it). Remove any
        // stale index first so a single-tree read starts from a clean slate.
        let _ = std::fs::remove_file(&index_file);
        match index_tree {
            Some(tree) => {
                let mut rt = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
                rt.args(["read-tree", &tree.to_hex()]);
                check(&run(rt, None)?, "interop read-tree")?;
                self.mark_skip_worktree(&op_dir, &objects_dir, &index_file)?;
            }
            None => {
                let mut rt = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
                rt.args(["read-tree", "--empty"]);
                check(&run(rt, None)?, "interop read-tree --empty")?;
            }
        }

        // Run the user's command with inherited stdio (editor/pager work).
        let mut cmd = self.bridge_cmd(&op_dir, &objects_dir, &index_file);
        cmd.args(args);
        cmd.stdin(Stdio::inherit());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        let status = cmd.status().map_err(|e| {
            Error::new(ErrorCode::Internal, format!("failed to run git: {e}")).with_source(e)
        })?;

        // Read back the (possibly advanced) HEAD.
        let head = self.bridge_head(&op_dir, &objects_dir, &index_file)?;
        Ok(InteropOutcome { status, head })
    }

    /// Build a `git` command targeting the operational gitdir with object I/O
    /// routed into the shared store and the synthesized index in place.
    fn bridge_cmd(&self, op_dir: &Path, objects_dir: &Path, index_file: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(op_dir);
        // Detachment is an implementation detail, not a user choice.
        cmd.args(["-c", "advice.detachedHead=false"]);
        cmd.env("GIT_OBJECT_DIRECTORY", objects_dir);
        cmd.env("GIT_INDEX_FILE", index_file);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        cmd
    }

    /// Initialize the operational gitdir and mirror the store's promisor remote.
    fn bridge_init(&self, op_dir: &Path) -> Result<()> {
        let mut init = Command::new("git");
        init.arg("init").arg("-q").arg(op_dir);
        init.env("GIT_TERMINAL_PROMPT", "0");
        run_checked(init, None, "interop init")?;

        if let Some((url, filter)) = self.promisor_remote()? {
            // Mirror as "origin" in the throwaway repo; only object I/O matters.
            self.bridge_config(op_dir, "extensions.partialClone", "origin")?;
            self.bridge_config(op_dir, "remote.origin.url", &url)?;
            self.bridge_config(op_dir, "remote.origin.promisor", "true")?;
            self.bridge_config(op_dir, "remote.origin.partialclonefilter", &filter)?;
            self.bridge_config(
                op_dir,
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            )?;
        }
        Ok(())
    }

    fn bridge_config(&self, op_dir: &Path, key: &str, value: &str) -> Result<()> {
        let mut c = Command::new("git");
        c.arg("-C").arg(op_dir).args(["config", key, value]);
        c.env("GIT_TERMINAL_PROMPT", "0");
        run_checked(c, None, "interop config")?;
        Ok(())
    }

    /// Mark every index entry skip-worktree so the empty bridge worktree does
    /// not register as mass deletions in `git status`.
    fn mark_skip_worktree(
        &self,
        op_dir: &Path,
        objects_dir: &Path,
        index_file: &Path,
    ) -> Result<()> {
        let mut ls = self.bridge_cmd(op_dir, objects_dir, index_file);
        ls.args(["ls-files", "-z"]);
        let listed = run(ls, None)?;
        check(&listed, "interop ls-files")?;
        if listed.stdout.is_empty() {
            return Ok(());
        }
        let mut upd = self.bridge_cmd(op_dir, objects_dir, index_file);
        upd.args(["update-index", "-z", "--skip-worktree", "--stdin"]);
        check(&run(upd, Some(&listed.stdout))?, "interop update-index")?;
        Ok(())
    }

    /// Read back the bridge HEAD after a run (the commit produced by, e.g.,
    /// `git commit`, or the unchanged base for read-only commands).
    fn bridge_head(
        &self,
        op_dir: &Path,
        objects_dir: &Path,
        index_file: &Path,
    ) -> Result<Option<ObjectId>> {
        let mut c = self.bridge_cmd(op_dir, objects_dir, index_file);
        c.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        let r = run(c, None)?;
        if !r.status_ok {
            return Ok(None);
        }
        let hex = String::from_utf8_lossy(&r.stdout);
        let hex = hex.trim();
        if hex.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            ObjectId::parse_hex(self.format().clone(), hex).map_err(|e| {
                Error::new(ErrorCode::Internal, format!("bad bridge HEAD oid: {e}"))
            })?,
        ))
    }

    /// The store's promisor remote as `(url, filter)`, if one is configured.
    /// glm configures the promisor as `origin`; `extensions.partialClone` is
    /// honored when present.
    fn promisor_remote(&self) -> Result<Option<(String, String)>> {
        let name = self
            .read_store_config("extensions.partialClone")?
            .unwrap_or_else(|| "origin".to_string());
        let url = match self.read_store_config(&format!("remote.{name}.url"))? {
            Some(u) => u,
            None => return Ok(None),
        };
        let filter = self
            .read_store_config(&format!("remote.{name}.partialclonefilter"))?
            .unwrap_or_else(|| "blob:none".to_string());
        Ok(Some((url, filter)))
    }

    fn read_store_config(&self, key: &str) -> Result<Option<String>> {
        let mut c = Command::new("git");
        c.arg("--git-dir").arg(self.git_dir());
        c.env("GIT_TERMINAL_PROMPT", "0");
        c.args(["config", "--get", key]);
        let r = run(c, None)?;
        if !r.status_ok {
            return Ok(None);
        }
        let v = String::from_utf8_lossy(&r.stdout).trim().to_string();
        Ok(if v.is_empty() { None } else { Some(v) })
    }
}

fn check(r: &crate::proc::Run, what: &str) -> Result<()> {
    if r.status_ok {
        Ok(())
    } else {
        Err(classify(&r.stderr, what))
    }
}
