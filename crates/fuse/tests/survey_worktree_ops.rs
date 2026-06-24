//! Working-tree-shaping stock-git commands exercised through the transparent
//! FUSE mount, against the real index/overlay/baseline:
//!
//! * `git clean -fd` — remove untracked files AND untracked directories that
//!   were created (through the mount) directly into the durable overlay.
//! * `git restore <path>` and `git checkout -- <path>` — discard a working-tree
//!   edit of a baseline file (the clean blob faults in once over the promisor,
//!   then the overlay copy is overwritten with the restored bytes).
//! * DIRECTORY RENAME (formerly the R5 gap, now fixed): a whole-directory
//!   rename, whether via `std::fs::rename` or `git mv <dir> <newdir>`, moves the
//!   entire subtree — `Projection::rename` recurses, re-keying overlay
//!   descendants and re-pointing baseline descendants as base-refs, then
//!   tombstoning the source. Metadata-only, no blob fetch (§29).
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

// ------------------------------------------------------------------------
// git clean -fd
// ------------------------------------------------------------------------

#[test]
fn git_clean_fd_removes_untracked_files_and_dirs() {
    // Seed one tracked file; everything else we add is untracked overlay state.
    let (m, _remote) = Mounted::new(&[("tracked.txt", b"keep me\n")]);

    // Create an untracked top-level file and an untracked directory holding a
    // file — all written through the mount, i.e. straight into the overlay
    // (File + Dir + File overlay entries; no baseline involvement).
    std::fs::write(m.mnt.join("loose.txt"), b"untracked loose file\n").unwrap();
    std::fs::create_dir(m.mnt.join("scratch")).unwrap();
    std::fs::write(
        m.mnt.join("scratch").join("inner.txt"),
        b"untracked inner\n",
    )
    .unwrap();

    // git sees them as untracked.
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(
        st.contains("loose.txt"),
        "loose.txt must show untracked: {st:?}"
    );
    assert!(
        st.contains("scratch/") || st.contains("scratch"),
        "untracked dir must show: {st:?}"
    );

    // `-fd` is required to remove untracked directories as well as files.
    let (ok, _, e) = git(&m.mnt, &["clean", "-fd"]);
    assert!(ok, "git clean -fd failed: {e}");

    // Untracked file, dir, and the nested file are all gone from the projection.
    assert!(
        !m.mnt.join("loose.txt").exists(),
        "untracked file survived clean"
    );
    assert!(
        !m.mnt.join("scratch").exists(),
        "untracked dir survived clean"
    );
    assert!(
        !m.mnt.join("scratch").join("inner.txt").exists(),
        "file inside untracked dir survived clean"
    );

    // The tracked file is untouched and the tree is clean.
    assert!(
        m.mnt.join("tracked.txt").exists(),
        "clean removed a tracked file"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("tracked.txt")).unwrap(),
        "keep me\n"
    );
    let (_, st2, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(
        st2, "",
        "tree must be clean after `git clean -fd`, got {st2:?}"
    );
}

// ------------------------------------------------------------------------
// git restore <path>  /  git checkout -- <path>
// ------------------------------------------------------------------------

#[test]
fn git_restore_discards_a_working_tree_edit() {
    // criterion: `git restore <path>` reverts an edited baseline file to HEAD.
    let (m, _remote) = Mounted::new(&[("doc.txt", b"committed content\n")]);

    // Edit the baseline file through the mount (copy-up into the overlay).
    std::fs::write(m.mnt.join("doc.txt"), b"local uncommitted edit\n").unwrap();
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(st.contains("doc.txt"), "edit must show as modified: {st:?}");

    // Restore from the index/HEAD; the clean blob faults in over the promisor
    // and the overlay copy is overwritten with the restored bytes.
    let (ok, _, e) = git(&m.mnt, &["restore", "doc.txt"]);
    assert!(ok, "git restore failed: {e}");

    assert_eq!(
        std::fs::read_to_string(m.mnt.join("doc.txt")).unwrap(),
        "committed content\n",
        "git restore must put back the committed bytes"
    );
    let (_, st2, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st2, "", "tree must be clean after restore, got {st2:?}");
}

#[test]
fn git_checkout_dashdash_discards_a_working_tree_edit() {
    // The older `git checkout -- <path>` spelling must behave identically.
    let (m, _remote) = Mounted::new(&[("doc.txt", b"committed content\n")]);

    std::fs::write(m.mnt.join("doc.txt"), b"scratch edit\n").unwrap();
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(st.contains("doc.txt"), "edit must show as modified: {st:?}");

    let (ok, _, e) = git(&m.mnt, &["checkout", "--", "doc.txt"]);
    assert!(ok, "git checkout -- <path> failed: {e}");

    assert_eq!(
        std::fs::read_to_string(m.mnt.join("doc.txt")).unwrap(),
        "committed content\n",
        "git checkout -- must put back the committed bytes"
    );
    let (_, st2, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert_eq!(st2, "", "tree must be clean after checkout --, got {st2:?}");
}

// ------------------------------------------------------------------------
// DIRECTORY RENAME — the known R5 gap. These tests pin the CURRENT behavior:
// the rename fails, and the repository stays consistent (the directory and its
// contents remain readable at the ORIGINAL path; the destination is absent).
// ------------------------------------------------------------------------

#[test]
fn fs_rename_of_a_directory_moves_the_subtree() {
    // A whole-directory rename via the raw `rename(2)` syscall through the mount.
    // `Projection::rename` now moves the entire subtree (baseline + overlay,
    // nested) with no blob fetch (§29). R5 fixed.
    let (m, _remote) = Mounted::new(&[("d/a.txt", b"alpha\n"), ("d/sub/b.txt", b"beta\n")]);
    // An untracked overlay file inside the dir, to prove overlay descendants move.
    std::fs::write(m.mnt.join("d").join("c.txt"), b"gamma\n").unwrap();

    std::fs::rename(m.mnt.join("d"), m.mnt.join("renamed")).expect("directory rename");

    assert!(!m.mnt.join("d").exists(), "source dir must be gone");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("renamed/a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("renamed/sub/b.txt")).unwrap(),
        "beta\n",
        "nested baseline file moved"
    );
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("renamed/c.txt")).unwrap(),
        "gamma\n",
        "overlay file moved"
    );
}

#[test]
fn git_mv_of_a_directory_moves_tracked_paths() {
    // `git mv <dir> <newdir>` renames the directory then restages. Through the
    // mount the tracked paths move to the destination and a commit records the
    // new layout. R5 fixed.
    let (m, _remote) = Mounted::new(&[("pkg/one.rs", b"// one\n"), ("pkg/two.rs", b"// two\n")]);

    let (ok, _out, err) = git(&m.mnt, &["mv", "pkg", "lib"]);
    assert!(ok, "git mv of a whole directory should now succeed: {err}");

    let (_, tracked, _) = git(&m.mnt, &["ls-files"]);
    assert!(
        tracked.contains("lib/one.rs") && tracked.contains("lib/two.rs"),
        "tracked paths must move to the destination: {tracked:?}"
    );
    assert!(
        !tracked.contains("pkg/one.rs") && !tracked.contains("pkg/two.rs"),
        "source paths must be gone: {tracked:?}"
    );
    assert!(!m.mnt.join("pkg").exists(), "source dir must be gone");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("lib/one.rs")).unwrap(),
        "// one\n"
    );

    let (okc, _, ce) = git(&m.mnt, &["commit", "-m", "move pkg -> lib"]);
    assert!(okc, "commit after the directory move failed: {ce}");
}

#[test]
fn git_mv_of_a_single_file_still_works() {
    // Contrast with the directory case: a single-FILE `git mv` is a file
    // rename, which the projection DOES support (clean base-ref move, no blob
    // fetch). This guards that the R5 gap is scoped to directories only.
    let (m, _remote) = Mounted::new(&[("old.txt", b"contents\n"), ("other.txt", b"x\n")]);

    let (ok, _out, err) = git(&m.mnt, &["mv", "old.txt", "new.txt"]);
    assert!(ok, "git mv of a single file failed: {err}");

    assert!(!m.mnt.join("old.txt").exists(), "old path must be gone");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("new.txt")).unwrap(),
        "contents\n",
        "content must survive the single-file rename"
    );
    let (_, tracked, _) = git(&m.mnt, &["ls-files"]);
    assert!(
        tracked.contains("new.txt"),
        "new path must be tracked: {tracked:?}"
    );
    assert!(
        !tracked.contains("old.txt"),
        "old path must be untracked: {tracked:?}"
    );
}
