//! The remaining "not yet classified" stock-git commands exercised through the
//! transparent FUSE mount: mailbox/patch application, notes, replace refs,
//! cherry-pick ranges, tag/describe, archive, and `restore --staged`. Each is
//! plain `git -C <mnt> ...`; we assert it behaves as it would on a normal
//! checkout. Real `/dev/fuse` mount — runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::io::Write as _;
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

/// Run git feeding `stdin`; returns (ok, stdout, stderr).
fn git_stdin(dir: &Path, args: &[&str], stdin: &[u8]) -> (bool, String, String) {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
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
    mnt: std::path::PathBuf,
    mount: Option<BackgroundMount>,
}
impl Mounted {
    fn new(files: &[(&str, &[u8])]) -> (Mounted, SeededRemote) {
        let remote = seed_remote(files);
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
        (
            Mounted {
                _tmp: tmp,
                mnt,
                mount: Some(mount),
            },
            remote,
        )
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
fn git_apply_a_patch_to_the_working_tree() {
    let (m, _r) = Mounted::new(&[("file.txt", b"line one\nline two\n")]);
    // A unified diff that edits line two.
    let patch = b"--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n line one\n-line two\n+line two EDITED\n";
    let (ok, _o, e) = git_stdin(&m.mnt, &["apply"], patch);
    assert!(ok, "git apply failed: {e}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("file.txt")).unwrap(),
        "line one\nline two EDITED\n",
        "the patch must be applied to the projected working tree"
    );
}

#[test]
fn git_am_applies_a_mailbox_patch() {
    let (m, _r) = Mounted::new(&[("doc.md", b"original\n")]);
    // Build a commit, format-patch it, reset back, then `git am` it — the commit
    // is recreated from the mailbox and the working tree gains the change.
    std::fs::write(m.mnt.join("doc.md"), b"amended body\n").unwrap();
    assert!(
        git(&m.mnt, &["commit", "-am", "edit doc"]).0,
        "commit failed"
    );
    let (okfp, patch, efp) = git(&m.mnt, &["format-patch", "-1", "--stdout"]);
    assert!(okfp, "format-patch failed: {efp}");
    assert!(
        git(&m.mnt, &["reset", "--hard", "HEAD~1"]).0,
        "reset failed"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("doc.md")).unwrap(),
        "original\n",
        "reset must restore the original"
    );
    let (okam, _o, eam) = git_stdin(&m.mnt, &["am"], patch.as_bytes());
    assert!(okam, "git am failed: {eam}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("doc.md")).unwrap(),
        "amended body\n",
        "am must re-apply the patched content"
    );
    assert_eq!(git(&m.mnt, &["log", "-1", "--pretty=%s"]).1, "edit doc");
}

#[test]
fn git_notes_attach_and_show() {
    let (m, _r) = Mounted::new(&[("a.txt", b"x\n")]);
    let (okadd, _o, e) = git(&m.mnt, &["notes", "add", "-m", "a useful note", "HEAD"]);
    assert!(okadd, "notes add failed: {e}");
    assert_eq!(
        git(&m.mnt, &["notes", "show", "HEAD"]).1,
        "a useful note",
        "the note must round-trip through refs/notes"
    );
}

#[test]
fn git_replace_substitutes_an_object() {
    let (m, _r) = Mounted::new(&[("a.txt", b"v1\n")]);
    let (_, c1, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    std::fs::write(m.mnt.join("a.txt"), b"v2\n").unwrap();
    assert!(git(&m.mnt, &["commit", "-am", "v2"]).0, "commit v2 failed");
    let (_, c2, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    // Replace c1 with c2: resolving c1 now yields c2's content.
    let (okr, _o, e) = git(&m.mnt, &["replace", &c1, &c2]);
    assert!(okr, "git replace failed: {e}");
    let (_, listed, _) = git(&m.mnt, &["replace", "-l"]);
    assert!(
        listed.contains(&c1[..8]),
        "replace ref must be listed: {listed}"
    );
    assert_eq!(
        git(&m.mnt, &["show", &format!("{c1}:a.txt")]).1,
        "v2",
        "the replaced object resolves to the replacement's content"
    );
}

#[test]
fn cherry_pick_a_commit_range() {
    let (m, _r) = Mounted::new(&[("base.txt", b"base\n")]);
    // A feature branch with TWO commits adding two files.
    assert!(git(&m.mnt, &["switch", "-c", "feature"]).0, "switch -c");
    std::fs::write(m.mnt.join("one.txt"), b"1\n").unwrap();
    git(&m.mnt, &["add", "one.txt"]);
    assert!(git(&m.mnt, &["commit", "-m", "add one"]).0, "commit one");
    std::fs::write(m.mnt.join("two.txt"), b"2\n").unwrap();
    git(&m.mnt, &["add", "two.txt"]);
    assert!(git(&m.mnt, &["commit", "-m", "add two"]).0, "commit two");

    // Back on main, advance it, then cherry-pick the whole feature range.
    assert!(git(&m.mnt, &["switch", "main"]).0, "switch main");
    std::fs::write(m.mnt.join("base.txt"), b"base+main\n").unwrap();
    assert!(
        git(&m.mnt, &["commit", "-am", "main moves"]).0,
        "main commit"
    );
    let (okcp, _o, e) = git(&m.mnt, &["cherry-pick", "main..feature"]);
    assert!(okcp, "cherry-pick range failed: {e}");

    assert_eq!(
        std::fs::read_to_string(m.mnt.join("one.txt")).unwrap(),
        "1\n"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("two.txt")).unwrap(),
        "2\n"
    );
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "clean after a cherry-pick range");
}

#[test]
fn tag_describe_and_archive() {
    let (m, _r) = Mounted::new(&[("v.txt", b"content\n"), ("src/lib.rs", b"pub fn f() {}\n")]);
    assert!(
        git(&m.mnt, &["tag", "-a", "v1.0", "-m", "release one"]).0,
        "annotated tag failed"
    );
    assert_eq!(
        git(&m.mnt, &["describe", "--tags"]).1,
        "v1.0",
        "describe must see the tag"
    );

    // `git archive` reads every tracked blob (faulting them over the promisor)
    // and streams a tarball — it must succeed and contain the tracked paths.
    let out = Command::new("git")
        .arg("-C")
        .arg(&m.mnt)
        .args(["archive", "--format=tar", "HEAD"])
        .output()
        .expect("git archive");
    assert!(out.status.success(), "git archive failed");
    let tar = out.stdout;
    assert!(!tar.is_empty(), "archive produced no output");
    // tar stores file names verbatim; both tracked paths must appear.
    let hay = String::from_utf8_lossy(&tar);
    assert!(hay.contains("v.txt"), "archive missing v.txt");
    assert!(hay.contains("src/lib.rs"), "archive missing src/lib.rs");
}

#[test]
fn restore_staged_unstages_without_touching_the_worktree() {
    let (m, _r) = Mounted::new(&[("f.txt", b"committed\n")]);
    std::fs::write(m.mnt.join("f.txt"), b"edited\n").unwrap();
    assert!(git(&m.mnt, &["add", "f.txt"]).0, "add failed");
    // The edit is staged (diff --cached --quiet exits nonzero when staged).
    assert!(!git(&m.mnt, &["diff", "--cached", "--quiet"]).0);
    // `restore --staged` unstages it; the working-tree bytes stay edited.
    let (ok, _o, e) = git(&m.mnt, &["restore", "--staged", "f.txt"]);
    assert!(ok, "restore --staged failed: {e}");
    assert!(
        git(&m.mnt, &["diff", "--cached", "--quiet"]).0,
        "index must match HEAD again after restore --staged"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("f.txt")).unwrap(),
        "edited\n",
        "restore --staged must NOT change the working tree"
    );
}
