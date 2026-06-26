//! History-rewriting / commit-replay workflows exercised by **stock git**
//! through the transparent FUSE mount, against the real index/refs and the
//! durable overlay. Each command is plain `git -C <mnt> ...`; we assert the
//! exact outcome a normal (non-FUSE) checkout would give.
//!
//! Cluster:
//! * `git cherry-pick` — replay a commit from another branch onto HEAD. The
//!   patched file is written back through FUSE (overlay copy-up), a new commit
//!   is recorded, and the projected bytes match the cherry-picked content.
//! * `git revert` — apply the inverse of a commit (`--no-edit`, no $EDITOR);
//!   the reverted change disappears from the working tree and a revert commit
//!   lands on the tip.
//! * `git rebase --continue` — start a *conflicting* rebase, resolve the
//!   conflict in the projected working tree, `add`, then `--continue` to a
//!   linear, conflict-free completion.
//! * `git pull --rebase` — advance the remote via `SeededRemote::add_commit`,
//!   make a local commit, then `pull --rebase` to replay the local commit on
//!   top of the fetched remote tip (linear history, both files present).
//!
//! Lazy-fetch note: all four commands must *apply* changes, which forces git to
//! read the relevant blobs. Under `blob:none` those blobs fault in over the
//! `file://` promisor on demand — bounded by the touched paths, never the whole
//! repo. The harness keeps the `SeededRemote` alive so those faults can resolve.
//!
//! Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::{spawn_mount, BackgroundMount};
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_testkit::{seed_remote, SeededRemote};
use glm_worktree::Projection;

/// Run stock git in dir; returns (success, stdout_trimmed, stderr_trimmed).
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

/// A live mount; Drop unmounts. The SeededRemote is returned to the caller and
/// must outlive the mount (its file:// URL backs lazy blob faults).
struct Mounted {
    _tmp: tempfile::TempDir,
    mnt: std::path::PathBuf,
    mount: Option<BackgroundMount>,
}
impl Mounted {
    fn new(files: &[(&str, &[u8])]) -> (Mounted, SeededRemote) {
        let remote = seed_remote(files);
        let m = Mounted::over(&remote);
        (m, remote)
    }
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
        // Fault HEAD trees into the gitdir (tree:0 fetches none) before projecting.
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

/// `git cherry-pick`: build a commit on a side branch, switch back to main, and
/// replay it. The cherry-picked file must materialize through FUSE and a new
/// commit must land with that content.
#[test]
fn cherry_pick_replays_a_side_branch_commit() {
    let (m, _remote) = Mounted::new(&[("README.md", b"root\n")]);

    // A side branch adds a brand-new file in its own commit.
    let (ok_sw, _, e) = git(&m.mnt, &["switch", "-c", "feature"]);
    assert!(ok_sw, "switch -c failed: {e}");
    std::fs::write(m.mnt.join("picked.txt"), b"cherry payload\n").unwrap();
    git(&m.mnt, &["add", "picked.txt"]);
    let (ok_c, _, ce) = git(&m.mnt, &["commit", "-m", "feature: add picked.txt"]);
    assert!(ok_c, "feature commit failed: {ce}");
    let (_, pick_sha, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    // Back on main, the file is absent (the side commit is not in main's history).
    let (ok_back, _, be) = git(&m.mnt, &["switch", "main"]);
    assert!(ok_back, "switch main failed: {be}");
    assert!(
        !m.mnt.join("picked.txt").exists(),
        "picked.txt must not be present on main before the cherry-pick"
    );

    // Advance main with an unrelated commit so the cherry-picked commit lands on
    // a *distinct* parent (otherwise an identical parent/tree/message/author would
    // make it the same object as the side commit).
    std::fs::write(m.mnt.join("README.md"), b"root\nmain advances\n").unwrap();
    let (ok_ma, _, mae) = git(&m.mnt, &["commit", "-am", "main advances"]);
    assert!(ok_ma, "main advance commit failed: {mae}");

    // Cherry-pick the side commit onto main.
    let (ok_cp, _, cpe) = git(&m.mnt, &["cherry-pick", &pick_sha]);
    assert!(ok_cp, "cherry-pick failed: {cpe}");

    // The patched file is now projected through FUSE with the right bytes...
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("picked.txt")).unwrap(),
        "cherry payload\n",
        "cherry-picked file must be projected with its content"
    );
    // ...the tree is clean, and HEAD:picked.txt holds the content.
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after cherry-pick, got {st:?}");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:picked.txt"]);
    assert_eq!(shown, "cherry payload");
    // The new commit is a distinct object from the original (new parent).
    let (_, new_head, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, pick_sha, "cherry-pick must create a new commit");
    let (_, subj, _) = git(&m.mnt, &["log", "-1", "--pretty=%s"]);
    assert_eq!(subj, "feature: add picked.txt");
}

/// `git revert`: revert a commit that modified a baseline file. The reverted
/// edit disappears from the working tree and a revert commit lands on the tip.
#[test]
fn revert_undoes_a_committed_change() {
    let (m, _remote) = Mounted::new(&[("data.txt", b"original\n")]);

    // Commit a change to the baseline file.
    std::fs::write(m.mnt.join("data.txt"), b"changed\n").unwrap();
    let (ok_c, _, ce) = git(&m.mnt, &["commit", "-am", "change data.txt"]);
    assert!(ok_c, "commit failed: {ce}");
    let (_, target, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("data.txt")).unwrap(),
        "changed\n"
    );

    // Revert that commit (no $EDITOR: --no-edit).
    let (ok_rv, _, rve) = git(&m.mnt, &["revert", "--no-edit", &target]);
    assert!(ok_rv, "revert failed: {rve}");

    // The file is back to its pre-change content through FUSE...
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("data.txt")).unwrap(),
        "original\n",
        "revert must restore the pre-change bytes in the working tree"
    );
    // ...the tree is clean and HEAD is a NEW (revert) commit on top of target.
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after revert, got {st:?}");
    let (_, head, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_ne!(head, target, "revert must add a new commit");
    let (_, parent, _) = git(&m.mnt, &["rev-parse", "HEAD^"]);
    assert_eq!(parent, target, "revert commit's parent must be the target");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:data.txt"]);
    assert_eq!(shown, "original");
}

/// `git rebase --continue`: start a *conflicting* rebase, resolve the conflict
/// the projected working tree surfaces, `add`, then `--continue` to a linear,
/// conflict-free completion.
#[test]
fn rebase_continue_completes_after_conflict_resolution() {
    let (m, _remote) = Mounted::new(&[("README.md", b"line one\nshared\nline three\n")]);

    // topic edits the shared line.
    let (ok_tp, _, tpe) = git(&m.mnt, &["switch", "-c", "topic"]);
    assert!(ok_tp, "switch -c topic failed: {tpe}");
    std::fs::write(m.mnt.join("README.md"), b"line one\nTOPIC\nline three\n").unwrap();
    git(&m.mnt, &["commit", "-am", "topic edits shared"]);
    let (_, topic_msg_before, _) = git(&m.mnt, &["log", "-1", "--pretty=%s"]);

    // main edits the SAME line differently → rebase will conflict.
    let (ok_back, _, be) = git(&m.mnt, &["switch", "main"]);
    assert!(ok_back, "switch main failed: {be}");
    std::fs::write(m.mnt.join("README.md"), b"line one\nMAIN\nline three\n").unwrap();
    git(&m.mnt, &["commit", "-am", "main edits shared"]);

    // Rebase topic onto main: it must stop with a conflict.
    git(&m.mnt, &["switch", "topic"]);
    let (ok_rb, _, _) = git(&m.mnt, &["rebase", "main"]);
    assert!(!ok_rb, "rebase across the shared-line edit should conflict");

    // A rebase is in progress (state lives in the admin gitdir, not the
    // synthetic `.git`): resolve via --git-path.
    let (_, rm, _) = git(&m.mnt, &["rev-parse", "--git-path", "rebase-merge"]);
    let (_, ra, _) = git(&m.mnt, &["rev-parse", "--git-path", "rebase-apply"]);
    assert!(
        Path::new(&rm).exists() || Path::new(&ra).exists(),
        "a rebase should be in progress (checked {rm} / {ra})"
    );
    // The conflicted file git wrote through FUSE carries conflict markers.
    let conflicted = std::fs::read_to_string(m.mnt.join("README.md")).unwrap();
    assert!(
        conflicted.contains("<<<<<<<")
            && conflicted.contains("=======")
            && conflicted.contains(">>>>>>>"),
        "working-tree file must contain conflict markers:\n{conflicted}"
    );

    // Resolve, stage, and continue (no $EDITOR: GIT_EDITOR=true keeps the
    // generated message).
    std::fs::write(m.mnt.join("README.md"), b"line one\nRESOLVED\nline three\n").unwrap();
    let (ok_add, _, ae) = git(&m.mnt, &["add", "README.md"]);
    assert!(ok_add, "add after resolve failed: {ae}");
    let cont = Command::new("git")
        .arg("-C")
        .arg(&m.mnt)
        .args(["rebase", "--continue"])
        .env("GIT_EDITOR", "true")
        .output()
        .expect("spawn rebase --continue");
    assert!(
        cont.status.success(),
        "rebase --continue failed: {}",
        String::from_utf8_lossy(&cont.stderr)
    );

    // No rebase in progress, history is linear on top of main, and the resolved
    // content is what HEAD records.
    assert!(
        !Path::new(&rm).exists() && !Path::new(&ra).exists(),
        "rebase state must be gone after --continue"
    );
    let (ok_anc, _, _) = git(&m.mnt, &["merge-base", "--is-ancestor", "main", "topic"]);
    assert!(ok_anc, "after rebase, main must be an ancestor of topic");
    let (_, merges, _) = git(&m.mnt, &["rev-list", "--merges", "main..topic"]);
    assert_eq!(merges, "", "rebased history must be linear");
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after --continue, got {st:?}");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:README.md"]);
    assert_eq!(shown, "line one\nRESOLVED\nline three");
    // The replayed commit kept its subject.
    let (_, topic_msg_after, _) = git(&m.mnt, &["log", "-1", "--pretty=%s"]);
    assert_eq!(topic_msg_after, topic_msg_before);
}

/// `git pull --rebase`: the remote advances (a new commit pushed from a separate
/// checkout), a local commit is made on a *different* file, then `pull --rebase`
/// replays the local commit on top of the fetched remote tip — linear history,
/// both files present.
#[test]
fn pull_rebase_replays_local_commit_atop_remote() {
    let (m, mut remote) = Mounted::new(&[("README.md", b"base\n")]);

    // Make a local commit (touches a local-only file, so it never conflicts).
    std::fs::write(m.mnt.join("local.txt"), b"local work\n").unwrap();
    git(&m.mnt, &["add", "local.txt"]);
    let (ok_lc, _, lce) = git(&m.mnt, &["commit", "-m", "local: add local.txt"]);
    assert!(ok_lc, "local commit failed: {lce}");
    let (_, local_sha, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    // The remote advances independently (separate plain checkout pushes a commit).
    let remote_tip = remote.add_commit(&[("remote.txt", b"remote work\n")], "remote add");

    // pull --rebase: fetch the remote tip, then replay the local commit on top.
    let (ok_pr, _, pre) = git(&m.mnt, &["pull", "--rebase", "origin", "main"]);
    assert!(ok_pr, "pull --rebase failed: {pre}");

    // Both files are projected through FUSE (the remote blob faulted in lazily).
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("remote.txt")).unwrap(),
        "remote work\n",
        "fetched remote file must be projected after pull --rebase"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("local.txt")).unwrap(),
        "local work\n",
        "local file must survive the replay"
    );

    // History is linear: the remote tip is an ancestor of HEAD, the local commit
    // was rewritten onto it (new sha, but the remote commit is unchanged), and
    // there is no merge commit.
    let (ok_anc, _, _) = git(
        &m.mnt,
        &["merge-base", "--is-ancestor", &remote_tip, "HEAD"],
    );
    assert!(
        ok_anc,
        "remote tip must be an ancestor of HEAD after rebase"
    );
    let (_, parent, _) = git(&m.mnt, &["rev-parse", "HEAD^"]);
    assert_eq!(
        parent, remote_tip,
        "the replayed local commit must sit directly on the remote tip"
    );
    let (_, head, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_ne!(head, local_sha, "rebase must rewrite the local commit");
    let (_, merges, _) = git(&m.mnt, &["rev-list", "--merges", "HEAD"]);
    assert_eq!(merges, "", "pull --rebase must not create a merge commit");
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after pull --rebase, got {st:?}");
}
