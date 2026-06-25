//! End-to-end FSMonitor: with `core.fsmonitor` wired to the
//! `git-lazy-mount-fsmonitor` hook and the index extension pre-seeded, a cold
//! `git status`/`git diff` faults **zero** blobs, and a real edit is always
//! surfaced (never a false negative).
//!
//! The zero-blob first status IS achievable with stock git + a `blob:none` clone.
//! A freshly `read-tree`'d index has no fsmonitor extension, so on the first
//! status git has no valid bits and stats (faults) every entry before writing the
//! extension. Seeding the extension up front (every entry `CE_FSMONITOR_VALID`
//! plus the journal's seq-0 token) lets git trust the hook's "nothing changed"
//! reply immediately and skip the stat/content check — `refresh_cache_ent` returns
//! early on `CE_FSMONITOR_VALID`, before any `lstat`. See `AdminRepo::seed_fsmonitor_valid`.
//!
//! Real `/dev/fuse` mount.
#![cfg(feature = "fuse")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::{spawn_mount, BackgroundMount};
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_testkit::SeededRemote;
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

fn config_gitdir(gitdir: &Path, key: &str, val: &str) {
    let ok = Command::new("git")
        .arg("--git-dir")
        .arg(gitdir)
        .args(["config", key, val])
        .status()
        .expect("git config")
        .success();
    assert!(ok, "git config {key}");
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

/// A mounted workspace whose index has been seeded exactly as `cmd_mount` does:
/// hook configured, journal created, fsmonitor extension pre-seeded. Holds the
/// remote and tempdir alive (the `blob:none` mount lazily fetches from the remote).
struct Mounted {
    proj: Arc<Projection>,
    mount: BackgroundMount,
    mnt: PathBuf,
    _tmp: tempfile::TempDir,
    _remote: SeededRemote,
}

fn mount_with_seed(refs: &[(&str, &[u8])]) -> Mounted {
    let remote = glm_testkit::seed_remote(refs);
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

    // Mirror `cmd_mount`: wire the hook, create the (empty) journal so the hook
    // answers the seq-0 bootstrap, then seed the fsmonitor extension — all before
    // the mount, so the seed itself never faults a worktree blob.
    let hook = env!("CARGO_BIN_EXE_git-lazy-mount-fsmonitor");
    config_gitdir(&gitdir, "core.fsmonitor", hook);
    config_gitdir(&gitdir, "core.fsmonitorHookVersion", "2");
    ChangeJournal::open(journal_dir(&gitdir), workspace_id(&gitdir), 1, 0).unwrap();
    repo.seed_fsmonitor_valid().unwrap();
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

    Mounted {
        proj,
        mount,
        mnt,
        _tmp: tmp,
        _remote: remote,
    }
}

#[test]
fn first_status_faults_zero_blobs_and_surfaces_edits() {
    const N: usize = 12;
    let files: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| (format!("src/f{i}.txt"), format!("file {i}\n").into_bytes()))
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let h = mount_with_seed(&refs);

    // The hook answers the quiescent bootstrap: a glm token, no paths (NOT a full
    // invalidation).
    let hook = env!("CARGO_BIN_EXE_git-lazy-mount-fsmonitor");
    let out = Command::new(hook)
        .args(["2", ""])
        .current_dir(&h.mnt)
        .output()
        .unwrap();
    assert!(out.status.success(), "hook must exit 0");
    assert!(out.stdout.starts_with(b"glm1:"), "hook emits a glm token");
    assert!(
        !out.stdout.ends_with(b"/\0"),
        "quiescent bootstrap is not a full invalidation"
    );

    // The fix: the first clean status faults ZERO blobs.
    let h0 = h.proj.hydrations();
    let (ok, st) = git(&h.mnt, &["status", "--porcelain"]);
    assert!(ok, "status failed");
    assert_eq!(st, "", "tree must be clean");
    assert_eq!(
        h.proj.hydrations() - h0,
        0,
        "first clean status must fault 0 blobs (seeded fsmonitor extension)"
    );

    // `git diff` is the same refresh path — also zero-fault on a clean tree.
    let h1 = h.proj.hydrations();
    let (ok, _) = git(&h.mnt, &["diff", "--quiet"]);
    assert!(ok, "diff on a clean tree exits 0");
    assert_eq!(
        h.proj.hydrations() - h1,
        0,
        "clean `git diff` faults 0 blobs"
    );

    // Correctness: an edit through the mount is surfaced (journal → hook), never a
    // false negative; an unedited file stays clean.
    std::fs::write(h.mnt.join("src/f3.txt"), b"EDITED THROUGH THE MOUNT\n").unwrap();
    let (_, st2) = git(&h.mnt, &["status", "--porcelain"]);
    assert!(
        st2.contains("src/f3.txt"),
        "fsmonitor must surface the edit (no false negative): {st2:?}"
    );
    assert!(
        !st2.contains("f5.txt"),
        "an unedited file must not show as modified: {st2:?}"
    );

    // Commits cleanly; the tree is clean again afterward.
    assert!(
        git(&h.mnt, &["commit", "-am", "edit f3"]).0,
        "commit failed"
    );
    assert_eq!(
        git(&h.mnt, &["status", "--porcelain"]).1,
        "",
        "tree clean after commit"
    );

    h.mount.unmount();
}

#[test]
fn conversion_attributed_files_are_not_seeded() {
    // A file under a clean/smudge `filter` must NOT be seeded valid — its
    // working-tree bytes can differ from the blob, so seeding it could hide a real
    // diff. The carve-out leaves it for git to check while the unattributed
    // majority are seeded and skipped, and correctness is preserved throughout.
    const N: usize = 10;
    let mut files: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| (format!("src/f{i}.txt"), format!("file {i}\n").into_bytes()))
        .collect();
    files.push((
        ".gitattributes".into(),
        b"secret.dat filter=redact\n".to_vec(),
    ));
    files.push(("secret.dat".into(), b"top secret\n".to_vec()));
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let h = mount_with_seed(&refs);
    let total = files.len() as u64;

    let h0 = h.proj.hydrations();
    let (ok, _) = git(&h.mnt, &["status", "--porcelain"]);
    assert!(ok, "status failed");
    let faulted = h.proj.hydrations() - h0;
    // The seeded majority is skipped: far fewer faults than every tracked file.
    assert!(
        faulted < total,
        "carve-out must still skip the seeded majority: faulted {faulted} of {total}"
    );

    // Correctness still holds with attributes present: an edit is surfaced.
    std::fs::write(h.mnt.join("src/f2.txt"), b"changed\n").unwrap();
    assert!(
        git(&h.mnt, &["status", "--porcelain"])
            .1
            .contains("src/f2.txt"),
        "edit must surface even with conversion attributes present"
    );

    h.mount.unmount();
}
