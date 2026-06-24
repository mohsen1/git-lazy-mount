//! More stock-git workflows through the transparent mount, focused on the
//! index-only-vs-working-tree separation the design insists on (§8.1/§25.1):
//! `git rm --cached` and `git reset --mixed` change the index but must NOT
//! change the projected working-tree bytes. Plus amend (criterion 12) and stash
//! (criterion 18). Real `/dev/fuse` mount — runs under `--features fuse`.
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

struct Mounted {
    _tmp: tempfile::TempDir,
    _remote: glm_testkit::SeededRemote,
    mnt: std::path::PathBuf,
    mount: Option<glm_fuse::BackgroundMount>,
}

impl Mounted {
    fn new(files: &[(&str, &[u8])]) -> Mounted {
        let remote = glm_testkit::seed_remote(files);
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
        Mounted {
            _tmp: tmp,
            _remote: remote,
            mnt,
            mount: Some(mount),
        }
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(m) = self.mount.take() {
            m.unmount();
        }
    }
}

#[test]
fn rm_cached_preserves_the_working_tree_file() {
    // criterion 19: `git rm --cached` unstages but the working-tree file stays.
    let m = Mounted::new(&[("a.txt", b"alpha\n"), ("b.txt", b"beta\n")]);
    let (ok, _, e) = git(&m.mnt, &["rm", "--cached", "a.txt"]);
    assert!(ok, "rm --cached failed: {e}");
    // The file is still present and readable in the working tree.
    assert!(
        m.mnt.join("a.txt").exists(),
        "rm --cached deleted the worktree file"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("a.txt")).unwrap(),
        "alpha\n"
    );
    // ...but it now shows as untracked / deleted-from-index.
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(
        st.contains("a.txt"),
        "status should reflect the unstage: {st:?}"
    );
}

#[test]
fn reset_mixed_changes_index_not_projected_bytes() {
    // criterion 20: stage an edit, then `git reset` (mixed) — the index unstages
    // but the projected working-tree bytes are unchanged (§8.1/§25.1).
    let m = Mounted::new(&[("f.txt", b"v1\n")]);
    std::fs::write(m.mnt.join("f.txt"), b"v2-edited\n").unwrap();
    let (ok_add, _, _) = git(&m.mnt, &["add", "f.txt"]);
    assert!(ok_add);
    // staged
    let (_, cached, _) = git(&m.mnt, &["diff", "--cached", "--name-only"]);
    assert!(cached.contains("f.txt"));
    // reset (mixed) unstages WITHOUT touching the worktree
    let (ok_r, _, e) = git(&m.mnt, &["reset", "HEAD", "f.txt"]);
    assert!(ok_r, "reset failed: {e}");
    let (_, cached2, _) = git(&m.mnt, &["diff", "--cached", "--name-only"]);
    assert_eq!(cached2, "", "index should be unstaged after reset");
    // the working-tree bytes are STILL the edit (not reverted)
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("f.txt")).unwrap(),
        "v2-edited\n",
        "reset --mixed must not change projected working-tree bytes"
    );
}

#[test]
fn commit_amend_rewrites_the_tip() {
    // criterion 12: amend.
    let m = Mounted::new(&[("x.txt", b"one\n")]);
    std::fs::write(m.mnt.join("x.txt"), b"two\n").unwrap();
    git(&m.mnt, &["add", "x.txt"]);
    let (ok1, _, e1) = git(&m.mnt, &["commit", "-m", "first message"]);
    assert!(ok1, "commit failed: {e1}");
    let (_, head1, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    let (ok2, _, e2) = git(&m.mnt, &["commit", "--amend", "-m", "amended message"]);
    assert!(ok2, "amend failed: {e2}");
    let (_, head2, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_ne!(head1, head2, "amend should create a new commit");
    let (_, subj, _) = git(&m.mnt, &["log", "-1", "--pretty=%s"]);
    assert_eq!(subj, "amended message");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:x.txt"]);
    assert_eq!(shown, "two");
}

#[test]
fn stash_and_pop_round_trip() {
    // criterion 18: stash save reverts the worktree, pop restores it.
    let m = Mounted::new(&[("s.txt", b"original\n")]);
    std::fs::write(m.mnt.join("s.txt"), b"work in progress\n").unwrap();
    let (ok_s, _, e) = git(&m.mnt, &["stash"]);
    assert!(ok_s, "stash failed: {e}");
    // worktree reverted to the committed content
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("s.txt")).unwrap(),
        "original\n",
        "stash should revert the working tree"
    );
    let (ok_p, _, pe) = git(&m.mnt, &["stash", "pop"]);
    assert!(ok_p, "stash pop failed: {pe}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("s.txt")).unwrap(),
        "work in progress\n",
        "stash pop should restore the work"
    );
}
