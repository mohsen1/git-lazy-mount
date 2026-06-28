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
use std::time::{Duration, Instant};

use glm_core::{Error, ErrorCode, ObjectId, Result};
use glm_git_store::GitStore;

/// Cap on reading a `.gitattributes` blob during the FSMonitor seed, so a slow or
/// throttled promisor fetch cannot stall the mount. On timeout the seed is skipped
/// and the first status falls back to the eager scan.
const SEED_ATTR_READ_TIMEOUT_SECS: u64 = 20;
/// Parallel workers used to warm tracked `.gitattributes` blobs before the seed
/// reads them. Under a `tree:0` clone each blob is a separate promisor round-trip,
/// so a repo with many `.gitattributes` (e.g. kubernetes' 15) serializes into
/// seconds on the mount-return path; faulting them concurrently collapses that to
/// roughly a single round-trip.
const SEED_ATTR_PREFETCH_WORKERS: usize = 8;

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
            // `tree:0` fetches every commit (so history, merge-base, and branch
            // switching work) but no trees or blobs. `build_index` lazily faults the
            // HEAD tree hierarchy; blobs hydrate on read. A full-history `blob:none`
            // clone would instead download every tree from all of history (slow and
            // large on big repos), and `--depth 1` would graft the commits (breaking
            // `git merge`/`git rebase`). `tree:0` avoids both.
            filter: Some("tree:0".into()),
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
            // `--depth` implies `--single-branch`, which fetches only the default
            // branch's tip, so `git switch <other-branch>` would fail with "invalid
            // reference". Override it: fetch every branch's tip (still shallow and
            // `blob:none`, so cheap) so branch switching works in the mount.
            cmd.arg("--no-single-branch");
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
        // Index-format settings for a large working tree (set before `build_index`
        // so the index it writes already uses them): the compact v4 path-prefix
        // encoding and `skipHash` (omit the trailing index checksum) shrink the
        // index and speed every later rewrite — the bulk of `git add -A`'s cost
        // once the directory walk is cheap.
        store.set_config("index.version", "4")?;
        store.set_config("index.skipHash", "true")?;
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
    /// fetching any blobs**. This is `git read-tree HEAD`: O(tracked paths), reads
    /// tree objects only, touches no working-tree files. The real index is then the
    /// single stage that stock `git add`/`status`/`commit` operate on.
    ///
    /// Under the default `--filter=tree:0` clone the HEAD tree hierarchy is absent,
    /// so this faults it (a one-time, bounded cost: the trees of HEAD, not of all
    /// history). Lazy fetch is therefore **permitted** here, unlike [`Self::git`].
    /// It still fetches no blobs (those hydrate on read).
    pub fn build_index(&self) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(&self.gitdir);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
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

    /// Fault every tracked `.gitattributes` blob concurrently (best-effort). The
    /// FSMonitor seed reads these serially; under a `tree:0` clone each read is a
    /// promisor round-trip, which on a repo with many `.gitattributes` (kubernetes:
    /// 15 files, ~10s serial) is the dominant mount-return cost. Warming them in
    /// parallel collapses that to roughly one round-trip. Errors are ignored — the
    /// serial reads that follow surface any genuine failure.
    fn prefetch_attr_blobs(&self, all: &[Vec<u8>]) {
        let attrs: Vec<&[u8]> = all
            .iter()
            .filter(|p| p.rsplit(|&b| b == b'/').next() == Some(b".gitattributes".as_slice()))
            .map(|p| p.as_slice())
            .collect();
        if attrs.len() < 2 {
            return;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let workers = attrs.len().min(SEED_ATTR_PREFETCH_WORKERS);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&p) = attrs.get(i) else { break };
                    let _ = self.read_index_blob(p);
                });
            }
        });
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
        // The seed then reads every tracked `.gitattributes` blob serially (in
        // `declares_conversion_attributes`, then `conversion_paths`). Under a
        // `tree:0` clone each is a promisor round-trip, so on a repo with many of
        // them that serial chain dominates the mount-return path. Warm them all
        // concurrently first so the reads below hit local objects.
        self.prefetch_attr_blobs(&all);
        // A checkout conversion (clean/smudge `filter`, `ident`,
        // `working-tree-encoding`, or CRLF `eol`) makes a file's working-tree bytes
        // differ from its stored blob, so seeding it valid could hide a real diff.
        // Carve those paths out *per file* and seed the rest. A handful of converted
        // files (e.g. a few `eol=crlf` entries in `.gitattributes`) must not cost the
        // whole index its seed — otherwise the first `git status` falls back to
        // stat-ing every tracked path, and on a lazy mount that size-faults every
        // blob (tens of thousands of round-trips), which can stall the mount.
        let converted = self.conversion_paths(&all)?;
        let to_seed: Vec<&Vec<u8>> = all.iter().filter(|p| !converted.contains(*p)).collect();
        if to_seed.is_empty() {
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
            for &p in &to_seed {
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

    /// Whether the repo declares any checkout-conversion attribute (clean/smudge
    /// `filter`, `ident`, `working-tree-encoding`, or CRLF `eol`) in a tracked
    /// `.gitattributes`.
    ///
    /// Reads the `.gitattributes` blobs directly (a few small objects) rather than
    /// running `check-attr` over every path: on a huge repo that is needlessly
    /// expensive and can stall on the attributes fetch. Each read is bounded by a
    /// timeout, so a slow promisor cannot hang the mount; a read failure or timeout
    /// errs, and the caller falls back to the eager (unseeded) first status.
    fn declares_conversion_attributes(&self, all: &[Vec<u8>]) -> Result<bool> {
        let attrs_files = all
            .iter()
            .filter(|p| p.rsplit(|&b| b == b'/').next() == Some(b".gitattributes".as_slice()));
        for path in attrs_files {
            if attributes_declare_conversion(&self.read_index_blob(path)?) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The exact tracked paths that carry a checkout-conversion attribute
    /// (`filter`, `ident`, `working-tree-encoding`, or `eol=crlf`), so the FSMonitor
    /// seed can carve out only those and seed everything else.
    ///
    /// Fast path: if no tracked `.gitattributes` declares any conversion attribute,
    /// returns an empty set without running `check-attr`. Otherwise it asks Git to
    /// resolve attributes from the index (`--cached`, so the working tree is never
    /// stat'd and no file blob is faulted — only the already-read `.gitattributes`
    /// definitions) over every path at once. The path list is streamed on a writer
    /// thread so a large result cannot deadlock the pipe.
    fn conversion_paths(&self, all: &[Vec<u8>]) -> Result<HashSet<Vec<u8>>> {
        if !self.declares_conversion_attributes(all)? {
            return Ok(HashSet::new());
        }
        // Pre-fetch every tracked `.gitattributes` (each bounded) so check-attr can
        // resolve attributes from local objects only: `self.git()` runs with
        // `GIT_NO_LAZY_FETCH=1`, and check-attr also reads sub-directory
        // `.gitattributes` — not just the root one `declares_conversion_attributes`
        // stopped at — so any it has not seen yet must already be present, or
        // check-attr would fail and skip the seed entirely.
        for path in all
            .iter()
            .filter(|p| p.rsplit(|&b| b == b'/').next() == Some(b".gitattributes".as_slice()))
        {
            let _ = self.read_index_blob(path);
        }
        let mut child = self
            .git()
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
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn check-attr: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new(ErrorCode::Internal, "check-attr stdin".to_string()))?;
        let paths: Vec<Vec<u8>> = all.to_vec();
        let writer = std::thread::spawn(move || {
            for p in &paths {
                if stdin
                    .write_all(p)
                    .and_then(|()| stdin.write_all(&[0]))
                    .is_err()
                {
                    break;
                }
            }
            drop(stdin);
        });
        let out = child
            .wait_with_output()
            .map_err(|e| Error::new(ErrorCode::Internal, format!("check-attr wait: {e}")))?;
        let _ = writer.join();
        if !out.status.success() {
            return Err(Error::new(
                ErrorCode::Internal,
                "check-attr --cached failed".to_string(),
            ));
        }
        // `-z` output is repeated `<path> NUL <attr> NUL <value> NUL` triples.
        let toks: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
        let mut converted = HashSet::new();
        let mut i = 0;
        while i + 2 < toks.len() {
            let (path, attr, val) = (toks[i], toks[i + 1], toks[i + 2]);
            i += 3;
            if path.is_empty() {
                continue;
            }
            let is_conversion = match attr {
                b"eol" => val == b"crlf",
                b"ident" => val == b"set",
                b"filter" | b"working-tree-encoding" => val != b"unspecified" && val != b"unset",
                _ => false,
            };
            if is_conversion {
                converted.insert(path.to_vec());
            }
        }
        Ok(converted)
    }

    /// Read a tracked blob's contents from the index by path (`:<path>`), fetching
    /// it if missing. Bounded by [`SEED_ATTR_READ_TIMEOUT_SECS`] so a slow promisor
    /// fetch cannot stall the caller.
    fn read_index_blob(&self, path: &[u8]) -> Result<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let mut spec = Vec::with_capacity(path.len() + 1);
        spec.push(b':');
        spec.extend_from_slice(path);
        // Lazy fetch is permitted here (no `GIT_NO_LAZY_FETCH`) so a missing
        // `.gitattributes` blob is retrieved, but the timeout bounds it.
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir").arg(&self.gitdir);
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.arg("cat-file")
            .arg("-p")
            .arg(std::ffi::OsStr::from_bytes(&spec));
        let out = output_bounded(cmd, SEED_ATTR_READ_TIMEOUT_SECS)?;
        if !out.status.success() {
            return Err(Error::new(
                ErrorCode::Internal,
                format!(
                    "read {}: {}",
                    String::from_utf8_lossy(path),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(out.stdout)
    }
}

/// Run `cmd` to completion, returning its output, but kill it and err if it runs
/// longer than `secs`. For commands with **small** output only: stdout is drained
/// after the child exits, not concurrently, so a child that fills the pipe would
/// hit the timeout rather than complete.
fn output_bounded(mut cmd: Command, secs: u64) -> Result<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::new(ErrorCode::Internal, format!("spawn: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|e| Error::new(ErrorCode::Internal, format!("wait: {e}")));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new(
                        ErrorCode::Internal,
                        "command timed out".to_string(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(Error::new(ErrorCode::Internal, format!("try_wait: {e}"))),
        }
    }
}

/// Scan `.gitattributes` content for a checkout-conversion attribute: a
/// clean/smudge `filter=`, `ident`, `working-tree-encoding=`, or CRLF `eol=crlf`.
/// `text`/`-text`/`eol=lf`/`linguist-*` are not conversions on a Linux mount.
fn attributes_declare_conversion(content: &[u8]) -> bool {
    for line in content.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        // Tokens after the leading pattern.
        let mut toks = line
            .split(|&b| b == b' ' || b == b'\t')
            .filter(|t| !t.is_empty());
        let _pattern = toks.next();
        for t in toks {
            if t == b"ident"
                || t == b"eol=crlf"
                || t.starts_with(b"filter=")
                || t.starts_with(b"working-tree-encoding=")
            {
                return true;
            }
        }
    }
    false
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

        // HEAD resolves straight from the clone; the baseline trees fault in via
        // build_index (the default tree:0 clone fetches commits but no trees/blobs).
        assert!(repo.head_commit().unwrap().is_some(), "HEAD resolves");
        repo.build_index().unwrap();
        let tree = repo.head_tree().unwrap().expect("baseline tree");
        // The baseline tree is readable without a physical checkout (trees fault
        // in, blobs do not).
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
