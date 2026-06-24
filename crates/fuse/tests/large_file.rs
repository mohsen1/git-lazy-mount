//! Large-file bounded-memory: reading a large baseline blob must **stream** —
//! materialized by `cat-file` straight to the content cache (no in-process
//! buffer) and served by `pread` in request-sized chunks — so the daemon's
//! resident memory never grows by the file size, no matter how big the file is.
//! Real `/dev/fuse` mount.
#![cfg(feature = "fuse")]

use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::spawn_mount;
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

/// Current resident set size (KiB) of this process — which hosts the FUSE loop.
fn rss_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            return v.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn reading_a_large_baseline_file_does_not_buffer_it_whole() {
    const MIB: usize = 1024 * 1024;
    const SIZE: usize = 64 * MIB; // a "large" file relative to a bounded daemon

    // A deterministic, non-compressible-ish payload so a partial read can't pass
    // by coincidence: byte i = (i * 31 + 7) as u8.
    let mut payload = vec![0u8; SIZE];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31).wrapping_add(7)) as u8;
    }
    let want_sum: u64 = payload.iter().map(|&b| b as u64).sum();

    let remote = glm_testkit::seed_remote(&[("big.bin", &payload), ("small.txt", b"hi\n")]);
    drop(payload); // release the seed buffer before we measure the daemon's RSS

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

    // The blob is absent in the blob:none clone; reading faults it in once.
    assert_eq!(proj.hydrations(), 0, "no blob hydrated before the read");
    let rss_before = rss_kb();

    // Stream the whole 64 MiB through the mount in 256 KiB chunks, summing bytes
    // — the test never holds more than one chunk, so any large RSS growth is the
    // daemon buffering the blob.
    let mut f = std::fs::File::open(mnt.join("big.bin")).unwrap();
    let mut buf = vec![0u8; 256 * 1024];
    let mut got_sum: u64 = 0;
    let mut total = 0usize;
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got_sum += buf[..n].iter().map(|&b| b as u64).sum::<u64>();
        total += n;
    }
    drop(f);

    let rss_after = rss_kb();
    let grew_mib = rss_after.saturating_sub(rss_before) as f64 / 1024.0;
    eprintln!(
        "LARGE FILE: size={}MiB hydrations={} RSS {}->{} KiB (grew {:.1} MiB)",
        SIZE / MIB,
        proj.hydrations(),
        rss_before,
        rss_after,
        grew_mib
    );

    assert_eq!(total, SIZE, "must read the whole file");
    assert_eq!(got_sum, want_sum, "content must round-trip byte-for-byte");
    assert_eq!(
        proj.hydrations(),
        1,
        "the large blob faults in exactly once"
    );

    // Bounded memory: the daemon must NOT grow by anything near the 64 MiB file.
    // A generous ceiling well below the file size proves it streamed.
    assert!(
        grew_mib < 24.0,
        "daemon RSS grew {grew_mib:.1} MiB reading a 64 MiB file — it is buffering, not streaming"
    );

    mount.unmount();
    let _ = remote;
}
