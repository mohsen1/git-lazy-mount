//! The transparent `git-lazy-mount` executable.
//!
//! The primary form replaces the initial `git clone`:
//!
//! ```text
//! git lazy-mount https://host/huge-repo ~/huge-repo
//! ```
//!
//! It clones a partial repo, mounts a transparent virtual working tree, validates
//! it, and **returns** — after which plain stock `git` (and editors, builds) work
//! with no wrapper, no aliases, no `git lazy-mount` workflow verbs. The only
//! `git lazy-mount` subcommands are lifecycle/diagnostics (`unmount`, `doctor`).
//! There is deliberately **no** `add`/`commit`/`switch`/`push`/`git --` (their
//! presence would mean transparency failed).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

type R = Result<(), String>;

#[derive(Parser)]
#[command(
    name = "git-lazy-mount",
    version,
    about = "Transparent, lazily hydrated Git working trees"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Verb>,

    /// Repository URL (primary form: `git lazy-mount <url> <path>`).
    url: Option<String>,
    /// Mountpoint for the working tree.
    path: Option<PathBuf>,

    /// Branch to attach to (default: the remote's default).
    #[arg(long)]
    branch: Option<String>,
    /// Shallow clone depth. Off by default: the `tree:0` filter already makes a
    /// full-history clone cheap, and a shallow clone grafts commits, which breaks
    /// `git merge`/`git rebase` and hides history.
    #[arg(long)]
    depth: Option<u32>,
    /// Partial-clone filter (default: `tree:0`, full commit history with trees and
    /// blobs fetched lazily).
    #[arg(long)]
    filter: Option<String>,
    /// Permit a full-object clone if the remote rejects the filter.
    #[arg(long)]
    allow_full_object_clone: bool,
    /// Run a synchronous `git status` before returning to pre-warm Git's
    /// untracked cache. This makes the first broad `git status`/`git add -A`
    /// cheaper, but it can add tens of seconds to mount startup on large repos.
    #[arg(long)]
    warm_status: bool,
}

#[derive(Subcommand)]
enum Verb {
    /// Unmount a lazy mount.
    Unmount {
        /// The mountpoint.
        path: PathBuf,
    },
    /// Diagnostics for a mount.
    Doctor {
        /// The mountpoint.
        path: PathBuf,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Serve a mount in the foreground. Internal — spawned by the mount flow to
    /// hold the kernel mount after the parent command returns.
    #[command(name = "__serve", hide = true)]
    Serve {
        #[arg(long)]
        gitdir: PathBuf,
        #[arg(long)]
        mountpoint: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        overlay: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => match (cli.url.clone(), cli.path.clone()) {
            (Some(url), Some(path)) => cmd_mount(&cli, &url, &path),
            _ => {
                Err("usage: git lazy-mount <url> <path>  (or a subcommand: unmount, doctor)".into())
            }
        },
        Some(Verb::Unmount { path }) => cmd_unmount(&path),
        Some(Verb::Doctor { path, json }) => cmd_doctor(&path, json),
        Some(Verb::Serve {
            gitdir,
            mountpoint,
            cache,
            overlay,
        }) => cmd_serve(&gitdir, &mountpoint, &cache, &overlay),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("git-lazy-mount: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Deterministic per-mountpoint workspace layout. Both the
/// parent (clone) and the detached serve child derive the same paths.
fn workspace_paths(mountpoint: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let abs = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    let mut h = DefaultHasher::new();
    abs.hash(&mut h);
    let id = format!("{:016x}", h.finish());
    let base = data_dir().join("workspaces").join(id);
    (
        base.join("git"),
        base.join("cache"),
        base.join("overlay"),
        base.join("anchor"),
    )
}

fn data_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("git-lazy-mount");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/git-lazy-mount")
}

fn cmd_mount(cli: &Cli, url: &str, path: &Path) -> R {
    // Preflight: the mountpoint must exist (or be creatable) and empty.
    std::fs::create_dir_all(path).map_err(|e| format!("create mountpoint: {e}"))?;
    if std::fs::read_dir(path)
        .map_err(|e| format!("read mountpoint: {e}"))?
        .next()
        .is_some()
    {
        return Err(format!("mountpoint {} is not empty", path.display()));
    }
    let (gitdir, cache, overlay, anchor) = workspace_paths(path);
    if let Some(parent) = gitdir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create workspace dir: {e}"))?;
    }

    // Clone + build the real index (no checkout, no blob fetches). The default
    // `tree:0` filter fetches every commit (full history, so `git merge`/`rebase`/
    // `log` and branch switching all work) but no trees or blobs; `build_index`
    // faults the HEAD trees, blobs hydrate on read. This is both correct and cheap,
    // unlike a shallow clone (grafts commits, breaks merge) or a full-history
    // `blob:none` clone (downloads every tree from all of history).
    let opts = glm_git_repo::CloneOptions {
        branch: cli.branch.clone(),
        depth: cli.depth,
        filter: cli.filter.clone().or_else(|| Some("tree:0".into())),
        allow_full_object_clone: cli.allow_full_object_clone,
    };
    let repo = glm_git_repo::AdminRepo::clone(url, &gitdir, path, &anchor, &opts)
        .map_err(|e| format!("clone: {e}"))?;
    repo.build_index()
        .map_err(|e| format!("build index: {e}"))?;
    // Configure the FSMonitor hook so git learns what changed from the daemon's
    // journal instead of re-statting the whole tree. Best-effort: if the hook
    // binary isn't found, git status still works (just eager).
    if configure_fsmonitor(&gitdir) {
        // Seed the FSMonitor extension so the FIRST `git status`/`git diff` faults
        // zero blobs, not just later ones. Without this, the first status has no
        // extension to trust and stats (faults) every entry before writing it.
        seed_first_status(&gitdir, &repo);
        if let Err(e) = install_commit_acceleration_hooks(&gitdir) {
            eprintln!(
                "git-lazy-mount: commit acceleration hooks skipped ({e}); first commit may be eager"
            );
        }
    }
    drop(repo);

    mount_and_validate(&gitdir, path, &cache, &overlay, cli.warm_status)
}

/// Point `core.fsmonitor` at the `git-lazy-mount-fsmonitor` hook installed
/// alongside this binary, and select hook protocol v2. Returns `true` if the hook
/// was found and configured.
fn configure_fsmonitor(gitdir: &Path) -> bool {
    let hook = match std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("git-lazy-mount-fsmonitor")))
    {
        Some(h) if h.exists() => h,
        _ => return false,
    };
    let Some(hook) = hook.to_str() else {
        return false;
    };
    let set = |key: &str, val: &str| {
        let _ = Command::new("git")
            .arg("--git-dir")
            .arg(gitdir)
            .args(["config", key, val])
            .status();
    };
    set("core.fsmonitor", hook);
    set("core.fsmonitorHookVersion", "2");
    // Enable the untracked cache (the `UNTR` index extension), gated behind the
    // FSMonitor hook that supplies its invalidation. By default the first broad
    // `git status`/`git add -A` populates it on demand; `--warm-status` pre-pays
    // that walk before returning from mount. The hook reports which paths changed,
    // so a new untracked file still invalidates exactly its directory.
    set("core.untrackedCache", "true");
    true
}

const GLM_HOOK_MARKER: &str = "# git-lazy-mount managed hook";

const POST_INDEX_CHANGE_HOOK: &str = r#"#!/usr/bin/env bash
# git-lazy-mount managed hook
set -euo pipefail

is_git_add=0
if [ -r "/proc/$PPID/cmdline" ]; then
  seen_git=0
  skip_next=0
  while IFS= read -r -d "" arg; do
    base=${arg##*/}
    if [ "$seen_git" = 0 ]; then
      [ "$base" = "git" ] && seen_git=1
      continue
    fi
    if [ "$skip_next" = 1 ]; then
      skip_next=0
      continue
    fi
    case "$arg" in
      -C|-c|--git-dir|--work-tree|--namespace|--super-prefix|--config-env)
        skip_next=1
        continue
        ;;
      --git-dir=*|--work-tree=*|--namespace=*|--super-prefix=*|--config-env=*)
        continue
        ;;
      --literal-pathspecs|--no-optional-locks|--no-pager|--paginate|--bare)
        continue
        ;;
      --*)
        continue
        ;;
      -*)
        continue
        ;;
    esac
    if [ "$arg" = "add" ]; then
      is_git_add=1
    fi
    break
  done < "/proc/$PPID/cmdline"
else
  cmd=$(ps -o args= -p "$PPID" 2>/dev/null || true)
  seen_git=0
  skip_next=0
  for arg in $cmd; do
    base=${arg##*/}
    if [ "$seen_git" = 0 ]; then
      [ "$base" = "git" ] && seen_git=1
      continue
    fi
    if [ "$skip_next" = 1 ]; then
      skip_next=0
      continue
    fi
    case "$arg" in
      -C|-c|--git-dir|--work-tree|--namespace|--super-prefix|--config-env)
        skip_next=1
        continue
        ;;
      --git-dir=*|--work-tree=*|--namespace=*|--super-prefix=*|--config-env=*)
        continue
        ;;
      --literal-pathspecs|--no-optional-locks|--no-pager|--paginate|--bare)
        continue
        ;;
      --*)
        continue
        ;;
      -*)
        continue
        ;;
    esac
    if [ "$arg" = "add" ]; then
      is_git_add=1
    fi
    break
  done
fi

[ "$is_git_add" = 1 ] || exit 0

gitdir=$(git rev-parse --git-dir)
guard="$gitdir/glm-skipworktree-hook.guard"
active="$gitdir/glm-skipworktree-active"
[ ! -e "$guard" ] || exit 0
: > "$guard"
trap 'rm -f "$guard"' EXIT

git ls-files -z | git update-index --skip-worktree -z --stdin
: > "$active"
"#;

const COMMIT_MSG_HOOK: &str = r#"#!/usr/bin/env bash
# git-lazy-mount managed hook
set -euo pipefail

gitdir=$(git rev-parse --git-dir)
guard="$gitdir/glm-skipworktree-hook.guard"
active="$gitdir/glm-skipworktree-active"
paths="$gitdir/glm-skipworktree-paths.z"
[ -e "$active" ] || exit 0
: > "$guard"
trap 'rm -f "$guard" "$paths"' EXIT

git ls-files -z > "$paths"
git update-index --no-skip-worktree -z --stdin < "$paths"
git update-index --fsmonitor-valid -z --stdin < "$paths"
rm -f "$active"
"#;

const POST_COMMIT_HOOK: &str = COMMIT_MSG_HOOK;

/// Install managed hooks that make the common `git add ... && git commit` path
/// cheap on a `tree:0` partial clone. Git's commit implementation bulk-fetches
/// missing promisor blobs unless index entries are `CE_SKIP_WORKTREE`; the
/// post-index hook marks entries skipped immediately after `git add`, records an
/// active sentinel, and the commit-msg hook clears the bits after Git has built
/// the commit tree, then restores FSMonitor-valid bits so the next status stays
/// lazy. The post-commit copy is a cleanup fallback for `git commit --no-verify`,
/// which skips commit-msg.
fn install_commit_acceleration_hooks(gitdir: &Path) -> R {
    let hooks = gitdir.join("hooks");
    std::fs::create_dir_all(&hooks).map_err(|e| format!("create hooks dir: {e}"))?;
    write_managed_hook(&hooks.join("post-index-change"), POST_INDEX_CHANGE_HOOK)?;
    write_managed_hook(&hooks.join("commit-msg"), COMMIT_MSG_HOOK)?;
    write_managed_hook(&hooks.join("post-commit"), POST_COMMIT_HOOK)?;
    Ok(())
}

fn write_managed_hook(path: &Path, content: &str) -> R {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if !existing.contains(GLM_HOOK_MARKER) {
            return Err(format!(
                "{} already exists and is not managed",
                path.display()
            ));
        }
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Pre-seed the FSMonitor extension so the first `git status`/`git diff` faults
/// zero blobs. The hook answers the seq-0 bootstrap query (no changes) only when
/// the journal exists, so create the empty journal first; the serve daemon reopens
/// the same log. Best-effort: any failure just leaves the first status eager.
fn seed_first_status(gitdir: &Path, repo: &glm_git_repo::AdminRepo) {
    use glm_worktree::journal::{journal_dir, workspace_id, ChangeJournal};
    if ChangeJournal::open(journal_dir(gitdir), workspace_id(gitdir), 1, 0).is_err() {
        return;
    }
    if let Err(e) = repo.seed_fsmonitor_valid() {
        eprintln!("git-lazy-mount: fsmonitor seed skipped ({e}); first status will be eager");
    }
}

#[cfg(feature = "fuse")]
fn mount_and_validate(
    gitdir: &Path,
    mountpoint: &Path,
    cache: &Path,
    overlay: &Path,
    warm_status: bool,
) -> R {
    // Spawn a detached serve child that holds the kernel mount. When this command
    // returns, the child is reparented to init and keeps serving. We do NOT
    // wait on it.
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Command::new(exe)
        .arg("__serve")
        .arg("--gitdir")
        .arg(gitdir)
        .arg("--mountpoint")
        .arg(mountpoint)
        .arg("--cache")
        .arg(cache)
        .arg("--overlay")
        .arg(overlay)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn serve: {e}"))?;

    // Wait for readiness: the synthetic `.git` appears once the mount serves.
    let gitfile = mountpoint.join(".git");
    let mut ready = false;
    for _ in 0..1000 {
        if gitfile.exists() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !ready {
        return Err("mount did not become ready".into());
    }

    // Health checks: stock git must discover the repo.
    let top = git_stdout(mountpoint, &["rev-parse", "--show-toplevel"])?;
    let want = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    if Path::new(&top).canonicalize().ok() != Some(want) {
        return Err(format!("health check failed: show-toplevel = {top}"));
    }
    if git_stdout(mountpoint, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return Err("health check failed: not inside work tree".into());
    }

    // Optionally establish git's untracked cache with a real `git status` over the
    // LIVE mount. The cache cannot be seeded offline (git stamps each cached
    // directory with its live stat_data and re-validates on read; a synthetic seed
    // records stats that don't match the FUSE dirs, so git re-walks). The only
    // thing that writes a trustworthy `UNTR` is a real walk over the mounted
    // directories.
    //
    // This is opt-in because on large repos the walk can dominate startup, while
    // many agent workflows stage exact paths and never need a pre-warmed untracked
    // cache. Users who expect their first command to be broad `git status` or
    // `git add -A` can pass `--warm-status` to pre-pay the walk before return.
    // Bounded by `ESTABLISH_TIMEOUT`; on overrun it is left to finish detached.
    if warm_status {
        establish_untracked_cache(mountpoint);
    }

    let branch = git_stdout(mountpoint, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    println!(
        "Mounted {} at {} (branch {}). Plain `git` now works here.",
        gitdir.display(),
        mountpoint.display(),
        branch
    );
    Ok(())
}

/// Wall-clock cap on the synchronous untracked-cache establish. On a normal repo
/// the walk returns in seconds; past this we stop waiting and let it finish
/// detached so a pathological repo can't stall the mount return indefinitely.
#[cfg(feature = "fuse")]
const ESTABLISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run a real `git status` over the live mount to establish git's untracked cache,
/// **synchronously** (bounded by [`ESTABLISH_TIMEOUT`]) so the user's first
/// `git status`/`git commit` consumes a populated `UNTR` instead of paying the
/// walk and racing it (see the call site). Not niced and not detached: there is no
/// foreground agent yet to protect, and the binding constraints are the index lock
/// and the FUSE fault queue, not CPU. Strictly best-effort — a spawn/exit error
/// only logs one line; the mount must never fail because of it.
#[cfg(feature = "fuse")]
fn establish_untracked_cache(mountpoint: &Path) {
    use std::process::Stdio;
    let mp = mountpoint.to_string_lossy().into_owned();
    let mut child = match Command::new("git")
        .args([
            "-C",
            &mp,
            "-c",
            "core.untrackedCache=true",
            "status",
            "--porcelain",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "git-lazy-mount: untracked-cache establish not started ({e}); first `git status` walk will be eager"
            );
            return;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return, // established before return
            Ok(None) if start.elapsed() > ESTABLISH_TIMEOUT => return, // left detached
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => return,
        }
    }
}

#[cfg(not(feature = "fuse"))]
fn mount_and_validate(
    _gitdir: &Path,
    _mountpoint: &Path,
    _cache: &Path,
    _overlay: &Path,
    _warm_status: bool,
) -> R {
    Err("this build has no FUSE mount support (rebuild with --features fuse on Linux)".into())
}

#[cfg(feature = "fuse")]
fn cmd_serve(gitdir: &Path, mountpoint: &Path, cache: &Path, overlay: &Path) -> R {
    let repo =
        glm_git_repo::AdminRepo::open(gitdir, mountpoint).map_err(|e| format!("open: {e}"))?;
    // The FSMonitor change journal: every worktree mutation is recorded so
    // the hook answers `git status` without statting the whole tree.
    let journal = glm_worktree::journal::ChangeJournal::open(
        glm_worktree::journal::journal_dir(gitdir),
        glm_worktree::journal::workspace_id(gitdir),
        1,
        0,
    )
    .map_err(|e| format!("journal: {e}"))?;
    let proj = std::sync::Arc::new(
        glm_worktree::Projection::open(repo, cache.to_path_buf(), overlay.to_path_buf())
            .map_err(|e| format!("projection: {e}"))?
            .with_journal(journal),
    );
    // Blocks until the mount is unmounted.
    glm_fuse::mount(proj, mountpoint).map_err(|e| format!("mount: {e}"))
}

#[cfg(not(feature = "fuse"))]
fn cmd_serve(_g: &Path, _m: &Path, _c: &Path, _o: &Path) -> R {
    Err("no FUSE mount support compiled in".into())
}

fn cmd_unmount(path: &Path) -> R {
    // fusermount3 -u releases the kernel mount; the serving child's `mount()`
    // then returns and it exits.
    for tool in [["fusermount3", "-u"], ["fusermount", "-u"]] {
        let ok = Command::new(tool[0])
            .arg(tool[1])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("unmounted {}", path.display());
            return Ok(());
        }
    }
    let ok = Command::new("umount")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        println!("unmounted {}", path.display());
        Ok(())
    } else {
        Err(format!("could not unmount {}", path.display()))
    }
}

fn cmd_doctor(path: &Path, json: bool) -> R {
    let mounted = path.join(".git").exists();
    let toplevel = git_stdout(path, &["rev-parse", "--show-toplevel"]).ok();
    if json {
        let v = serde_json::json!({
            "mountpoint": path.display().to_string(),
            "mounted": mounted,
            "show_toplevel": toplevel,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("mountpoint: {}", path.display());
        println!("mounted:    {mounted}");
        println!("toplevel:   {}", toplevel.unwrap_or_else(|| "<n/a>".into()));
    }
    Ok(())
}

fn git_stdout(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_status_is_opt_in() {
        let cli = Cli::try_parse_from(["git-lazy-mount", "https://example.com/repo", "/tmp/repo"])
            .unwrap();
        assert!(!cli.warm_status);

        let cli = Cli::try_parse_from([
            "git-lazy-mount",
            "--warm-status",
            "https://example.com/repo",
            "/tmp/repo",
        ])
        .unwrap();
        assert!(cli.warm_status);
    }

    #[test]
    fn managed_commit_hooks_install_and_preserve_custom_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join("git");
        install_commit_acceleration_hooks(&gitdir).unwrap();

        let post_index = gitdir.join("hooks/post-index-change");
        let commit_msg = gitdir.join("hooks/commit-msg");
        let post_commit = gitdir.join("hooks/post-commit");
        let post_index_text = std::fs::read_to_string(&post_index).unwrap();
        let commit_msg_text = std::fs::read_to_string(&commit_msg).unwrap();
        let post_commit_text = std::fs::read_to_string(&post_commit).unwrap();
        assert!(post_index_text.contains(GLM_HOOK_MARKER));
        assert!(commit_msg_text.contains("--fsmonitor-valid"));
        assert!(post_commit_text.contains("--fsmonitor-valid"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&post_index).unwrap().permissions().mode() & 0o111,
                0o111
            );
            assert_eq!(
                std::fs::metadata(&commit_msg).unwrap().permissions().mode() & 0o111,
                0o111
            );
            assert_eq!(
                std::fs::metadata(&post_commit)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0o111
            );
        }

        install_commit_acceleration_hooks(&gitdir).unwrap();
        std::fs::write(&commit_msg, "#!/bin/sh\necho custom\n").unwrap();
        let err = install_commit_acceleration_hooks(&gitdir).unwrap_err();
        assert!(err.contains("already exists and is not managed"));
    }
}
