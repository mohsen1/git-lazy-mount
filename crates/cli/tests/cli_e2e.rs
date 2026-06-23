//! End-to-end test of the `git-lazy-mount` binary (spec §53 criteria 1–4, 15, 17).
//!
//! Drives the real executable against a real partial-clone-capable remote and
//! verifies the lazy clone → ls → cat → edit → stage → commit → push flow,
//! including that the pushed commit lands on the bare remote.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_git-lazy-mount")
}

/// Apply a deterministic, host-independent Git environment to a command:
/// identity (CI runners have none) and `core.autocrlf=false` injected via
/// `GIT_CONFIG_*` env so faithful filtering doesn't introduce platform CRLF
/// (Git for Windows ships `core.autocrlf=true` in system config).
fn det_env(cmd: &mut Command, data_root: &Path) {
    cmd.env("GLM_DATA_ROOT", data_root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.autocrlf")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com");
}

fn run(data_root: &Path, args: &[&str]) -> (String, String, bool) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    det_env(&mut cmd, data_root);
    let out = cmd.output().expect("spawn git-lazy-mount");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn run_stdin(data_root: &Path, args: &[&str], stdin: &[u8]) -> bool {
    use std::io::Write;
    let mut cmd = Command::new(bin());
    cmd.args(args);
    det_env(&mut cmd, data_root);
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait().unwrap().success()
}

#[test]
fn clone_read_edit_commit_push_end_to_end() {
    let mut remote = glm_testkit::seed_remote(&[
        ("README.md", b"hello world\n"),
        ("src/compiler/checker.rs", b"fn check() {}\n"),
    ]);
    let data = tempfile::tempdir().unwrap();
    let mnt = tempfile::tempdir().unwrap();
    let mnt = mnt.path();

    // clone (lazy)
    let (_o, e, ok) = run(
        data.path(),
        &[
            "clone",
            &remote.url,
            mnt.to_str().unwrap(),
            "--branch",
            "main",
        ],
    );
    assert!(ok, "clone failed: {e}");

    // ls root and a nested directory, from Git trees (no checkout).
    let (root, _, ok) = run(data.path(), &["--mount", mnt.to_str().unwrap(), "ls"]);
    assert!(ok);
    assert!(root.contains("README.md"));
    assert!(root.contains("src/"));
    let (nested, _, ok) = run(
        data.path(),
        &["--mount", mnt.to_str().unwrap(), "ls", "src/compiler"],
    );
    assert!(ok);
    assert!(nested.contains("checker.rs"));

    // cat lazily fetches one file's content.
    let (content, _, ok) = run(
        data.path(),
        &["--mount", mnt.to_str().unwrap(), "cat", "README.md"],
    );
    assert!(ok);
    assert_eq!(content, "hello world\n");

    // edit via the overlay, stage, and commit.
    assert!(run_stdin(
        data.path(),
        &[
            "--mount",
            mnt.to_str().unwrap(),
            "debug",
            "write",
            "notes.txt"
        ],
        b"a brand new file\n",
    ));
    let (_o, _e, ok) = run(
        data.path(),
        &["--mount", mnt.to_str().unwrap(), "add", "notes.txt"],
    );
    assert!(ok);
    let (commit_out, e, ok) = run(
        data.path(),
        &[
            "--mount",
            mnt.to_str().unwrap(),
            "--json",
            "commit",
            "-m",
            "add notes",
        ],
    );
    assert!(ok, "commit failed: {e}");
    assert!(commit_out.contains("\"commit\""));

    // push to the bare remote, then confirm it received the commit (criterion 17).
    let (_o, e, ok) = run(data.path(), &["--mount", mnt.to_str().unwrap(), "push"]);
    assert!(ok, "push failed: {e}");

    // The bare remote now has notes.txt at the branch tip.
    remote.head_hex =
        String::from_utf8(glm_testkit::git(&remote.bare_path, &["rev-parse", "main"]))
            .unwrap()
            .trim()
            .to_string();
    let shown = glm_testkit::git(&remote.bare_path, &["show", "main:notes.txt"]);
    assert_eq!(shown, b"a brand new file\n");

    // status is clean after the committed change was dematerialized.
    let (status, _, ok) = run(data.path(), &["--mount", mnt.to_str().unwrap(), "status"]);
    assert!(ok);
    assert!(status.contains("clean"), "status: {status}");
}
