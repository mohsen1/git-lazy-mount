//! Measure branch-change eagerness: switching to a branch that differs in M of
//! N files must touch O(M) blobs, NOT O(N). Quantifies the M-stage eagerness
//! rather than hiding it (we do not claim google3-style lazy branch switching).
//! Real `/dev/fuse` mount.
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
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
fn switch_eagerness_is_bounded_by_the_delta_not_the_repo() {
    const N: usize = 40; // total tracked files
    const M: usize = 4; // files the feature branch changes

    let files: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| (format!("d/f{i:03}.txt"), format!("base {i}\n").into_bytes()))
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

    // A feature branch that edits exactly M of the N files.
    assert!(
        git(&mnt, &["switch", "-c", "feature"]).0,
        "switch -c feature"
    );
    for i in 0..M {
        std::fs::write(mnt.join(format!("d/f{i:03}.txt")), format!("feature {i}\n")).unwrap();
    }
    assert!(
        git(&mnt, &["commit", "-am", "feature changes M files"]).0,
        "feature commit"
    );
    assert!(git(&mnt, &["switch", "main"]).0, "switch back to main");

    // Measure the blobs faulted while switching main -> feature. Git updates only
    // the changed paths, so the cost is bounded by the delta, not the repo.
    let before = proj.hydrations();
    assert!(git(&mnt, &["switch", "feature"]).0, "switch feature");
    let faulted = proj.hydrations() - before;

    eprintln!("SWITCH EAGERNESS: N={N} files, M={M} changed, blobs faulted on switch = {faulted}");

    // Correctness: the changed files now hold the feature content; the rest are
    // untouched baseline.
    for i in 0..M {
        assert_eq!(
            std::fs::read_to_string(mnt.join(format!("d/f{i:03}.txt"))).unwrap(),
            format!("feature {i}\n"),
            "changed file {i} must hold feature content after switch"
        );
    }
    assert_eq!(
        std::fs::read_to_string(mnt.join(format!("d/f{:03}.txt", N - 1))).unwrap(),
        format!("base {}\n", N - 1),
        "an unchanged file must still hold its baseline content"
    );

    // The headline: eagerness is bounded by the delta. It must be well below the
    // repo size — switching does NOT rewrite/stat every file.
    assert!(
        faulted < N as u64,
        "switch faulted {faulted} blobs for a {M}-file delta in a {N}-file repo — \
         that is not bounded by the delta"
    );

    mount.unmount();
    let _ = remote;
}
