//! Repository-maintenance commands run by **stock git** through the transparent
//! FUSE mount, against the real admin object store.
//!
//! Every command in this cluster operates on the admin gitdir's object store,
//! NOT the projected working tree: `git fsck`, `git gc`, `git repack -ad`,
//! `git maintenance run`, and `git prune`. The admin repo is a
//! `--filter=blob:none` partial clone whose `origin` remote is configured as a
//! *promisor* (`remote.origin.promisor=true`); the HEAD-baseline blobs are
//! genuinely absent locally and faulted in lazily over the `file://` promisor.
//!
//! The interesting question for this cluster is whether these object-store
//! commands behave identically to a normal partial checkout under that promisor
//! contract — in particular whether `git fsck` PASSES (treating the missing
//! promisor blobs as expected) rather than reporting them as broken links. It
//! does: git knows the promisor remote, so missing blobs reachable from HEAD are
//! not errors. Each test asserts the command's exit status AND that the repo is
//! still usable afterward (`git status` / `git log` keep working, and the tree
//! stays clean because nothing in the working tree was touched).
//!
//! Real `/dev/fuse` mount — runs under `--features fuse`.
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
// Helpers shared by the maintenance tests.
// ---------------------------------------------------------------------------

/// Assert the repo is still usable through the mount after a maintenance run:
/// `git status` and `git log` both succeed, and — because none of these object-
/// store commands touch the projected working tree — `status` is still clean.
fn assert_repo_usable(m: &Mounted) {
    let (ok_st, st, est) = git(&m.mnt, &["status", "--porcelain"]);
    assert!(ok_st, "git status failed after maintenance: {est}");
    assert_eq!(
        st, "",
        "maintenance must not perturb the projected working tree: {st:?}"
    );
    let (ok_log, log, elog) = git(&m.mnt, &["log", "--oneline"]);
    assert!(ok_log, "git log failed after maintenance: {elog}");
    assert!(!log.is_empty(), "log should list at least the seed commit");
}

/// Confirm the admin store really is a blob:none promisor clone with the
/// HEAD-baseline blobs ABSENT locally — i.e. the maintenance commands below are
/// genuinely exercising the partial-clone path, not a fully-hydrated repo.
fn assert_baseline_blob_is_missing_locally(m: &Mounted) {
    // origin is configured as the promisor.
    let (_, promisor, _) = git(&m.mnt, &["config", "--get", "remote.origin.promisor"]);
    assert_eq!(promisor, "true", "origin must be a promisor remote");
    // The blob for the seed file is referenced by HEAD but not present locally.
    let (ok_ls, oid, _) = git(&m.mnt, &["rev-parse", "HEAD:seed.txt"]);
    assert!(ok_ls, "could not resolve HEAD:seed.txt");
    let out = Command::new("git")
        .arg("-C")
        .arg(&m.mnt)
        .args(["cat-file", "-e", &oid])
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()
        .expect("spawn cat-file");
    assert!(
        !out.status.success(),
        "baseline blob {oid} is present locally — expected it ABSENT under blob:none"
    );
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn fsck_passes_under_blob_none_promisor() {
    // The headline case: `git fsck` on a blob:none promisor clone PASSES and
    // does not report the (legitimately absent) HEAD-baseline blobs as missing
    // links — git treats objects reachable through a promisor remote as
    // expected-absent. This is the same behavior a normal partial checkout has.
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n"), ("dir/nested.txt", b"beta\n")]);
    assert_baseline_blob_is_missing_locally(&m);

    let (ok, _out, err) = git(&m.mnt, &["fsck"]);
    assert!(
        ok,
        "git fsck must pass under a blob:none promisor (missing promisor blobs \
         are not errors): {err}"
    );
    // fsck must NOT flag the absent baseline blobs as broken/missing links.
    assert!(
        !err.contains("missing blob") && !err.contains("broken link"),
        "fsck reported missing/broken promisor objects as errors: {err}"
    );
    assert_repo_usable(&m);
}

#[test]
fn fsck_connectivity_only_passes() {
    // A connectivity-only fsck (the variant gc runs internally) likewise treats
    // promisor-absent blobs as expected and exits 0. It may print informational
    // "dangling"/"unreachable" lines on stdout — those are not failures, so we
    // assert only on exit status.
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n")]);
    let (ok, _out, err) = git(&m.mnt, &["fsck", "--connectivity-only"]);
    assert!(
        ok,
        "fsck --connectivity-only must pass under the promisor: {err}"
    );
    assert_repo_usable(&m);
}

#[test]
fn gc_repacks_and_repo_stays_usable() {
    // `git gc` repacks/prunes the admin object store. Under the promisor it does
    // not try to fetch or delete the absent baseline blobs, exits 0, and leaves
    // the repo fully usable. Run it twice to prove idempotence (a second gc on
    // an already-packed store must also succeed).
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n"), ("dir/nested.txt", b"beta\n")]);

    let (ok1, _, err1) = git(&m.mnt, &["gc"]);
    assert!(ok1, "git gc failed: {err1}");
    // fsck after gc still passes — gc didn't corrupt the promisor contract.
    let (ok_fsck, _, efsck) = git(&m.mnt, &["fsck"]);
    assert!(ok_fsck, "fsck after gc failed: {efsck}");

    let (ok2, _, err2) = git(&m.mnt, &["gc"]);
    assert!(ok2, "second git gc failed: {err2}");
    assert_repo_usable(&m);
}

#[test]
fn repack_ad_rewrites_pack_and_repo_stays_usable() {
    // `git repack -ad` rewrites all packs into one and drops the redundant ones.
    // The promisor-absent blobs are not in any pack, so they are simply never
    // referenced by the repack; the command exits 0 and the repo stays usable.
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n"), ("dir/nested.txt", b"beta\n")]);
    assert_baseline_blob_is_missing_locally(&m);

    let (ok, _out, err) = git(&m.mnt, &["repack", "-ad"]);
    assert!(ok, "git repack -ad failed: {err}");
    // The pack directory now holds a (re)packed store and fsck still passes.
    let (ok_fsck, _, efsck) = git(&m.mnt, &["fsck"]);
    assert!(ok_fsck, "fsck after repack -ad failed: {efsck}");
    assert_repo_usable(&m);
}

#[test]
fn maintenance_run_succeeds_and_repo_stays_usable() {
    // `git maintenance run` runs the default maintenance tasks (commit-graph,
    // prefetch, loose-object/incremental-repack, etc.) against the admin store.
    // Under the promisor it completes without trying to materialize the absent
    // baseline blobs and leaves the repo usable.
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n"), ("dir/nested.txt", b"beta\n")]);

    let (ok, _out, err) = git(&m.mnt, &["maintenance", "run"]);
    assert!(ok, "git maintenance run failed: {err}");
    let (ok_fsck, _, efsck) = git(&m.mnt, &["fsck"]);
    assert!(ok_fsck, "fsck after maintenance run failed: {efsck}");
    assert_repo_usable(&m);
}

#[test]
fn prune_keeps_reachable_objects_and_repo_stays_usable() {
    // `git prune` removes unreachable loose objects. The HEAD-baseline blobs are
    // absent (nothing to prune there) and the reachable trees/commits are kept,
    // so prune is a no-op-ish success and the repo stays usable. We also create
    // an unreachable object (a dangling blob) and confirm prune does not break
    // the promisor contract.
    let (m, _remote) = Mounted::new(&[("seed.txt", b"alpha\n")]);

    // Write a dangling blob into the store (unreferenced by any ref).
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&m.mnt)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hash-object");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"dangling content\n")
        .expect("feed hash-object");
    let ho = child.wait_with_output().expect("wait hash-object");
    assert!(
        ho.status.success(),
        "hash-object failed: {}",
        String::from_utf8_lossy(&ho.stderr)
    );

    // Prune unreachable objects older than now.
    let (ok, _out, err) = git(&m.mnt, &["prune", "--expire=now"]);
    assert!(ok, "git prune failed: {err}");
    let (ok_fsck, _, efsck) = git(&m.mnt, &["fsck"]);
    assert!(ok_fsck, "fsck after prune failed: {efsck}");
    assert_repo_usable(&m);
}
