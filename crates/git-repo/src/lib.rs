//! Transparent per-workspace admin Git repository.
//!
//! `git clone --filter=blob:none --no-checkout --separate-git-dir=<gitdir> <url>
//! <anchor>`, then `core.worktree=<mountpoint>` — so stock Git resolves the
//! repository through a synthetic `.git` gitfile the FUSE projection serves at
//! the mount root, and operates on the mounted worktree using its normal index,
//! refs, locks, and hooks. The admin gitdir lives on a **native** filesystem,
//! never inside FUSE. This is the design's `git-repo`; Git is
//! authoritative for all repository state.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use glm_core::{Error, ErrorCode, ObjectId, Result};
use glm_git_store::GitStore;

/// Options for the transparent clone.
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// Branch to attach to; `None` = the remote's default.
    pub branch: Option<String>,
    /// Shallow depth; `None` = full history (the default).
    pub depth: Option<u32>,
    /// Partial-clone filter; defaults to `blob:none`.
    pub filter: Option<String>,
    /// Permit a full-object clone if the remote rejects the filter.
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
    /// Transparent clone: create the admin gitdir and point it
    /// at `worktree`. `anchor` is a temporary clone anchor that is discarded after
    /// init — we do **not** depend on a physical checkout. A full-object
    /// clone (filter rejected) still implies **no** checkout.
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
        // synthetic one. Do not depend on a temporary physical checkout.
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

    /// The exact bytes of the synthetic root `.git` gitfile.
    /// Stock Git reads this and follows it to the admin gitdir.
    pub fn synthetic_gitfile(&self) -> Vec<u8> {
        format!("gitdir: {}\n", self.gitdir.display()).into_bytes()
    }

    /// The current HEAD commit, or `None` on an unborn branch.
    pub fn head_commit(&self) -> Result<Option<ObjectId>> {
        self.store.rev_parse("HEAD")
    }

    /// The HEAD commit's tree — the initial projection baseline. `None` if
    /// HEAD is unborn.
    pub fn head_tree(&self) -> Result<Option<ObjectId>> {
        self.store.rev_parse("HEAD^{tree}")
    }

    /// A `git --git-dir=<gitdir>` command with lazy fetch disabled, so index/
    /// inspection plumbing can never trigger a network blob fetch.
    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(&self.gitdir);
        cmd.env("GIT_NO_LAZY_FETCH", "1");
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    /// Populate the real `.git/index` from the baseline (HEAD) tree **without
    /// fetching any blobs**. This is `git read-tree HEAD`:
    /// O(tracked paths), reads tree objects only (present under `blob:none`),
    /// touches no working-tree files. The real index is then the single stage
    /// that stock `git add`/`status`/`commit` operate on.
    pub fn build_index(&self) -> Result<()> {
        let mut cmd = self.git();
        cmd.args(["read-tree", "HEAD"]);
        run(cmd, "read-tree")?;
        Ok(())
    }

    /// The paths tracked in the real index (`git ls-files -z`), as raw bytes
    ///. Reads the index only.
    pub fn tracked_paths(&self) -> Result<Vec<Vec<u8>>> {
        let mut cmd = self.git();
        cmd.args(["ls-files", "-z"]);
        let out = cmd
            .output()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn ls-files: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::new(
                ErrorCode::Internal,
                format!("ls-files failed: {}", err.trim()),
            ));
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec())
            .collect())
    }

    /// Seed the FSMonitor index extension so the **first** `git status`/`git diff`
    /// faults zero blobs.
    ///
    /// A freshly `read-tree`'d index carries no FSMonitor extension, so on the
    /// first status git has no valid bits to trust: it stats every entry (and a
    /// `blob:none` `getattr` faults the blob for its size), populates the
    /// extension, and only *then* could trust the hook. By marking every entry
    /// `CE_FSMONITOR_VALID` up front, git trusts the daemon's change journal
    /// immediately and skips the stat/content check for every unchanged path.
    ///
    /// `git update-index --fsmonitor-valid` queries the configured hook for the
    /// current token and writes the extension, so this requires `core.fsmonitor`
    /// already set and the (empty) journal present so the hook answers the seq-0
    /// bootstrap query. Best-effort by contract: callers treat failure as "fall
    /// back to the eager first status", never as a mount failure.
    pub fn seed_fsmonitor_valid(&self) -> Result<()> {
        let all = self.tracked_paths()?;
        if all.is_empty() {
            return Ok(());
        }
        // Carve out paths whose working-tree bytes can differ from the stored blob
        // (checkout conversions): seeding those valid could hide a real diff.
        let converted = self.conversion_attributed_paths(&all)?;
        let seedable: Vec<&[u8]> = all
            .iter()
            .map(Vec::as_slice)
            .filter(|p| !converted.contains(*p))
            .collect();
        if seedable.is_empty() {
            return Ok(());
        }
        let mut child = self
            .git()
            .args(["update-index", "-z", "--fsmonitor-valid", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn update-index: {e}")))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::new(ErrorCode::Internal, "update-index stdin".to_string()))?;
            for p in &seedable {
                stdin
                    .write_all(p)
                    .and_then(|()| stdin.write_all(&[0]))
                    .map_err(|e| Error::new(ErrorCode::Internal, format!("pipe paths: {e}")))?;
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("update-index wait: {e}")))?;
        if !out.status.success() {
            return Err(Error::new(
                ErrorCode::Internal,
                format!(
                    "update-index --fsmonitor-valid: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(())
    }

    /// A `git` command that **may** lazily fetch missing objects (unlike
    /// [`Self::git`], which forbids it). Used only to read `.gitattributes` blobs
    /// for the seed carve-out.
    fn git_with_fetch(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(&self.gitdir);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd
    }

    /// Tracked paths whose working-tree bytes can differ from the stored blob
    /// because of a checkout conversion (`filter`, `ident`, `working-tree-encoding`,
    /// or CRLF `eol`). These must not be seeded fsmonitor-valid, so stock git
    /// checks them normally and never hides a real difference.
    ///
    /// Attributes are read from the index (`check-attr --cached`), populated by
    /// `read-tree HEAD`, so this works before the worktree is mounted. When the
    /// repo declares no `.gitattributes` at all (the common case) it returns empty
    /// without fetching or spawning anything.
    fn conversion_attributed_paths(&self, all: &[Vec<u8>]) -> Result<HashSet<Vec<u8>>> {
        let has_attrs = all
            .iter()
            .any(|p| p.rsplit(|&b| b == b'/').next() == Some(b".gitattributes".as_slice()));
        if !has_attrs {
            return Ok(HashSet::new());
        }
        let mut child = self
            .git_with_fetch()
            .args([
                "check-attr",
                "--cached",
                "-z",
                "--stdin",
                "filter",
                "ident",
                "working-tree-encoding",
                "eol",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn check-attr: {e}")))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::new(ErrorCode::Internal, "check-attr stdin".to_string()))?;
            for p in all {
                stdin
                    .write_all(p)
                    .and_then(|()| stdin.write_all(&[0]))
                    .map_err(|e| {
                        Error::new(ErrorCode::Internal, format!("pipe check-attr: {e}"))
                    })?;
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("check-attr wait: {e}")))?;
        if !out.status.success() {
            return Err(Error::new(
                ErrorCode::Internal,
                format!(
                    "check-attr --source: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        // Output is NUL-separated `path, attr, value` triples.
        let mut converted = HashSet::new();
        let mut it = out.stdout.split(|&b| b == 0);
        while let (Some(path), Some(attr), Some(value)) = (it.next(), it.next(), it.next()) {
            if path.is_empty() {
                continue;
            }
            let carve = match attr {
                b"filter" | b"working-tree-encoding" => {
                    value != b"unspecified" && value != b"unset"
                }
                b"ident" => value == b"set",
                b"eol" => value == b"crlf",
                _ => false,
            };
            if carve {
                converted.insert(path.to_vec());
            }
        }
        Ok(converted)
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

    #[test]
    fn build_index_from_baseline_without_fetching_blobs() {
        let remote = glm_testkit::seed_remote(&[
            ("README.md", b"hi\n"),
            ("src/main.rs", b"fn main() {}\n"),
            ("src/lib.rs", b"pub fn f() {}\n"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let repo = AdminRepo::clone(
            &remote.url,
            &tmp.path().join("git"),
            &tmp.path().join("mnt"),
            &tmp.path().join("anchor"),
            &CloneOptions::default(),
        )
        .unwrap();

        // read-tree runs with GIT_NO_LAZY_FETCH=1, so succeeding proves the real
        // index was built from tree objects alone — zero blobs fetched.
        repo.build_index().unwrap();

        let mut paths: Vec<String> = repo
            .tracked_paths()
            .unwrap()
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["README.md", "src/lib.rs", "src/main.rs"]);
        let _ = remote; // keep the promisor alive for the duration
    }
}
