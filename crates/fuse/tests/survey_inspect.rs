//! Read-only / inspection stock-git commands exercised through the transparent
//! FUSE mount. This cluster surveys commands that *search* and *read* tracked
//! content and history rather than mutate the working tree:
//!
//! * `git grep` — searching tracked content, both the projected working tree
//!   (blobs faulted in through FUSE) and a revision (`git grep <pat> HEAD`,
//!   blobs faulted in over the `file://` promisor from the object store).
//! * `git blame` — line-history of one file (reads the file blob + the diff
//!   blobs of the commits that touched it).
//! * `git bisect` — `start` / `good` / `bad` then `bisect run` a trivial test
//!   over the mount, then `reset` (each step checks out a commit, writing the
//!   working tree through FUSE and faulting blobs lazily).
//! * `git log -p` and `git show` — read-only diffs over the mount (blob faults
//!   over the promisor; the working tree is never mutated).
//!
//! Every command here must behave exactly as it would against a normal
//! checkout. Real `/dev/fuse` mount — runs under `--features fuse`.
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

// ---------------------------------------------------------------------------
// git grep
// ---------------------------------------------------------------------------

#[test]
fn grep_working_tree_and_revision() {
    // `git grep` over the projected working tree must find matches by hydrating
    // the relevant blobs through FUSE; `git grep <pat> HEAD` must find them by
    // faulting blobs out of the object store over the file:// promisor. Both
    // must behave exactly as a normal checkout.
    let (m, _remote) = Mounted::new(&[
        ("src/a.rs", b"fn alpha() { let needle = 1; }\n"),
        ("src/b.rs", b"fn beta() { let haystack = 2; }\n"),
        ("docs/readme.md", b"nothing to see here\n"),
    ]);

    // Working-tree grep: -n for line numbers, default searches the worktree.
    let (ok, out, e) = git(&m.mnt, &["grep", "-n", "needle"]);
    assert!(ok, "grep working tree failed: {e}");
    assert!(
        out.contains("src/a.rs") && out.contains("needle"),
        "grep must locate the match in src/a.rs: {out:?}"
    );
    assert!(
        !out.contains("src/b.rs"),
        "grep must not report a non-matching file: {out:?}"
    );

    // A pattern that appears in two files: both paths must be listed (-l).
    let (ok2, out2, e2) = git(&m.mnt, &["grep", "-l", "fn "]);
    assert!(ok2, "grep -l failed: {e2}");
    assert!(
        out2.contains("src/a.rs") && out2.contains("src/b.rs"),
        "grep -l must list both source files: {out2:?}"
    );

    // Grep against a revision reads blobs from the object store (promisor lazy
    // fetch), not the working tree.
    let (ok3, out3, e3) = git(&m.mnt, &["grep", "-n", "haystack", "HEAD"]);
    assert!(ok3, "grep against HEAD failed: {e3}");
    assert!(
        out3.contains("b.rs") && out3.contains("haystack"),
        "grep HEAD must find the revision match: {out3:?}"
    );

    // A pattern present in no file yields a clean nonzero (grep's "no match"),
    // exactly as a normal checkout — the repo stays consistent.
    let (ok4, out4, _) = git(&m.mnt, &["grep", "definitely-absent-token"]);
    assert!(!ok4, "grep with no match returns nonzero");
    assert_eq!(out4, "", "no-match grep prints nothing");
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "grep must not dirty the working tree: {st:?}");
}

// ---------------------------------------------------------------------------
// git blame
// ---------------------------------------------------------------------------

#[test]
fn blame_attributes_lines_to_commits() {
    // Build a small two-commit history so blame has something to attribute, then
    // blame the file through the mount. Blame reads the file blob and the diff
    // blobs of the touching commits (all faulted lazily) and must agree with a
    // normal checkout.
    let (m, _remote) = Mounted::new(&[("poem.txt", b"first line\nsecond line\n")]);

    // Second commit edits one line and adds another.
    std::fs::write(
        m.mnt.join("poem.txt"),
        b"first line\nSECOND EDITED\nthird line\n",
    )
    .unwrap();
    let (ok_c, _, ce) = git(&m.mnt, &["commit", "-am", "edit and extend the poem"]);
    assert!(ok_c, "second commit failed: {ce}");

    // Porcelain blame is stable to parse: each content line is preceded by a
    // header line beginning with the 40-hex commit it is attributed to.
    let (ok, out, e) = git(&m.mnt, &["blame", "--porcelain", "poem.txt"]);
    assert!(ok, "blame failed: {e}");
    assert!(
        out.contains("first line") && out.contains("SECOND EDITED") && out.contains("third line"),
        "blame must reproduce every line of the file: {out:?}"
    );

    // The unchanged first line is attributed to the initial commit; the edited
    // and added lines are attributed to HEAD. Confirm both commits appear.
    let (_, head, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    let (_, parent, _) = git(&m.mnt, &["rev-parse", "HEAD~1"]);
    assert!(
        out.contains(&head[..8]),
        "blame must attribute edited lines to HEAD {head}: {out:?}"
    );
    assert!(
        out.contains(&parent[..8]),
        "blame must attribute the untouched line to the parent {parent}: {out:?}"
    );

    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "blame must not dirty the working tree: {st:?}");
}

// ---------------------------------------------------------------------------
// git bisect
// ---------------------------------------------------------------------------

#[test]
fn bisect_run_finds_the_first_bad_commit() {
    // Build a linear history where a "marker" file gains a bad token at a known
    // commit. `git bisect run` checks out commits (writing the working tree
    // through FUSE, faulting blobs lazily) and runs a trivial test that greps
    // the marker. bisect must converge on the first bad commit, and `reset` must
    // return us to the branch tip with a clean tree.
    let (m, _remote) = Mounted::new(&[("marker.txt", b"good\n")]);

    // Record the first (good) commit.
    let (_, good, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    // A few more good commits.
    for i in 0..2 {
        std::fs::write(m.mnt.join("marker.txt"), format!("good {i}\n")).unwrap();
        let (ok, _, e) = git(&m.mnt, &["commit", "-am", &format!("still good {i}")]);
        assert!(ok, "good commit {i} failed: {e}");
    }
    // The commit that introduces the regression.
    std::fs::write(m.mnt.join("marker.txt"), b"good\nBADTOKEN\n").unwrap();
    let (ok_b, _, be) = git(&m.mnt, &["commit", "-am", "introduce regression"]);
    assert!(ok_b, "bad commit failed: {be}");
    let (_, first_bad, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    // One more commit after the regression so the bad range has depth.
    std::fs::write(m.mnt.join("marker.txt"), b"good\nBADTOKEN\nmore\n").unwrap();
    let (ok_a, _, ae) = git(&m.mnt, &["commit", "-am", "more work after regression"]);
    assert!(ok_a, "post-regression commit failed: {ae}");
    let (_, tip, _) = git(&m.mnt, &["rev-parse", "HEAD"]);

    // Write a trivial test script: exit nonzero (bad) iff the marker contains
    // BADTOKEN. The script greps the checked-out working-tree file through FUSE.
    let script = m._tmp.path().join("test.sh");
    std::fs::write(
        &script,
        b"#!/bin/sh\nif grep -q BADTOKEN marker.txt; then exit 1; else exit 0; fi\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let (ok_s, _, se) = git(&m.mnt, &["bisect", "start"]);
    assert!(ok_s, "bisect start failed: {se}");
    let (ok_bad, _, bbe) = git(&m.mnt, &["bisect", "bad", &tip]);
    assert!(ok_bad, "bisect bad failed: {bbe}");
    let (ok_good, _, gge) = git(&m.mnt, &["bisect", "good", &good]);
    assert!(ok_good, "bisect good failed: {gge}");

    let (ok_run, run_out, re) = git(&m.mnt, &["bisect", "run", script.to_str().unwrap()]);
    assert!(ok_run, "bisect run failed: {re}\n{run_out}");
    assert!(
        run_out.contains(&first_bad[..8]) || run_out.contains(&first_bad),
        "bisect run must blame the first bad commit {first_bad}: {run_out:?}"
    );
    assert!(
        run_out.contains("is the first bad commit"),
        "bisect run must announce the first bad commit: {run_out:?}"
    );

    // Reset returns to the branch tip with a clean, consistent tree.
    let (ok_reset, _, rse) = git(&m.mnt, &["bisect", "reset"]);
    assert!(ok_reset, "bisect reset failed: {rse}");
    let (_, after, _) = git(&m.mnt, &["rev-parse", "HEAD"]);
    assert_eq!(after, tip, "bisect reset must return HEAD to the tip");
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "tree must be clean after bisect reset: {st:?}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("marker.txt")).unwrap(),
        "good\nBADTOKEN\nmore\n",
        "working tree must be restored to the tip content after reset"
    );
}

// ---------------------------------------------------------------------------
// git log -p
// ---------------------------------------------------------------------------

#[test]
fn log_p_shows_patches_without_mutating_the_tree() {
    // `git log -p` walks history and emits a patch per commit, faulting the
    // before/after blobs of each touched path over the promisor. It is purely
    // read-only and must match a normal checkout.
    let (m, _remote) = Mounted::new(&[("change.txt", b"one\n")]);
    std::fs::write(m.mnt.join("change.txt"), b"one\ntwo\n").unwrap();
    let (ok_c, _, ce) = git(&m.mnt, &["commit", "-am", "append two"]);
    assert!(ok_c, "commit failed: {ce}");

    let (ok, out, e) = git(&m.mnt, &["log", "-p"]);
    assert!(ok, "log -p failed: {e}");
    // The added line shows as a `+two` hunk; both commit subjects appear.
    assert!(
        out.contains("+two"),
        "log -p must show the added line: {out:?}"
    );
    assert!(
        out.contains("append two"),
        "log -p must show the commit subject: {out:?}"
    );
    assert!(
        out.contains("diff --git"),
        "log -p must include a unified diff header: {out:?}"
    );

    // -p with a pathspec restricts to one path and still produces a patch.
    let (ok2, out2, e2) = git(&m.mnt, &["log", "-p", "--", "change.txt"]);
    assert!(ok2, "log -p -- path failed: {e2}");
    assert!(
        out2.contains("+two"),
        "path-restricted log -p must show the hunk: {out2:?}"
    );

    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "log -p must not dirty the working tree: {st:?}");
}

// ---------------------------------------------------------------------------
// git show
// ---------------------------------------------------------------------------

#[test]
fn show_renders_commit_diff_and_blob() {
    // `git show <commit>` renders the commit's diff; `git show <commit>:<path>`
    // emits a blob's bytes (faulted over the promisor). Both are read-only.
    let (m, _remote) = Mounted::new(&[("file.txt", b"alpha\n")]);
    std::fs::write(m.mnt.join("file.txt"), b"alpha\nbeta\n").unwrap();
    let (ok_c, _, ce) = git(&m.mnt, &["commit", "-am", "add beta"]);
    assert!(ok_c, "commit failed: {ce}");

    // show HEAD: commit metadata + diff.
    let (ok, out, e) = git(&m.mnt, &["show", "HEAD"]);
    assert!(ok, "show HEAD failed: {e}");
    assert!(
        out.contains("add beta"),
        "show must include the subject: {out:?}"
    );
    assert!(
        out.contains("+beta"),
        "show must include the added-line hunk: {out:?}"
    );

    // show HEAD:<path>: the blob content at HEAD (faulted from the store).
    let (ok2, out2, e2) = git(&m.mnt, &["show", "HEAD:file.txt"]);
    assert!(ok2, "show blob failed: {e2}");
    assert_eq!(
        out2, "alpha\nbeta",
        "show blob must emit the file content at HEAD"
    );

    // show the parent's version of the blob (the original baseline blob,
    // faulted lazily over the promisor).
    let (ok3, out3, e3) = git(&m.mnt, &["show", "HEAD~1:file.txt"]);
    assert!(ok3, "show parent blob failed: {e3}");
    assert_eq!(out3, "alpha", "show must emit the parent blob content");

    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st, "", "show must not dirty the working tree: {st:?}");
}
