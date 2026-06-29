//! The local-edits overlay.
//!
//! Remote results reflect the *committed* tree. To make `sgrep` reflect the
//! *working* tree, we grep locally-changed files on disk (they're already
//! materialized because you edited them) and drop their now-stale remote hits.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;

use crate::provider::Match;

/// The git toplevel containing `start`, if any.
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| PathBuf::from(t))
}

/// Infer `OWNER/REPO` from the `origin` remote URL.
pub fn infer_repo(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_github_slug(String::from_utf8(out.stdout).ok()?.trim())
}

/// Pull `OWNER/REPO` (exactly the first two path segments) out of a GitHub
/// remote URL (ssh or https), ignoring any trailing `/tree/...` etc.
pub fn parse_github_slug(url: &str) -> Option<String> {
    let after = url.split("github.com").nth(1)?;
    let after = after.trim_start_matches([':', '/']);
    let after = after.strip_suffix(".git").unwrap_or(after);
    let mut segs = after.split('/').filter(|s| !s.is_empty());
    let owner = segs.next()?;
    let repo = segs.next()?;
    Some(format!("{owner}/{repo}"))
}

/// On a git-lazy-mount mount, the paths mutated through the mount — read cheaply
/// from the durable change journal (`<gitdir>/glm-fsmonitor/changes.log`, a
/// NUL-separated append log the serve daemon writes). This is **zero blob
/// faults**, unlike [`locally_changed`] which on a cold mount materializes every
/// file. Returns `None` for a normal repo or when there's no journal.
pub fn glm_changed(root: &Path) -> Option<Vec<String>> {
    // The mount's `.git` is a gitfile: `gitdir: <admin gitdir>`.
    let gitfile = std::fs::read_to_string(root.join(".git")).ok()?;
    let raw = gitfile
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))?
        .trim();
    let gitdir = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let bytes = std::fs::read(gitdir.join("glm-fsmonitor").join("changes.log")).ok()?;
    let mut paths: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    Some(paths)
}

/// Files changed vs the index/HEAD (modified, added, untracked, deleted) per
/// `git status --porcelain`.
///
/// On a *cold* lazy mount this faults every blob (the first-status R6 cost), so
/// prefer passing the changed set explicitly when materialization matters.
pub fn locally_changed(root: &Path) -> Vec<String> {
    // `-z` gives NUL-terminated, *unquoted* records, so non-ASCII names and names
    // with spaces/control chars come through verbatim (no C-quoting to decode).
    let out = match Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "-z", "--no-renames"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        // Each record is `XY <path>`: 2-char status + a space, then the path.
        .filter(|rec| rec.len() > 3)
        .map(|rec| rec[3..].to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Drop remote hits for `changed` paths, grep those paths locally, and merge.
/// Preserves provider result order for unchanged files; callers may apply a
/// stable usefulness ranking after this.
pub fn apply(remote: Vec<Match>, base: &Path, changed: &[String], re: &Regex) -> Vec<Match> {
    let changed_set: HashSet<&str> = changed.iter().map(String::as_str).collect();
    let mut out: Vec<Match> = remote
        .into_iter()
        .filter(|m| !changed_set.contains(m.path.as_str()))
        .collect();
    out.extend(grep_local(base, changed, re));
    let mut seen = HashSet::new();
    out.retain(|m| seen.insert((m.path.clone(), m.line, m.text.clone())));
    out
}

/// Grep `paths` (repo-relative) under `base` with `re`, yielding 1-based lines.
/// Missing paths (deletions) are skipped; file bytes are decoded lossily so a
/// stray invalid byte doesn't drop a whole file. Absolute or parent-escaping
/// paths are refused (the overlay only searches inside the repo).
pub fn grep_local(base: &Path, paths: &[String], re: &Regex) -> Vec<Match> {
    let mut out = Vec::new();
    for p in paths {
        if escapes_base(p) {
            continue;
        }
        let bytes = match std::fs::read(base.join(p)) {
            Ok(b) => b,
            Err(_) => continue,
        };
        for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if re.is_match(line) {
                out.push(Match {
                    path: p.clone(),
                    line: i as u64 + 1,
                    text: line.to_string(),
                });
            }
        }
    }
    out
}

/// Whether a repo-relative path would read outside the repo (absolute, or with a
/// `..` component).
fn escapes_base(p: &str) -> bool {
    let path = Path::new(p);
    path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}
