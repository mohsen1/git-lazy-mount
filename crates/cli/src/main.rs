//! The transparent `git-lazy-mount` executable (redesign.md §1, §10, §43).
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
//! presence would mean transparency failed, §1).

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
    /// Shallow clone depth.
    #[arg(long)]
    depth: Option<u32>,
    /// Partial-clone filter (default: blob:none).
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

/// Deterministic per-mountpoint workspace layout (redesign.md §6). Both the
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
    // Preflight (§10.1): the mountpoint must exist (or be creatable) and empty.
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

    // Clone + build the real index (no checkout, no blob fetches).
    let opts = glm_git_repo::CloneOptions {
        branch: cli.branch.clone(),
        depth: cli.depth,
        filter: cli.filter.clone().or_else(|| Some("blob:none".into())),
        allow_full_object_clone: cli.allow_full_object_clone,
    };
    let repo = glm_git_repo::AdminRepo::clone(url, &gitdir, path, &anchor, &opts)
        .map_err(|e| format!("clone: {e}"))?;
    repo.build_index()
        .map_err(|e| format!("build index: {e}"))?;
    drop(repo);

    mount_and_validate(&gitdir, path, &cache, &overlay)
}

#[cfg(feature = "fuse")]
fn mount_and_validate(gitdir: &Path, mountpoint: &Path, cache: &Path, overlay: &Path) -> R {
    // Spawn a detached serve child that holds the kernel mount. When this command
    // returns, the child is reparented to init and keeps serving (§9). We do NOT
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

    // Health checks (§10.6): stock git must discover the repo.
    let top = git_stdout(mountpoint, &["rev-parse", "--show-toplevel"])?;
    let want = std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
    if Path::new(&top).canonicalize().ok() != Some(want) {
        return Err(format!("health check failed: show-toplevel = {top}"));
    }
    if git_stdout(mountpoint, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return Err("health check failed: not inside work tree".into());
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

#[cfg(not(feature = "fuse"))]
fn mount_and_validate(_gitdir: &Path, _mountpoint: &Path, _cache: &Path, _overlay: &Path) -> R {
    Err("this build has no FUSE mount support (rebuild with --features fuse on Linux)".into())
}

#[cfg(feature = "fuse")]
fn cmd_serve(gitdir: &Path, mountpoint: &Path, cache: &Path, overlay: &Path) -> R {
    let repo =
        glm_git_repo::AdminRepo::open(gitdir, mountpoint).map_err(|e| format!("open: {e}"))?;
    let proj = std::sync::Arc::new(
        glm_worktree::Projection::open(repo, cache.to_path_buf(), overlay.to_path_buf())
            .map_err(|e| format!("projection: {e}"))?,
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
