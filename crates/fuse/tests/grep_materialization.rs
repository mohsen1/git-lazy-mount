//! Empirical answer to the README's "What about Grep?" question: does a
//! content search (`rg`, `git grep`, `grep -r`) over a lazily-mounted repo
//! materialize every file?
//!
//! Each scenario gets its OWN fresh mount (so blob-fault counts don't carry
//! over via the cache) and we read the projection's `hydrations()` blob-fault
//! counter around an external command run over the real `/dev/fuse` mount.
//!
//! Run with: `cargo test -p glm-fuse --features fuse --test grep_materialization -- --nocapture`
#![cfg(feature = "fuse")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

const N: usize = 1500;

/// A representative "large repo": N source files across subdirectories, ~1 in 10
/// containing the search NEEDLE (so a content search still has to read all of
/// them to know which match).
fn corpus() -> Vec<(String, Vec<u8>)> {
    (0..N)
        .map(|i| {
            let mut c = format!(
                "// module {i}\npub fn f{i}(x: i64) -> i64 {{\n    let y = x * {i};\n    y + {i}\n}}\n"
            );
            if i % 10 == 0 {
                c.push_str("// NEEDLE: distinctive marker token\n");
            }
            (format!("src/pkg{:02}/mod{i}.rs", i / 100), c.into_bytes())
        })
        .collect()
}

fn du_kb(p: &Path) -> u64 {
    Command::new("du")
        .args(["-sk"])
        .arg(p)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().next().map(str::to_owned))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Mount a fresh lazy copy of the corpus, run `make_cmd(mnt)` over it, and return
/// (blobs faulted, wall time, cache KiB on disk after).
fn measure<F>(make_cmd: F) -> (u64, Duration, u64)
where
    F: FnOnce(&Path) -> Command,
{
    let bodies = corpus();
    let refs: Vec<(&str, &[u8])> = bodies
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let remote = glm_testkit::seed_remote(&refs);
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    let gitdir = tmp.path().join("git");
    let cache = tmp.path().join("cache");

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
    let repo2 = AdminRepo::open(&gitdir, &mnt).unwrap();
    let proj =
        Arc::new(Projection::open(repo2, cache.clone(), tmp.path().join("overlay")).unwrap());
    let mount = spawn_mount(Arc::clone(&proj), &mnt).unwrap();
    for _ in 0..500 {
        if mnt.join(".git").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // git config so `git grep` works inside the mount.
    let _ = Command::new("git")
        .arg("-C")
        .arg(&mnt)
        .args(["config", "user.email", "t@e"])
        .output();

    let h0 = proj.hydrations();
    let t = Instant::now();
    let _ = make_cmd(&mnt).output().expect("command ran");
    let dt = t.elapsed();
    let faults = proj.hydrations() - h0;
    let kb = du_kb(&cache);
    mount.unmount();
    let _ = remote;
    (faults, dt, kb)
}

#[test]
fn grep_over_lazy_mount_materializes_every_file() {
    eprintln!("\n=== GREP MATERIALIZATION EXPERIMENT (repo = {N} files) ===");

    // Control 1: listing the whole tree fetches nothing.
    let (find_f, find_t, find_kb) = measure(|mnt| {
        let mut c = Command::new("find");
        c.arg(mnt).args(["-type", "f"]);
        c
    });
    eprintln!(
        "find -type f (readdir all):  {find_f:>5} blobs faulted, {find_t:?}, {find_kb} KiB cached"
    );

    // Control 2: reading one file fetches exactly one blob.
    let (one_f, one_t, _) = measure(|mnt| {
        let mut c = Command::new("cat");
        c.arg(mnt.join("src/pkg00/mod0.rs"));
        c
    });
    eprintln!("cat one file:                {one_f:>5} blobs faulted, {one_t:?}");

    // The question: ripgrep (the typical AI-agent Grep tool).
    let (rg_f, rg_t, rg_kb) = measure(|mnt| {
        let mut c = Command::new("rg");
        c.args(["--no-ignore", "--no-messages", "NEEDLE"]).arg(mnt);
        c
    });
    eprintln!("rg NEEDLE:                   {rg_f:>5} blobs faulted, {rg_t:?}, {rg_kb} KiB cached");

    // The README's other example: `git grep` over the working tree.
    let (gg_f, gg_t, gg_kb) = measure(|mnt| {
        let mut c = Command::new("git");
        c.arg("-C")
            .arg(mnt)
            .args(["grep", "--no-index", "NEEDLE", ":/"]);
        c
    });
    eprintln!("git grep NEEDLE:             {gg_f:>5} blobs faulted, {gg_t:?}, {gg_kb} KiB cached");

    // POSIX grep -r, for completeness.
    let (gr_f, gr_t, _) = measure(|mnt| {
        let mut c = Command::new("grep");
        c.args(["-rl", "NEEDLE"]).arg(mnt);
        c
    });
    eprintln!("grep -rl NEEDLE:             {gr_f:>5} blobs faulted, {gr_t:?}");
    eprintln!("=== END (repo has {N} files) ===\n");

    // The claims, as assertions.
    assert_eq!(find_f, 0, "readdir must fault zero blobs");
    assert_eq!(one_f, 1, "reading one file must fault exactly one blob");
    assert!(
        rg_f as f64 >= 0.9 * N as f64,
        "ripgrep must materialize ~every file: {rg_f} of {N}"
    );
    assert!(
        gr_f as f64 >= 0.9 * N as f64,
        "grep -r must materialize ~every file: {gr_f} of {N}"
    );
}
