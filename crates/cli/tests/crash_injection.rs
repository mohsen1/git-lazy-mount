//! Crash-injection durability (design.md §40.5, criterion 27): SIGKILL the
//! serving process mid-session and prove no acknowledged user write is lost — the
//! durable overlay (atomic sidecars + content files) survives an ungraceful
//! daemon death. Real `/dev/fuse` mount; runs under `--features fuse`.
#![cfg(feature = "fuse")]

use std::process::Command;
use std::time::Duration;

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
fn dirty_state_survives_an_injected_daemon_crash() {
    let bin = env!("CARGO_BIN_EXE_git-lazy-mount");
    let remote = glm_testkit::seed_remote(&[("README.md", b"baseline\n")]);
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    let gitdir = tmp.path().join("git");
    let cache = tmp.path().join("cache");
    let overlay = tmp.path().join("overlay");

    // Set up the admin repo + real index (the CLI start flow's first half).
    let repo = AdminRepo::clone(
        &remote.url,
        &gitdir,
        &mnt,
        &tmp.path().join("anchor"),
        &CloneOptions::default(),
    )
    .unwrap();
    repo.build_index().unwrap();
    drop(repo);

    // Spawn the serving daemon (the `__serve` mode that holds the kernel mount).
    let mut serve = Command::new(bin)
        .args(["__serve"])
        .arg("--gitdir")
        .arg(&gitdir)
        .arg("--mountpoint")
        .arg(&mnt)
        .arg("--cache")
        .arg(&cache)
        .arg("--overlay")
        .arg(&overlay)
        .spawn()
        .expect("spawn serve");
    assert!(wait_until(|| mnt.join(".git").exists()), "mount not ready");

    // Acknowledged user writes through the mount: an edit + a new file. Each
    // `write` opens, writes, and closes (FLUSH+RELEASE), so the overlay's durable
    // sidecar + content are published before we return.
    std::fs::write(mnt.join("README.md"), b"edited before the crash\n").unwrap();
    std::fs::write(mnt.join("fresh.txt"), b"created before the crash\n").unwrap();

    // INJECT THE CRASH: SIGKILL the serving daemon (no graceful unmount/quiesce).
    serve.kill().expect("kill serve");
    let _ = serve.wait();
    // Release the now-dead kernel mount so the path is usable again.
    for t in [["fusermount3", "-u"], ["fusermount", "-u"], ["umount", ""]] {
        let mut c = Command::new(t[0]);
        if !t[1].is_empty() {
            c.arg(t[1]);
        }
        if c.arg(&mnt).status().map(|s| s.success()).unwrap_or(false) {
            break;
        }
    }

    // Recover: re-open the projection on the SAME overlay/cache (a fresh daemon
    // would do exactly this). No acknowledged write may be lost.
    let repo2 = AdminRepo::open(&gitdir, &mnt).unwrap();
    let proj = Projection::open(repo2, cache, overlay).unwrap();
    let root = proj.root_ino();

    let read = |p: &Projection, name: &[u8]| -> Vec<u8> {
        let a = p
            .lookup(root, name)
            .unwrap()
            .unwrap_or_else(|| panic!("{:?} lost across the crash", String::from_utf8_lossy(name)));
        p.open_content(a.ino).unwrap().read_at(0, 4096).unwrap()
    };
    assert_eq!(read(&proj, b"README.md"), b"edited before the crash\n");
    assert_eq!(read(&proj, b"fresh.txt"), b"created before the crash\n");

    let _ = remote; // keep the promisor alive for the whole test
}
