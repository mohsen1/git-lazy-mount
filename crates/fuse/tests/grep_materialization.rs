//! Empirical answer to the README's "What about Grep?" question: does a
//! content search (`rg`, `git grep`, `grep -r`) over a lazily-mounted repo
//! materialize every file?
//!
//! Each scenario gets its OWN fresh mount (so blob-fault counts don't carry
//! over via the cache) and we read the projection's blob-materialization
//! counters around an external command run over the real `/dev/fuse` mount:
//! `hydrations()` (on-demand faults) and `hydrate_warms()` (the read-ahead
//! hydrator's speculative warms). A content search materializes ~every file in
//! their SUM — the read-ahead just shifts most faults off the on-demand counter.
//! Tools that aren't installed (e.g. `rg` in a minimal CI image) are skipped.
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
/// (blobs faulted, wall time, cache KiB). `None` if the command isn't installed.
fn measure<F>(make_cmd: F) -> Option<(u64, u64, Duration, u64)>
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
    let w0 = proj.hydrate_warms();
    let t = Instant::now();
    let ran = make_cmd(&mnt).output();
    let dt = t.elapsed();
    // On-demand faults (`hydrations`) vs. total content materialized (on-demand +
    // the read-ahead hydrator's speculative warms). A content search defeats
    // laziness in TOTAL even though the read-ahead shifts most faults off the
    // on-demand counter (its opens hit the pre-warmed cache).
    let ondemand = proj.hydrations() - h0;
    let total = ondemand + (proj.hydrate_warms() - w0);
    let kb = du_kb(&cache);
    mount.unmount();
    let _ = remote;
    ran.ok().map(|_| (ondemand, total, dt, kb))
}

fn report(label: &str, m: Option<(u64, u64, Duration, u64)>) -> Option<(u64, u64)> {
    match m {
        Some((ondemand, total, t, kb)) => {
            eprintln!(
                "{label:<28} {ondemand:>5} on-demand + {:>5} read-ahead = {total:>5} total, {t:?}, {kb} KiB cached",
                total - ondemand
            );
            Some((ondemand, total))
        }
        None => {
            eprintln!("{label:<28} (command not installed — skipped)");
            None
        }
    }
}

#[test]
fn grep_over_lazy_mount_materializes_every_file() {
    eprintln!("\n=== GREP MATERIALIZATION EXPERIMENT (repo = {N} files) ===");

    // Controls: listing fetches nothing; reading one file fetches one blob.
    let find = report(
        "find -type f (readdir all):",
        measure(|mnt| {
            let mut c = Command::new("find");
            c.arg(mnt).args(["-type", "f"]);
            c
        }),
    );
    let cat = report(
        "cat one file:",
        measure(|mnt| {
            let mut c = Command::new("cat");
            c.arg(mnt.join("src/pkg00/mod0.rs"));
            c
        }),
    );

    // Content searches — each must read (and so fault) every file.
    let rg = report(
        "rg NEEDLE:",
        measure(|mnt| {
            let mut c = Command::new("rg");
            c.args(["--no-ignore", "--no-messages", "NEEDLE"]).arg(mnt);
            c
        }),
    );
    let git_grep = report(
        "git grep NEEDLE:",
        measure(|mnt| {
            let mut c = Command::new("git");
            c.arg("-C")
                .arg(mnt)
                .args(["grep", "--no-index", "NEEDLE", ":/"]);
            c
        }),
    );
    let grep_r = report(
        "grep -rl NEEDLE:",
        measure(|mnt| {
            let mut c = Command::new("grep");
            c.args(["-rl", "NEEDLE"]).arg(mnt);
            c
        }),
    );
    eprintln!("=== END (repo has {N} files) ===\n");

    // Controls (find/cat are always present).
    if let Some((_, total)) = find {
        assert_eq!(total, 0, "readdir (no open) must materialize zero blobs");
    }
    if let Some((ondemand, _)) = cat {
        // Exactly one file is read on demand. (The read-ahead hydrator may warm
        // that file's source siblings off the on-demand counter — see `total`.)
        assert_eq!(
            ondemand, 1,
            "reading one file must fault exactly one blob on demand"
        );
    }

    // At least one content-search tool runs; every one that does must
    // materialize ~the whole repo in TOTAL (on-demand faults + read-ahead warms),
    // since a content search reads every file regardless of who fetched it.
    let content: Vec<u64> = [rg, git_grep, grep_r]
        .into_iter()
        .flatten()
        .map(|(_, total)| total)
        .collect();
    assert!(
        !content.is_empty(),
        "no content-search tool available to measure"
    );
    for f in content {
        assert!(
            f as f64 >= 0.9 * N as f64,
            "a content search must materialize ~every file: {f} of {N}"
        );
    }
}
