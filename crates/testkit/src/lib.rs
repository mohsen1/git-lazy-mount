//! `glm-testkit` — ephemeral Git fixtures for integration tests.
//!
//! Provides real (not mocked) Git remotes that support partial-clone filters,
//! so tests can exercise the genuine lazy-fetch code paths against `git`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A seeded bare "remote" repository on the local filesystem, reachable via a
/// `file://` URL, with partial-clone (`uploadpack.allowFilter`) enabled.
pub struct SeededRemote {
    /// The bare repository path.
    pub bare_path: PathBuf,
    /// A `file://` URL pointing at the bare repo.
    pub url: String,
    /// Hex object id of the branch tip.
    pub head_hex: String,
    /// Default branch name.
    pub branch: String,
    _tmp: TempDir,
}

/// Run `git` in `dir` with the given args; panics with captured output on error.
pub fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Deterministic identity; never sign in tests.
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

/// Seed a bare remote with a single initial commit containing `files`, on the
/// `main` branch.
pub fn seed_remote(files: &[(&str, &[u8])]) -> SeededRemote {
    seed_remote_with(files, "main")
}

/// Seed a bare remote on a named branch.
pub fn seed_remote_with(files: &[(&str, &[u8])], branch: &str) -> SeededRemote {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    git(&work, &["init", "-b", branch]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.com"]);

    write_files(&work, files);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "initial commit"]);

    let bare = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    // Enable partial clone on the "server" side.
    git(&bare, &["config", "uploadpack.allowFilter", "true"]);
    git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);

    let head_hex = String::from_utf8(git(&bare, &["rev-parse", branch]))
        .unwrap()
        .trim()
        .to_string();
    let url = format!("file://{}", bare.display());

    SeededRemote {
        bare_path: bare,
        url,
        head_hex,
        branch: branch.to_string(),
        _tmp: tmp,
    }
}

/// Seed a remote whose tree contains a symlink `name` -> `target` (Git mode
/// 120000) plus a regular `keep.txt`. Unix-only (Git records the link from the
/// real symlink in the seed working tree).
#[cfg(unix)]
pub fn seed_remote_symlink(name: &str, target: &str) -> SeededRemote {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    git(&work, &["config", "user.name", "Test"]);
    git(&work, &["config", "user.email", "test@example.com"]);
    std::fs::write(work.join("keep.txt"), b"k\n").unwrap();
    std::os::unix::fs::symlink(target, work.join(name)).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "symlink"]);

    let bare = tmp.path().join("remote.git");
    git(
        tmp.path(),
        &[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["config", "uploadpack.allowFilter", "true"]);
    git(&bare, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let head_hex = String::from_utf8(git(&bare, &["rev-parse", "main"]))
        .unwrap()
        .trim()
        .to_string();
    let url = format!("file://{}", bare.display());
    SeededRemote {
        bare_path: bare,
        url,
        head_hex,
        branch: "main".to_string(),
        _tmp: tmp,
    }
}

impl SeededRemote {
    /// Add a new commit to the remote's branch with the given files, returning
    /// the new tip hex. Useful for simulating concurrent branch movement.
    pub fn add_commit(&mut self, files: &[(&str, &[u8])], message: &str) -> String {
        let work = self._tmp.path().join("work2");
        if !work.exists() {
            git(
                self._tmp.path(),
                &[
                    "clone",
                    self.bare_path.to_str().unwrap(),
                    work.to_str().unwrap(),
                ],
            );
            git(&work, &["config", "commit.gpgsign", "false"]);
            git(&work, &["config", "user.name", "Test"]);
            git(&work, &["config", "user.email", "test@example.com"]);
        }
        write_files(&work, files);
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", message]);
        git(&work, &["push", "origin", &format!("HEAD:{}", self.branch)]);
        self.head_hex = String::from_utf8(git(&self.bare_path, &["rev-parse", &self.branch]))
            .unwrap()
            .trim()
            .to_string();
        self.head_hex.clone()
    }
}

fn write_files(root: &Path, files: &[(&str, &[u8])]) {
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }
}
