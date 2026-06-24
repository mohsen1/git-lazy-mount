//! Measure `git status` hydration through the mount (design.md §38.4, §27):
//! how many working-file blobs does a clean status fault in, the first time and
//! on repeat? This quantifies the current (pre-FSMonitor) eagerness rather than
//! hiding it. Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

fn git(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
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

#[test]
fn clean_status_hydration_is_measured() {
    // A handful of tracked files across a couple of directories.
    let files: Vec<(String, Vec<u8>)> = (0..12)
        .map(|i| {
            (
                format!("src/f{i}.txt"),
                format!("contents of file {i}\n").into_bytes(),
            )
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let remote = glm_testkit::seed_remote(&refs);
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
    git(&mnt, &["config", "user.email", "t@e"]);
    git(&mnt, &["config", "user.name", "t"]);

    // Status #1 (cold): may be eager — git verifies content against the index.
    let h0 = proj.hydrations();
    let (ok1, st1) = git(&mnt, &["status", "--porcelain"]);
    assert!(ok1);
    assert_eq!(st1, "", "tree should be clean");
    let cold = proj.hydrations() - h0;

    // Status #2 (repeat clean): should fault ZERO blobs — git's index refresh
    // from status #1 means the stat info now matches, so it skips re-reading.
    let h1 = proj.hydrations();
    let (ok2, st2) = git(&mnt, &["status", "--porcelain"]);
    assert!(ok2);
    assert_eq!(st2, "", "tree should still be clean");
    let warm = proj.hydrations() - h1;

    eprintln!(
        "STATUS HYDRATION: cold(first)={cold} warm(repeat)={warm} files={}",
        files.len()
    );
    // The headline budget (§38.4): a *repeated* clean status fetches no blobs.
    assert_eq!(
        warm, 0,
        "repeat clean status must fetch 0 blobs, got {warm}"
    );

    mount.unmount();
    let _ = remote;
}
