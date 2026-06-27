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
    }
    drop(repo);

    mount_and_validate(&gitdir, path, &cache, &overlay)
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
    // FSMonitor hook that supplies its invalidation: once the post-mount warm
    // populates it, a repeat `git status`/`git add -A` skips the untracked-
    // directory walk for every unchanged directory. The hook reports which paths
    // changed, so a new untracked file still invalidates exactly its directory.
    set("core.untrackedCache", "true");
    true
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
fn mount_and_validate(gitdir: &Path, mountpoint: &Path, cache: &Path, overlay: &Path) -> R {
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

    // Warm git's untracked cache with a real `git status` over the LIVE mount.
    // The cache cannot be seeded offline (git stamps each cached directory with
    // its live stat_data and re-validates on read; a synthetic seed records
    // stats that don't match the FUSE dirs, so git re-walks). The only thing that
    // writes a trustworthy `UNTR` is a real walk over the mounted directories, so
    // we trigger one — detached and niced so the first walk doesn't steal
    // foreground CPU. Once it completes, with `core.untrackedCache=true` every
    // later `git status` skips the walk. Best-effort: never fails the mount.
    warm_untracked_cache(mountpoint);

    let branch = git_stdout(mountpoint, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    println!(
        "Mounted {} at {} (branch {}). Plain `git` now works here.",
        gitdir.display(),
        mountpoint.display(),
        branch
    );
    Ok(())
}

/// Spawn a detached, low-priority `git status` on the live mount to warm git's
/// untracked cache (see the call site for why it cannot be seeded offline).
/// Prefers `nice -n 19`; falls back to normal priority if `nice` is unavailable.
/// Detached (never waited on) and strictly best-effort — on a spawn error we log
/// one line and return; the mount must never fail because of the warm.
#[cfg(feature = "fuse")]
fn warm_untracked_cache(mountpoint: &Path) {
    use std::process::Stdio;
    let mp = mountpoint.to_string_lossy().into_owned();
    let git_args = [
        "-C",
        &mp,
        "-c",
        "core.untrackedCache=true",
        "status",
        "--porcelain",
    ];
    let mut nice = Command::new("nice");
    nice.args(["-n", "19", "git"])
        .args(git_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if nice.spawn().is_ok() {
        return;
    }
    let spawned = Command::new("git")
        .args(git_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        eprintln!(
            "git-lazy-mount: untracked-cache warm not started ({e}); first `git status` untracked walk will be eager"
        );
    }
}

#[cfg(not(feature = "fuse"))]
fn mount_and_validate(_gitdir: &Path, _mountpoint: &Path, _cache: &Path, _overlay: &Path) -> R {
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
