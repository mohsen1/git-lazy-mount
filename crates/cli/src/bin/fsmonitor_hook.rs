//! `git-lazy-mount-fsmonitor` — the `core.fsmonitor` v2 hook (design.md §12).
//!
//! Git invokes it as `<hook> <version> <prev-token>`; it must print
//! `<new-token>\0<path>\0…` to stdout (or `<token>\0/\0` for a full rescan). It
//! reads the durable change journal the serve daemon writes (no IPC, no worktree
//! scan) and is **fail-safe**: any error prints a full invalidation, which is
//! always correct (just eager). Because every worktree mutation is recorded
//! synchronously before its FUSE reply, the journal the hook reads always
//! reflects every acknowledged change — no false negatives.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use glm_worktree::journal::{journal_dir, workspace_id, ChangeJournal};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let version = args.get(1).map(String::as_str).unwrap_or("");
    let prev = args.get(2).cloned().unwrap_or_default();
    if version != "2" {
        full_invalidation();
        return ExitCode::SUCCESS;
    }
    match query(&prev) {
        Ok(reply) => {
            let _ = std::io::stdout().write_all(&reply);
        }
        Err(()) => full_invalidation(),
    }
    // Exit 0 always: git treats a nonzero exit as "rescan everything" anyway, and
    // we never want a garbage stdout paired with a nonzero status.
    ExitCode::SUCCESS
}

fn query(prev: &str) -> Result<Vec<u8>, ()> {
    let gitdir = resolve_gitdir().ok_or(())?;
    let dir = journal_dir(&gitdir);
    if !dir.join("changes.log").exists() {
        return Err(()); // no journal yet → full invalidation
    }
    let j = ChangeJournal::open(&dir, workspace_id(&gitdir), 1, 0).map_err(|_| ())?;
    Ok(j.query(prev).encode())
}

/// The admin gitdir. Git runs this hook with the worktree top as cwd, so the
/// synthetic `.git` gitfile (`gitdir: <abs admin dir>`) is the primary, most
/// *consistent* source — it holds the exact path the CLI wrote, which the serve
/// daemon also uses, so the workspace id + journal dir always agree across calls.
/// `GIT_DIR` and `rev-parse` are fallbacks.
fn resolve_gitdir() -> Option<PathBuf> {
    if let Ok(s) = std::fs::read_to_string(".git") {
        if let Some(rest) = s.trim().strip_prefix("gitdir:") {
            let p = PathBuf::from(rest.trim());
            if p.exists() {
                return Some(p);
            }
        }
    }
    if let Some(d) = std::env::var_os("GIT_DIR") {
        let p = PathBuf::from(d);
        if p.exists() {
            return Some(p);
        }
    }
    let out = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(PathBuf::from(s.trim()))
}

fn full_invalidation() {
    // `<token>\0/\0`. The token is opaque to git; with no journal we emit a
    // sentinel that the journal will refuse to place next time → another full
    // invalidation (safe, never a false negative).
    let _ = std::io::stdout().write_all(b"glm1::0:0:0\0/\0");
}
