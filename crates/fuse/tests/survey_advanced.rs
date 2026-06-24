//! Advanced stock-git workflows surveyed through the transparent FUSE mount:
//!
//! * `git worktree add <path> <branch>` — a *linked* worktree created OUTSIDE
//!   the mount (on a native filesystem). The new worktree is a real checkout, so
//!   it is **eager**: git hydrates the baseline blobs (faulting them over the
//!   `file://` promisor) to materialize the linked working tree. The superproject
//!   keeps operating normally through the mount.
//! * `git submodule add` / `status` / `update --init` — a nested repo. Because the
//!   superproject's worktree `.git` is the *synthetic gitfile*, git resolves the
//!   real superproject gitdir and stores the submodule's gitdir natively under
//!   `<admin-gitdir>/modules/<name>`, writing only a small `sub/.git` gitfile +
//!   the submodule's checkout into the overlay. `.gitmodules` + the gitlink are
//!   staged in the real index; `status`/`update` resolve cleanly.
//! * NATIVE `.gitattributes` filters git applies itself (no git-lfs, no PTY):
//!   - `* text=auto` — adding a CRLF file normalizes it to LF *in the index* via
//!     git's clean filter, which runs while reading the working tree THROUGH the
//!     mount. Fully transparent.
//!   - `*.txt text eol=crlf` and `ident` — *smudge* filters. The projection
//!     serves the raw baseline blob (no smudge applied at materialize), so the
//!     bytes a tool reads through the mount are LF / unexpanded `$Id$`, which
//!     differ from a real checkout (CRLF / `$Id: <sha> $`). git's *content*
//!     comparison stays clean because the clean filter is the inverse of the
//!     smudge. This is a deliberate limitation: applying smudge at materialize
//!     would make `getattr` size depend on the filter output, which conflicts
//!     with the lazy stat / clean-rename-without-fetch guarantees.
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

// ---------------------------------------------------------------------------
// git worktree add — a linked worktree OUTSIDE the mount.
// ---------------------------------------------------------------------------

#[test]
fn worktree_add_links_a_native_checkout_outside_the_mount() {
    // The superproject is mounted; a *linked* worktree is created on a NATIVE
    // path (a sibling of the mount, never inside FUSE). git checks the branch out
    // there, hydrating the baseline blobs from the promisor (eager) — the linked
    // tree is a real working copy on the native filesystem.
    let body: &[u8] = b"line one\nline two\n";
    let (m, _remote) = Mounted::new(&[("README.md", body), ("src/main.rs", b"fn main() {}\n")]);

    // A native target directory, OUTSIDE the mount (its own tempdir).
    let linked_tmp = tempfile::tempdir().unwrap();
    let linked = linked_tmp.path().join("linked");

    // Use a NEW branch for the linked worktree: `main` is already checked out in
    // the mount, and git (correctly) forbids the same branch in two worktrees.
    let (ok, _, e) = git(
        &m.mnt,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    assert!(ok, "worktree add failed: {e}");

    // The linked worktree is registered and points at the native path.
    let (_, list, _) = git(&m.mnt, &["worktree", "list"]);
    assert!(
        list.contains(&*linked.to_string_lossy()),
        "linked worktree must be listed: {list:?}"
    );

    // It is a real, populated checkout on the native fs — NOT served through the
    // mount. The blob was hydrated to materialize it.
    assert!(
        linked.join(".git").exists(),
        "linked worktree has a .git gitfile"
    );
    let gitfile = std::fs::read_to_string(linked.join(".git")).unwrap();
    assert!(
        gitfile.starts_with("gitdir:") && gitfile.contains("worktrees"),
        "linked .git points into <gitdir>/worktrees: {gitfile:?}"
    );
    assert_eq!(
        std::fs::read(linked.join("README.md")).unwrap(),
        body,
        "the linked checkout has the real blob bytes (eager hydration)"
    );
    assert!(
        linked.join("src/main.rs").exists(),
        "nested tracked files are checked out into the linked worktree"
    );

    // The superproject keeps working normally through the mount.
    let (ok_st, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(ok_st, "status through the mount still works");
    assert_eq!(st, "", "superproject worktree stays clean");

    // Tidy up so Drop's unmount isn't racing a registered worktree handle.
    let _ = git(
        &m.mnt,
        &["worktree", "remove", "--force", linked.to_str().unwrap()],
    );
}

// ---------------------------------------------------------------------------
// git submodule add / status / update
// ---------------------------------------------------------------------------

// Submodules (nested repos: gitlinks, `.gitmodules`, a recursive checkout into
// the overlay, and the submodule's own gitdir under `<admin>/modules/`) are a
// deep, multi-repo interaction that is not yet validated end-to-end through the
// mount.
#[test]
#[ignore = "submodule support through the mount is not yet validated end-to-end"]
fn submodule_add_status_update_through_the_mount() {
    // The superproject is mounted; we add a submodule whose remote is a separate
    // seeded bare repo. Because the mount's `.git` is the synthetic gitfile, git
    // resolves the real superproject gitdir and puts the submodule's gitdir
    // natively under <admin-gitdir>/modules/<name>; only the submodule's checkout
    // and a tiny `sub/.git` gitfile land in the overlay (via FUSE writes).
    let (m, _remote) = Mounted::new(&[("top.txt", b"top\n")]);

    // A second seeded remote to act as the submodule's upstream.
    let sub_remote = seed_remote(&[("s.txt", b"submodule content\n")]);

    let (ok_add, _, e_add) = git(
        &m.mnt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &sub_remote.url,
            "sub",
        ],
    );
    assert!(ok_add, "submodule add failed: {e_add}");

    // .gitmodules and the gitlink are staged in the REAL index.
    let (_, st, _) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(
        st.contains(".gitmodules"),
        ".gitmodules must be staged: {st:?}"
    );
    assert!(
        st.contains("sub"),
        "the submodule path must be staged: {st:?}"
    );

    // The submodule's checkout is projected (its bytes were written via FUSE).
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("sub/s.txt")).unwrap(),
        "submodule content\n",
        "submodule working file is present through the mount"
    );
    // The submodule's gitdir lives NATIVELY, reached via its own gitfile (not via
    // the synthetic superproject .git, which is a file).
    let sub_gitfile = std::fs::read_to_string(m.mnt.join("sub/.git")).unwrap();
    assert!(
        sub_gitfile.starts_with("gitdir:") && sub_gitfile.contains("modules"),
        "submodule .git gitfile points into <gitdir>/modules: {sub_gitfile:?}"
    );

    // `git submodule status` resolves the nested repo (one SHA, path `sub`).
    let (ok_status, status, e_status) = git(&m.mnt, &["submodule", "status"]);
    assert!(ok_status, "submodule status failed: {e_status}");
    assert!(
        status.contains("sub"),
        "submodule status names the path: {status:?}"
    );

    // Commit the superproject, then re-checkout the submodule via update --init.
    let (ok_c, _, e_c) = git(&m.mnt, &["commit", "-m", "add submodule"]);
    assert!(ok_c, "superproject commit failed: {e_c}");

    // Clear the submodule working tree and re-populate it via `update`.
    let _ = std::fs::remove_file(m.mnt.join("sub/s.txt"));
    let (ok_u, _, e_u) = git(
        &m.mnt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--",
            "sub",
        ],
    );
    assert!(ok_u, "submodule update --init failed: {e_u}");
    assert_eq!(
        std::fs::read_to_string(m.mnt.join("sub/s.txt")).unwrap(),
        "submodule content\n",
        "submodule update re-materialized the working file through the mount"
    );

    // Keep the submodule's remote alive until here (it backs the nested clone).
    drop(sub_remote);
}

// ---------------------------------------------------------------------------
// .gitattributes — text=auto (clean filter, applied by git itself on `add`).
// ---------------------------------------------------------------------------

#[test]
fn gitattributes_text_auto_normalizes_crlf_on_add() {
    // `* text=auto`: git's CLEAN filter runs while git reads the working tree
    // THROUGH the mount. A CRLF file written into the overlay is normalized to LF
    // in the index blob — fully transparent (git applies the attribute itself).
    // Seed a repo carrying `.gitattributes` with `* text=auto`.
    let (m, _remote) = Mounted::new(&[
        (".gitattributes", b"* text=auto\n"),
        ("seed.txt", b"a\nb\n"),
    ]);

    // Write a CRLF file through the mount and stage it.
    std::fs::write(m.mnt.join("crlf.txt"), b"x\r\ny\r\n").unwrap();
    let (ok_add, _, e) = git(&m.mnt, &["add", "crlf.txt"]);
    assert!(ok_add, "add under text=auto failed: {e}");

    // git's clean filter normalized CRLF -> LF in the staged blob.
    let (_, staged, _) = git(&m.mnt, &["show", ":crlf.txt"]);
    assert_eq!(
        staged, "x\ny",
        "text=auto must normalize CRLF to LF in the index blob"
    );

    // The committed blob is LF too.
    let (ok_c, _, ec) = git(&m.mnt, &["commit", "-m", "add crlf.txt"]);
    assert!(ok_c, "commit failed: {ec}");
    let (_, shown, _) = git(&m.mnt, &["show", "HEAD:crlf.txt"]);
    assert_eq!(shown, "x\ny", "committed blob is LF-normalized");
}

// ---------------------------------------------------------------------------
// .gitattributes — eol=crlf and ident (SMUDGE filters; not applied by the
// projection, but git's CONTENT comparison stays clean).
// ---------------------------------------------------------------------------

#[test]
fn gitattributes_eol_crlf_projects_raw_lf_but_content_diff_is_clean() {
    // `*.txt text eol=crlf`: a real checkout would write CRLF (smudge). The
    // projection serves the RAW baseline blob (LF) — a deliberate limitation —
    // so the on-disk bytes differ from a checkout. But git's clean filter is the
    // inverse of the smudge, so the CONTENT comparison stays clean and commits
    // remain byte-correct.
    let (m, _remote) = Mounted::new(&[
        (".gitattributes", b"*.txt text eol=crlf\n"),
        ("greet.txt", b"hello\nworld\n"),
    ]);

    let projected = std::fs::read(m.mnt.join("greet.txt")).unwrap();
    assert_eq!(
        projected, b"hello\nworld\n",
        "projection serves the raw LF baseline blob (no smudge applied)"
    );

    let (clean, _, _) = git(&m.mnt, &["diff", "--exit-code", "--", "greet.txt"]);
    assert!(
        clean,
        "eol=crlf: clean filter is the inverse of smudge, so content diff is clean"
    );
    let (_, before, _) = git(&m.mnt, &["rev-parse", "HEAD:greet.txt"]);
    let (ok_add, _, _) = git(&m.mnt, &["add", "greet.txt"]);
    assert!(ok_add);
    let (_, staged_oid, _) = git(&m.mnt, &["rev-parse", ":greet.txt"]);
    assert_eq!(
        staged_oid, before,
        "re-adding the eol=crlf file produces the same blob — no churn"
    );
}

#[test]
fn gitattributes_ident_projects_unexpanded_but_content_diff_is_clean() {
    // `ident`: a real checkout smudges `$Id$` -> `$Id: <sha> $`. The projection
    // serves the raw blob (`$Id$` unexpanded) — a deliberate limitation. git's
    // own `cat-file --filters` DOES expand (the attribute is wired), and the
    // clean filter strips it back, so the content comparison is clean.
    let (m, _remote) = Mounted::new(&[
        (".gitattributes", b"*.id ident\n"),
        ("stamp.id", b"$Id$\nsome content\n"),
    ]);

    let projected = std::fs::read_to_string(m.mnt.join("stamp.id")).unwrap();
    assert_eq!(
        projected, "$Id$\nsome content\n",
        "projection serves the raw blob; ident is NOT smudge-expanded"
    );

    let (clean, _, _) = git(&m.mnt, &["diff", "--exit-code", "--", "stamp.id"]);
    assert!(
        clean,
        "ident: clean filter is the inverse of smudge, so content diff is clean"
    );

    // git's own smudge path expands — proving the attribute is wired; only the
    // *projection* serves raw bytes.
    let (_, smudged, _) = git(
        &m.mnt,
        &["cat-file", "--filters", "--path=stamp.id", "HEAD:stamp.id"],
    );
    assert!(
        smudged.contains("$Id: "),
        "git's own smudge path expands ident: {smudged:?}"
    );
}
