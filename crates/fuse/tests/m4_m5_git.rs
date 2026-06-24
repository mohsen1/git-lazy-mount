//! Branch-changing + remote workflows through the transparent mount, using
//! stock git against the real index/refs. These happy-path flows let git do the
//! work (writing changed files into the overlay); conflict-stage and
//! hydration-budget measurement are tracked separately. Real `/dev/fuse` mount —
//! runs under `--features fuse`.
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
fn branch_switch_commit_merge_and_push_through_the_mount() {
    let remote =
        glm_testkit::seed_remote(&[("README.md", b"base\n"), ("src/main.rs", b"fn main() {}\n")]);
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
    proj.repo().build_index().unwrap();
    let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
    assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");
    git(&mnt, &["config", "user.email", "t@example.com"]);
    git(&mnt, &["config", "user.name", "Test"]);

    // Create a branch, commit a new file on it.
    let (ok_sw, _, e) = git(&mnt, &["switch", "-c", "feature"]);
    assert!(ok_sw, "switch -c failed: {e}");
    std::fs::write(mnt.join("feature.txt"), b"from feature\n").unwrap();
    let (ok_add, _, _) = git(&mnt, &["add", "feature.txt"]);
    assert!(ok_add);
    let (ok_c, _, ce) = git(&mnt, &["commit", "-m", "add feature.txt"]);
    assert!(ok_c, "commit failed: {ce}");

    // switch back to the default branch — the new file must disappear from the
    // working tree (git updates the worktree through the overlay).
    let (ok_back, _, be) = git(&mnt, &["switch", "main"]);
    assert!(ok_back, "switch main failed: {be}");
    assert!(
        !mnt.join("feature.txt").exists(),
        "feature.txt should not exist on main after switch"
    );
    let (_, st, _) = git(&mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "main should be clean after switch, got {st:?}");

    // A conflict-free merge brings the file back.
    let (ok_m, _, me) = git(&mnt, &["merge", "--no-edit", "feature"]);
    assert!(ok_m, "merge failed: {me}");
    assert_eq!(
        std::fs::read_to_string(mnt.join("feature.txt")).unwrap(),
        "from feature\n",
        "merged file content"
    );

    // reset --hard discards an uncommitted edit.
    std::fs::write(mnt.join("README.md"), b"dirty\n").unwrap();
    let (ok_r, _, re) = git(&mnt, &["reset", "--hard", "HEAD"]);
    assert!(ok_r, "reset --hard failed: {re}");
    assert_eq!(
        std::fs::read_to_string(mnt.join("README.md")).unwrap(),
        "base\n",
        "reset --hard restored the baseline content"
    );

    // Push the merge commit to the ordinary remote.
    let (ok_p, _, pe) = git(&mnt, &["push", "origin", "HEAD:main"]);
    assert!(ok_p, "push failed: {pe}");
    // the remote now has the merge commit at its tip
    let new_tip =
        String::from_utf8(glm_testkit::git(&remote.bare_path, &["rev-parse", "main"])).unwrap();
    let (_, local_head, _) = git(&mnt, &["rev-parse", "HEAD"]);
    assert_eq!(new_tip.trim(), local_head, "remote advanced to our commit");

    mount.unmount();
    let _ = &remote;
}
