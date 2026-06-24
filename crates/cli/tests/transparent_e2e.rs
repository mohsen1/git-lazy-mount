//! End-to-end test of the transparent one-command flow: run the real
//! `git-lazy-mount <url> <path>` binary, assert it RETURNS, then drive the
//! mountpoint with plain stock `git` — no wrapper. Real `/dev/fuse` mount;
//! runs under `--features fuse` (the Linux mount CI job).
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;

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

#[test]
fn one_command_mount_then_stock_git_works() {
    let bin = env!("CARGO_BIN_EXE_git-lazy-mount");
    let remote = glm_testkit::seed_remote(&[
        ("README.md", b"hello\n"),
        ("src/main.rs", b"fn main() {}\n"),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("repo");
    let data = tmp.path().join("data");

    // The whole product contract in one command: clone + mount + validate +
    // RETURN. After it returns, the mount is live.
    let out = Command::new(bin)
        .arg(&remote.url)
        .arg(&mnt)
        .env("XDG_DATA_HOME", &data)
        .output()
        .expect("spawn git-lazy-mount");
    assert!(
        out.status.success(),
        "mount command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Now plain stock git works in the directory — no wrapper, no verbs.
    let (ok, top) = git(&mnt, &["rev-parse", "--show-toplevel"]);
    assert!(ok, "rev-parse failed after mount");
    assert_eq!(
        Path::new(&top).canonicalize().unwrap(),
        mnt.canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("README.md")).unwrap(),
        "hello\n"
    );

    // A full transparent workflow through the one-command mount.
    git(&mnt, &["config", "user.email", "t@e"]);
    git(&mnt, &["config", "user.name", "t"]);
    std::fs::write(mnt.join("README.md"), b"edited via the mount\n").unwrap();
    assert!(git(&mnt, &["add", "README.md"]).0, "git add failed");
    assert!(
        git(&mnt, &["commit", "-m", "edit through one-command mount"]).0,
        "git commit failed"
    );
    assert_eq!(
        git(&mnt, &["show", "HEAD:README.md"]).1,
        "edited via the mount"
    );

    // Clean up: the lifecycle subcommand unmounts (and the serve child exits).
    let un = Command::new(bin)
        .args(["unmount"])
        .arg(&mnt)
        .status()
        .expect("unmount");
    assert!(un.success(), "unmount failed");
    let _ = remote;
}
