//! M3 correctness: stock `git status` / `add` / `commit` through the transparent
//! mount, operating the REAL `.git/index` (redesign.md §43 criteria 8/9/11,
//! Experiments C/D/F). Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

fn git(dir: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn git_status_add_commit_through_the_transparent_mount() {
    let remote = glm_testkit::seed_remote(&[
        ("README.md", b"hello\n"),
        ("src/main.rs", b"fn main() {}\n"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    let repo = AdminRepo::clone(
        &remote.url,
        &tmp.path().join("git"),
        &mnt,
        &tmp.path().join("anchor"),
        &CloneOptions::default(),
    )
    .unwrap();
    let proj = Arc::new(
        Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap(),
    );
    // M3: build the real index from the baseline (the single stage).
    proj.repo().build_index().unwrap();

    let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
    assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");

    // identity for commits in this isolated repo
    git(&mnt, &["config", "user.email", "t@example.com"]);
    git(&mnt, &["config", "user.name", "Test"]);

    // A clean tree (possibly eager — git may read each file to verify; that's the
    // §27 'correct but eager' status the FSMonitor work will make lazy).
    let (ok, clean, err) = git(&mnt, &["status", "--porcelain"]);
    assert!(ok, "status failed: {err}");
    assert_eq!(
        clean, "",
        "freshly-mounted tree should be clean, got: {clean:?}"
    );

    // Experiment C: a transparent edit is seen by stock `git status`.
    std::fs::write(mnt.join("README.md"), b"edited via the mount\n").unwrap();
    let (_, st, _) = git(&mnt, &["status", "--porcelain"]);
    assert!(
        st.lines()
            .any(|l| l.ends_with("README.md") && l.contains('M')),
        "edit not reported as modified: {st:?}"
    );

    // Experiment D: `git add` stages it in the real index.
    let (ok_add, _, e) = git(&mnt, &["add", "README.md"]);
    assert!(ok_add, "add failed: {e}");
    let (_, cached, _) = git(&mnt, &["diff", "--cached", "--name-only"]);
    assert!(cached.contains("README.md"), "not staged: {cached:?}");

    // Experiment F: `git commit` advances the branch directly (no adoption).
    let (ok_commit, _, ce) = git(&mnt, &["commit", "-m", "edit through the mount"]);
    assert!(ok_commit, "commit failed: {ce}");
    let (_, log, _) = git(&mnt, &["log", "--oneline", "-1"]);
    assert!(log.contains("edit through the mount"), "log: {log:?}");

    // The committed blob has the new content (it really went into the object store).
    let (_, shown, _) = git(&mnt, &["show", "HEAD:README.md"]);
    assert_eq!(shown, "edited via the mount");

    // And the tree is clean again after the commit.
    let (_, after, _) = git(&mnt, &["status", "--porcelain"]);
    assert_eq!(
        after, "",
        "tree should be clean post-commit, got: {after:?}"
    );

    mount.unmount();
    let _ = remote;
}
