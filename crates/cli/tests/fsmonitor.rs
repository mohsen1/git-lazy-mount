//! End-to-end FSMonitor: with `core.fsmonitor` wired to the
//! `git-lazy-mount-fsmonitor` hook, the durable change journal lets git learn
//! what changed without re-statting the whole tree — and, crucially, **without
//! ever a false negative** (a real edit is always surfaced).
//!
//! NOTE on the zero-blob first status: it is **not achievable** with stock git +
//! a `blob:none` clone, and that is a fundamental limitation, not a bug.
//! `GIT_TRACE_FSMONITOR` shows git marks each read-tree'd entry clean from the
//! hook's empty reply, then immediately `mark_fsmonitor_invalid` because the
//! entry has **no stat data** — git must populate the stat (size + mtime) to skip
//! the content check, and under `blob:none` the size requires fetching the blob.
//! The fsmonitor-valid bit does not override an empty-stat entry. So the *first*
//! clean status faults each blob once; fsmonitor's win is the *subsequent*
//! statuses (no redundant stat scan on huge repos).
//!
//! Real `/dev/fuse` mount.
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::journal::{journal_dir, workspace_id, ChangeJournal};
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
fn fsmonitor_hook_detects_changes_with_no_false_negatives() {
    const N: usize = 12;
    let files: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| (format!("src/f{i}.txt"), format!("file {i}\n").into_bytes()))
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let remote = glm_testkit::seed_remote(&refs);
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    let gitdir = tmp.path().join("git");

    let repo = AdminRepo::clone(
        &remote.url,
        &gitdir,
        &mnt,
        &tmp.path().join("anchor"),
        &CloneOptions::default(),
    )
    .unwrap();
    repo.build_index().unwrap();
    drop(repo);

    let repo2 = AdminRepo::open(&gitdir, &mnt).unwrap();
    let journal = ChangeJournal::open(journal_dir(&gitdir), workspace_id(&gitdir), 1, 0).unwrap();
    let proj = Arc::new(
        Projection::open(repo2, tmp.path().join("cache"), tmp.path().join("overlay"))
            .unwrap()
            .with_journal(journal),
    );
    let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
    assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");
    git(&mnt, &["config", "user.email", "t@e"]);
    git(&mnt, &["config", "user.name", "t"]);

    let hook = env!("CARGO_BIN_EXE_git-lazy-mount-fsmonitor");
    git(&mnt, &["config", "core.fsmonitor", hook]);
    git(&mnt, &["config", "core.fsmonitorHookVersion", "2"]);

    // The hook is invoked from the worktree and returns a valid v2 reply — a glm
    // token, no paths (the quiescent bootstrap), NOT a full invalidation.
    let out = Command::new(hook)
        .args(["2", ""])
        .current_dir(&mnt)
        .output()
        .unwrap();
    assert!(out.status.success(), "hook must exit 0");
    assert!(
        out.stdout.starts_with(b"glm1:"),
        "hook emits a glm token: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !out.stdout.ends_with(b"/\0"),
        "quiescent bootstrap is not a full invalidation"
    );

    // First clean status: correct (its blob faults are the fundamental blob:none
    // cost — measured, not asserted to 0; see the module note).
    let h0 = proj.hydrations();
    let (ok, st) = git(&mnt, &["status", "--porcelain"]);
    assert!(ok, "status failed");
    assert_eq!(st, "", "tree must be clean");
    eprintln!(
        "FSMONITOR: first clean status faulted {} blobs for {N} files (fundamental blob:none cost)",
        proj.hydrations() - h0
    );

    // The load-bearing guarantee: an edit through the mount is surfaced (the
    // journal records it synchronously; the hook reports it) — NEVER a false
    // negative — while an unedited file stays clean.
    std::fs::write(mnt.join("src/f3.txt"), b"EDITED THROUGH THE MOUNT\n").unwrap();
    let (_, st2) = git(&mnt, &["status", "--porcelain"]);
    assert!(
        st2.contains("src/f3.txt"),
        "fsmonitor must surface the edit (no false negative): {st2:?}"
    );
    assert!(
        !st2.contains("f5.txt"),
        "an unedited file must not show as modified: {st2:?}"
    );

    // It commits cleanly and the tree is clean again afterward.
    assert!(git(&mnt, &["commit", "-am", "edit f3"]).0, "commit failed");
    let (_, st3) = git(&mnt, &["status", "--porcelain"]);
    assert_eq!(st3, "", "tree clean after commit");

    mount.unmount();
    let _ = remote;
}
