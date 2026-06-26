//! Further git workflows exercised by **stock git** through the transparent
//! FUSE mount, against the real index/refs/overlay:
//!
//! * a *conflicting* merge: the real index gains the base/ours/theirs conflict
//!   stages and the overlay surfaces the conflict-marker file git writes
//!   through FUSE; then a normal resolve + `add` + `commit` clears it.
//! * `git rebase`: a conflict-free rebase onto an advanced `main` yields linear
//!   history; a *conflicting* rebase followed by `git rebase --abort` returns
//!   the tree to its pre-rebase state.
//! * `git fetch` + merge: a NEW commit pushed to the remote from a separate
//!   plain checkout advances the remote-tracking ref through the mount, and a
//!   merge brings the new file into the working tree (the lazily-cloned blob
//!   faults in over the `file://` promisor).
//! * `git add -p`: stage exactly ONE of two hunks by feeding the interactive
//!   decisions on stdin (no PTY needed on this platform).
//!
//! Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::{spawn_mount, BackgroundMount};
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_testkit::{seed_remote, SeededRemote};
use glm_worktree::Projection;

fn git(dir: &Path, args: &[&str]) -> (bool, String, String) {
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

/// A live mount. `Drop` unmounts. The `SeededRemote` is owned by the *caller*
/// (returned alongside) so tests that push to it can do so safely — its
/// `file://` URL must outlive the mount to back lazy blob faults.
struct Mounted {
    _tmp: tempfile::TempDir,
    mnt: std::path::PathBuf,
    mount: Option<BackgroundMount>,
}

impl Mounted {
    /// Seed a remote with `files`, clone+mount it, and return both the mount and
    /// the remote (the caller keeps the remote alive).
    fn new(files: &[(&str, &[u8])]) -> (Mounted, SeededRemote) {
        let remote = seed_remote(files);
        let m = Mounted::over(&remote);
        (m, remote)
    }

    /// Clone `remote` (by URL) into a fresh workspace and mount it. Does not own
    /// the remote.
    fn over(remote: &SeededRemote) -> Mounted {
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
        // Fault the HEAD trees into the gitdir (the default `tree:0` clone fetches
        // none) before projecting, exactly as the real mount flow does.
        repo.build_index().unwrap();
        let proj = Arc::new(
            Projection::open(repo, tmp.path().join("cache"), tmp.path().join("overlay")).unwrap(),
        );
        let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
        assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");
        git(&mnt, &["config", "user.email", "t@example.com"]);
        git(&mnt, &["config", "user.name", "Test"]);
        Mounted {
            _tmp: tmp,
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
fn conflicting_merge_stages_conflict_and_resolves() {
    // Two branches edit the SAME README line differently.
    let (m, _remote) = Mounted::new(&[("README.md", b"line one\nshared line\nline three\n")]);

    let (ok_sw, _, e) = git(&m.mnt, &["switch", "-c", "feature"]);
    assert!(ok_sw, "switch -c failed: {e}");
    std::fs::write(
        m.mnt.join("README.md"),
        b"line one\nFEATURE EDIT\nline three\n",
    )
    .unwrap();
    let (ok_c1, _, e1) = git(&m.mnt, &["commit", "-am", "feature edits the shared line"]);
    assert!(ok_c1, "feature commit failed: {e1}");

    let (ok_back, _, be) = git(&m.mnt, &["switch", "main"]);
    assert!(ok_back, "switch main failed: {be}");
    std::fs::write(
        m.mnt.join("README.md"),
        b"line one\nMAIN EDIT\nline three\n",
    )
    .unwrap();
    let (ok_c2, _, e2) = git(&m.mnt, &["commit", "-am", "main edits the shared line"]);
    assert!(ok_c2, "main commit failed: {e2}");

    // The merge must conflict.
    let (ok_m, _, _) = git(&m.mnt, &["merge", "--no-edit", "feature"]);
    assert!(!ok_m, "merge of conflicting edits should fail");
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(
        st.contains("UU") && st.contains("README.md"),
        "unmerged path: {st:?}"
    );

    // The real index carries the three conflict stages.
    let (_, stages, _) = git(&m.mnt, &["ls-files", "-u", "README.md"]);
    for stage in ["1\t", "2\t", "3\t"] {
        assert!(
            stages.contains(stage),
            "missing conflict stage {stage:?}: {stages:?}"
        );
    }
    // The overlay file git wrote through FUSE contains conflict markers.
    let conflicted = std::fs::read_to_string(m.mnt.join("README.md")).unwrap();
    assert!(
        conflicted.contains("<<<<<<<")
            && conflicted.contains("=======")
            && conflicted.contains(">>>>>>>"),
        "working-tree file must contain conflict markers:\n{conflicted}"
    );
    assert!(conflicted.contains("MAIN EDIT") && conflicted.contains("FEATURE EDIT"));

    // Resolve normally.
    std::fs::write(m.mnt.join("README.md"), b"line one\nRESOLVED\nline three\n").unwrap();
    let (ok_add, _, ae) = git(&m.mnt, &["add", "README.md"]);
    assert!(ok_add, "add after resolve failed: {ae}");
    let (ok_commit, _, ce) = git(&m.mnt, &["commit", "--no-edit"]);
    assert!(ok_commit, "merge commit after resolve failed: {ce}");
    let (_, st2, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st2, "", "tree should be clean after resolve, got {st2:?}");
    let (_, unmerged, _) = git(&m.mnt, &["ls-files", "-u"]);
    assert_eq!(unmerged, "", "no unmerged entries should remain");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:README.md"]);
    assert_eq!(shown, "line one\nRESOLVED\nline three");
}

#[test]
fn rebase_linearizes_then_abort_restores() {
    let (m, _remote) = Mounted::new(&[("README.md", b"line one\nshared\nline three\n")]);

    // Part A: conflict-free rebase onto an advanced main.
    let (ok_sw, _, e) = git(&m.mnt, &["switch", "-c", "feature"]);
    assert!(ok_sw, "switch -c failed: {e}");
    std::fs::write(m.mnt.join("feat.txt"), b"feature work\n").unwrap();
    git(&m.mnt, &["add", "feat.txt"]);
    let (ok_fc, _, fe) = git(&m.mnt, &["commit", "-m", "add feat.txt"]);
    assert!(ok_fc, "feature commit failed: {fe}");

    let (ok_back, _, be) = git(&m.mnt, &["switch", "main"]);
    assert!(ok_back, "switch main failed: {be}");
    std::fs::write(m.mnt.join("mainfile.txt"), b"main advanced\n").unwrap();
    git(&m.mnt, &["add", "mainfile.txt"]);
    let (ok_mc, _, me) = git(&m.mnt, &["commit", "-m", "advance main"]);
    assert!(ok_mc, "main commit failed: {me}");

    git(&m.mnt, &["switch", "feature"]);
    let (ok_rb, _, rbe) = git(&m.mnt, &["rebase", "main"]);
    assert!(ok_rb, "clean rebase failed: {rbe}");
    let (ok_anc, _, _) = git(&m.mnt, &["merge-base", "--is-ancestor", "main", "feature"]);
    assert!(ok_anc, "after rebase, main must be an ancestor of feature");
    let (_, merges, _) = git(&m.mnt, &["rev-list", "--merges", "main..feature"]);
    assert_eq!(merges, "", "rebased history must be linear");
    assert!(m.mnt.join("feat.txt").exists() && m.mnt.join("mainfile.txt").exists());

    // Part B: conflicting rebase, then --abort restores prior state.
    git(&m.mnt, &["switch", "main"]);
    std::fs::write(m.mnt.join("README.md"), b"line one\nMAIN\nline three\n").unwrap();
    git(&m.mnt, &["commit", "-am", "main edits shared"]);
    let (ok_tp, _, tpe) = git(&m.mnt, &["switch", "-c", "topic", "feature"]);
    assert!(ok_tp, "switch -c topic failed: {tpe}");
    std::fs::write(m.mnt.join("README.md"), b"line one\nTOPIC\nline three\n").unwrap();
    git(&m.mnt, &["commit", "-am", "topic edits shared"]);
    let (_, pre_tip, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    let (ok_rb2, _, _) = git(&m.mnt, &["rebase", "main"]);
    assert!(
        !ok_rb2,
        "rebase across the shared-line edit should conflict"
    );
    // `.git` at the mount is the synthetic gitfile, so rebase state lives in the
    // admin gitdir — resolve the real path via `rev-parse --git-path`.
    let (_, rm, _) = git(&m.mnt, &["rev-parse", "--git-path", "rebase-merge"]);
    let (_, ra, _) = git(&m.mnt, &["rev-parse", "--git-path", "rebase-apply"]);
    assert!(
        std::path::Path::new(&rm).exists() || std::path::Path::new(&ra).exists(),
        "a rebase should be in progress (checked {rm} / {ra})"
    );
    let (ok_ab, _, abe) = git(&m.mnt, &["rebase", "--abort"]);
    assert!(ok_ab, "rebase --abort failed: {abe}");
    let (_, post_tip, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_eq!(post_tip, pre_tip, "abort must restore the pre-rebase tip");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("README.md")).unwrap(),
        "line one\nTOPIC\nline three\n",
        "abort must restore the working-tree content"
    );
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after abort, got {st:?}");
}

#[test]
fn fetch_then_merge_brings_remote_commit_into_worktree() {
    // A NEW commit pushed to the remote from a separate plain checkout must be
    // observable through the mount.
    let (m, mut remote) = Mounted::new(&[("README.md", b"base\n")]);
    let (_, before, _) = git(&m.mnt, &["rev-parse", "origin/main"]);

    // push a new commit to the remote from a separate plain checkout.
    let new_tip = remote.add_commit(
        &[("from_remote.txt", b"hello from the remote\n")],
        "remote add",
    );

    let (ok_f, _, fe) = git(&m.mnt, &["fetch", "origin"]);
    assert!(ok_f, "fetch failed: {fe}");
    let (_, after, _) = git(&m.mnt, &["rev-parse", "origin/main"]);
    assert_ne!(
        after, before,
        "remote-tracking ref must advance after fetch"
    );
    assert_eq!(
        after, new_tip,
        "origin/main must point at the pushed commit"
    );

    assert!(
        !m.mnt.join("from_remote.txt").exists(),
        "the remote file must not be present before the merge"
    );
    let (ok_m, _, me) = git(&m.mnt, &["merge", "--ff-only", "origin/main"]);
    assert!(ok_m, "merge of fetched commit failed: {me}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("from_remote.txt")).unwrap(),
        "hello from the remote\n",
        "the fetched file must be projected after the merge"
    );
    let (_, head, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_eq!(head, new_tip, "HEAD must be at the merged remote commit");
}

#[test]
fn add_patch_stages_one_of_two_hunks() {
    // `git add -p` stages exactly ONE of two hunks via stdin.
    let (m, _remote) = Mounted::new(&[("f.txt", b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n")]);
    std::fs::write(
        m.mnt.join("f.txt"),
        b"TOP\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nBOTTOM\n",
    )
    .unwrap();

    let mut child = Command::new("git")
        .arg("-C")
        .arg(&m.mnt)
        .arg("-c")
        .arg("interactive.singleKey=false")
        .args(["add", "-p", "f.txt"])
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git add -p");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"y\nn\n")
        .expect("feed add -p decisions");
    let out = child.wait_with_output().expect("wait add -p");
    assert!(
        out.status.success(),
        "git add -p failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let (_, cached, _) = git(&m.mnt, &["diff", "--cached"]);
    assert!(
        cached.contains("TOP"),
        "first hunk must be staged: {cached:?}"
    );
    assert!(
        !cached.contains("BOTTOM"),
        "second hunk must NOT be staged: {cached:?}"
    );
    let (_, unstaged, _) = git(&m.mnt, &["diff"]);
    assert!(
        unstaged.contains("BOTTOM"),
        "second hunk must remain unstaged: {unstaged:?}"
    );
}
