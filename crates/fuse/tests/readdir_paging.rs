//! readdir paging: a directory larger than one kernel readdir page must list
//! **every** entry exactly once through the FUSE mount. The listing is
//! snapshotted at `opendir` and served in offset slices across the kernel's
//! paged `readdir` calls; a regression to re-reading the whole directory per
//! page is both O(entries²) and a chance to drop or duplicate entries at a page
//! boundary. This test would catch the latter.
//!
//! Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

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
fn large_directory_lists_every_entry_through_the_mount() {
    // Many more entries than fit in one readdir reply, so the kernel pages the
    // directory and the daemon serves it across several `readdir` calls.
    const N: usize = 1500;
    let owned: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| (format!("big/f{i:05}.txt"), b"x\n".to_vec()))
        .collect();
    let refs: Vec<(&str, &[u8])> = owned
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
    repo.build_index().unwrap();
    let proj = Arc::new(
        Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap(),
    );
    let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
    assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");

    let mut got: Vec<String> = std::fs::read_dir(mnt.join("big"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    got.sort();

    let mut want: Vec<String> = (0..N).map(|i| format!("f{i:05}.txt")).collect();
    want.sort();

    assert_eq!(
        got.len(),
        N,
        "every entry must be listed exactly once (no paging loss/dup)"
    );
    assert_eq!(got, want, "listing must match the directory exactly");

    mount.unmount();
}
