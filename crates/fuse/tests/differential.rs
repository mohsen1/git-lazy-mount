//! Differential test against a normal checkout (redesign.md §40.1) — the
//! redesign's primary correctness strategy. The same sequence of git operations
//! applied to (a) a conventional full `git checkout` and (b) the transparent
//! lazy mount of the same commit must yield identical results: the same HEAD
//! tree, the same committed bytes, the same status. Real `/dev/fuse` mount.
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut full = vec![
        "-c",
        "protocol.file.allow=always",
        "-C",
        dir.to_str().unwrap(),
    ];
    full.extend_from_slice(args);
    let out = Command::new("git").args(&full).output().expect("git");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
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

/// Apply the identical workflow to a worktree and return (HEAD tree oid,
/// committed README bytes, porcelain status after commit).
fn workflow(dir: &Path) -> (String, String, String) {
    git(dir, &["config", "user.email", "t@e"]);
    git(dir, &["config", "user.name", "t"]);
    // edit an existing file, add a new file, stage, commit.
    std::fs::write(dir.join("README.md"), b"edited body\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).ok();
    std::fs::write(dir.join("src/new.rs"), b"pub fn added() {}\n").unwrap();
    assert!(
        git(dir, &["add", "-A"]).0,
        "add -A failed in {}",
        dir.display()
    );
    assert!(
        git(
            dir,
            &["-c", "commit.gpgsign=false", "commit", "-m", "same change"]
        )
        .0,
        "commit failed in {}",
        dir.display()
    );
    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]).1;
    let readme = git(dir, &["show", "HEAD:README.md"]).1;
    let status = git(dir, &["status", "--porcelain"]).1;
    (tree, readme, status)
}

#[test]
fn lazy_mount_matches_a_normal_checkout() {
    let remote = glm_testkit::seed_remote(&[
        ("README.md", b"base body\n"),
        ("src/main.rs", b"fn main() {}\n"),
        ("docs/guide.md", b"# guide\n"),
    ]);
    let tmp = tempfile::tempdir().unwrap();

    // (a) a conventional full checkout of the remote.
    let normal = tmp.path().join("normal");
    assert!(
        Command::new("git")
            .args([
                "-c",
                "protocol.file.allow=always",
                "clone",
                "--quiet",
                &remote.url
            ])
            .arg(&normal)
            .status()
            .unwrap()
            .success(),
        "normal clone failed"
    );
    let normal_result = workflow(&normal);

    // (b) the transparent lazy mount of the same remote.
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
    let mount_result = workflow(&mnt);

    // The results must be byte-identical — same tree, same committed bytes, same
    // (clean) status.
    assert_eq!(
        mount_result.0, normal_result.0,
        "HEAD tree differs: mount={} normal={}",
        mount_result.0, normal_result.0
    );
    assert_eq!(
        mount_result.1, normal_result.1,
        "committed README bytes differ"
    );
    assert_eq!(
        mount_result.2, normal_result.2,
        "post-commit status differs"
    );
    assert_eq!(
        mount_result.2, "",
        "both trees should be clean after commit"
    );

    mount.unmount();
    let _ = remote;
}
