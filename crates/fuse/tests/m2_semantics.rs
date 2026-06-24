//! M2 transparent-mount semantics: real-mount integration tests that mount the
//! projection through the kernel (`spawn_mount`) and drive it with ordinary
//! `std::fs` calls, exactly as an editor or a build tool would. Each test owns
//! its own `Arc<Projection>` clone (taken before `spawn_mount`, which clones
//! again), so it can read the `hydrations()` budget counter while the mount
//! runs, then unmounts.
//!
//! Real `/dev/fuse` mount — gated on the lib's `fuse` feature (matching the
//! sibling `m3_git.rs`), so the whole file compiles/runs only under
//! `cargo test -p glm-fuse --features fuse` (the Linux mount CI job). It uses
//! the crate's PUBLIC API only: `glm_fuse::{spawn_mount, BackgroundMount}` plus
//! the `glm_worktree::Projection` / `glm_git_repo::AdminRepo` it is built from.
//!
//! Coverage (design.md):
//! * §17.4 open-then-unlink — handle survives namespace removal
//! * §17.5 / §28 rename-while-open — atomic editor save, fd identity preserved
//! * §4.9 empty untracked dir survives unmount → remount (same overlay/cache)
//! * §38.7 `O_TRUNC` open fetches no old blob
//! * §38.6 100 concurrent reads → one retrieval (target == 1; ignored until
//!   coalescing lands)
//! * §38.8 repeated 4 KiB writes into a large file (in-place pwrite)
//!
//! NOTE: Experiment C (`git status` sees a transparent edit) is already covered
//! by `m3_git.rs::git_status_add_commit_through_the_transparent_mount`, which
//! builds the real index via `proj.repo().build_index()` and drives status/add/
//! commit — so it is intentionally NOT duplicated here (it is M3 work).

#![cfg(feature = "fuse")]

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use glm_fuse::{spawn_mount, BackgroundMount};
use glm_git_repo::{AdminRepo, CloneOptions};
use glm_worktree::Projection;

/// Keeps every piece of mount state alive for a test: the seeded promisor remote
/// (so lazy blob fetches still reach it), the tempdir holding gitdir/overlay/
/// cache/mountpoint, and the path layout.
struct Fixture {
    // The `SeededRemote` owns the bare repo's tempdir; it MUST outlive the
    // projection so a lazy blob fetch can reach the promisor. Kept by field.
    _remote: glm_testkit::SeededRemote,
    _tmp: tempfile::TempDir,
    mnt: PathBuf,
    gitdir: PathBuf,
    overlay_dir: PathBuf,
    cache_dir: PathBuf,
}

impl Fixture {
    /// Seed a remote with `files`, transparently clone it, and lay out the
    /// gitdir / cache / overlay / mountpoint dirs (but do not open or mount yet,
    /// so a test can open the `Projection` itself and keep its own `Arc`).
    fn seed(files: &[(&str, &[u8])]) -> Fixture {
        let remote = glm_testkit::seed_remote(files);
        let tmp = tempfile::tempdir().unwrap();
        let mnt = tmp.path().join("mnt");
        let gitdir = tmp.path().join("git");
        let overlay_dir = tmp.path().join("overlay");
        let cache_dir = tmp.path().join("cache");
        // `AdminRepo::clone` creates the gitdir + worktree dir; we re-`open` it
        // per mount via `open_projection` so each (re)mount gets a fresh handle.
        let _repo = AdminRepo::clone(
            &remote.url,
            &gitdir,
            &mnt,
            &tmp.path().join("anchor"),
            &CloneOptions::default(),
        )
        .unwrap();
        Fixture {
            _remote: remote,
            _tmp: tmp,
            mnt,
            gitdir,
            overlay_dir,
            cache_dir,
        }
    }

    /// Open a fresh `Projection` over this fixture's (already-cloned) admin repo,
    /// reusing the SAME overlay/cache dirs — so a remount sees persisted state.
    fn open_projection(&self) -> Arc<Projection> {
        let repo = AdminRepo::open(&self.gitdir, &self.mnt).unwrap();
        Arc::new(Projection::open(repo, self.cache_dir.clone(), self.overlay_dir.clone()).unwrap())
    }
}

/// Poll `cond` up to ~5s; returns whether it became true.
fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Mount `proj` at `mnt` and block until the synthetic `.git` is visible.
fn mount_ready(proj: Arc<Projection>, mnt: &Path) -> BackgroundMount {
    let mount = spawn_mount(proj, mnt).unwrap();
    assert!(
        wait_until(|| mnt.join(".git").exists()),
        "mount did not become ready at {}",
        mnt.display()
    );
    mount
}

// ---------------------------------------------------------------------------
// 1. §17.4 — open then unlink: the handle outlives the name.
// ---------------------------------------------------------------------------

#[test]
fn open_then_unlink_handle_survives_and_name_is_gone() {
    let fx = Fixture::seed(&[("keep.txt", b"v0\n"), ("doomed.txt", b"hello\n")]);
    let proj = fx.open_projection();
    let mount = mount_ready(Arc::clone(&proj), &fx.mnt);

    let target = fx.mnt.join("doomed.txt");
    // Open a read+write fd BEFORE unlinking (copy-up happens on first write).
    let mut fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .unwrap();
    let mut pre = String::new();
    fd.read_to_string(&mut pre).unwrap();
    assert_eq!(pre, "hello\n");

    // Remove the name from the namespace while the fd is still open.
    std::fs::remove_file(&target).unwrap();

    // The name is gone from lookup and from readdir.
    assert!(!target.exists(), "unlinked name must vanish");
    let names: Vec<String> = std::fs::read_dir(&fx.mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        !names.iter().any(|n| n == "doomed.txt"),
        "readdir still shows the unlinked name: {names:?}"
    );
    assert!(names.iter().any(|n| n == "keep.txt"));

    // The open fd still reads and writes correctly (storage retained, §17.4).
    fd.seek(SeekFrom::Start(0)).unwrap();
    let mut again = String::new();
    fd.read_to_string(&mut again).unwrap();
    assert_eq!(again, "hello\n", "open fd lost its content after unlink");

    fd.seek(SeekFrom::End(0)).unwrap();
    fd.write_all(b"appended\n").unwrap();
    fd.seek(SeekFrom::Start(0)).unwrap();
    let mut after_write = String::new();
    fd.read_to_string(&mut after_write).unwrap();
    assert_eq!(
        after_write, "hello\nappended\n",
        "write to unlinked fd failed"
    );

    drop(fd);
    mount.unmount();
}

// ---------------------------------------------------------------------------
// 2. §17.5 / §28 — rename-over-original (editor atomic save) while a reader fd
//    is open; the final content is the new file's, the open fd keeps identity.
// ---------------------------------------------------------------------------

#[test]
fn rename_over_original_atomic_editor_save() {
    let fx = Fixture::seed(&[("notes.txt", b"old contents\n")]);
    let proj = fx.open_projection();
    let mount = mount_ready(Arc::clone(&proj), &fx.mnt);

    let orig = fx.mnt.join("notes.txt");
    let tmp_sibling = fx.mnt.join(".notes.txt.swp");

    // A reader holds the original open across the rename (editors / tail -f).
    let mut reader = std::fs::File::open(&orig).unwrap();
    let mut before = String::new();
    reader.read_to_string(&mut before).unwrap();
    assert_eq!(before, "old contents\n");

    // Editor's atomic save: write the full new body to a tmp sibling, then
    // rename it over the original name.
    std::fs::write(&tmp_sibling, b"brand new contents\n").unwrap();
    std::fs::rename(&tmp_sibling, &orig).unwrap();

    // The tmp name is consumed; the original name now holds the new content.
    assert!(
        !tmp_sibling.exists(),
        "tmp sibling should be gone after rename"
    );
    assert_eq!(
        std::fs::read_to_string(&orig).unwrap(),
        "brand new contents\n",
        "rename-over did not publish the new content"
    );

    // The pre-existing reader fd keeps its OWN file identity: per POSIX a rename
    // over the path does not redirect an already-open fd. It still reads the
    // bytes it was opened on (§17.5: handles refer to the same identity).
    reader.seek(SeekFrom::Start(0)).unwrap();
    let mut via_old_fd = String::new();
    reader.read_to_string(&mut via_old_fd).unwrap();
    assert_eq!(
        via_old_fd, "old contents\n",
        "open fd identity not preserved across rename-over"
    );

    drop(reader);
    mount.unmount();
}

// ---------------------------------------------------------------------------
// 3. §4.9 — an empty untracked dir is durable workspace state: it survives an
//    unmount and a remount of the SAME overlay/cache dirs.
// ---------------------------------------------------------------------------

#[test]
fn empty_untracked_dir_survives_unmount_remount() {
    let fx = Fixture::seed(&[("README.md", b"x\n")]);

    // First mount: create an empty dir through the kernel, then unmount.
    {
        let proj = fx.open_projection();
        let mount = mount_ready(Arc::clone(&proj), &fx.mnt);
        std::fs::create_dir(fx.mnt.join("empty_dir")).unwrap();
        assert!(fx.mnt.join("empty_dir").is_dir());
        mount.unmount();
    }
    // Wait for the kernel to fully tear the first mount down before remounting
    // the same mountpoint.
    assert!(
        wait_until(|| !fx.mnt.join("empty_dir").exists()),
        "first mount did not unmount"
    );

    // Remount a FRESH projection over the SAME overlay + cache dirs (daemon
    // restart). The empty dir must still be present (lookup + readdir).
    {
        let proj = fx.open_projection();
        let mount = mount_ready(Arc::clone(&proj), &fx.mnt);
        assert!(
            fx.mnt.join("empty_dir").is_dir(),
            "empty untracked dir did not survive remount (§4.9)"
        );
        let names: Vec<String> = std::fs::read_dir(&fx.mnt)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            names.iter().any(|n| n == "empty_dir"),
            "remounted readdir is missing the empty dir: {names:?}"
        );
        mount.unmount();
    }
}

// ---------------------------------------------------------------------------
// 4. §38.7 — open(O_WRONLY|O_TRUNC) of a baseline file fetches NO old blob.
// ---------------------------------------------------------------------------

#[test]
fn otrunc_open_fetches_no_old_blob() {
    let fx = Fixture::seed(&[("big.txt", b"the original baseline body, never read\n")]);
    let proj = fx.open_projection();
    let mount = mount_ready(Arc::clone(&proj), &fx.mnt);

    let target = fx.mnt.join("big.txt");

    // Snapshot the hydration budget WITHOUT reading the file first (a read would
    // materialize the baseline blob). A bare getattr may fault the exact size via
    // GitStore::object_size, but that does NOT touch the projection's hydration
    // counter — only a content materialization (copy-up / cache miss) does.
    let before = proj.hydrations();

    // O_WRONLY | O_TRUNC: `write(true)` without `read(true)` => O_WRONLY.
    let mut fd = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&target)
        .unwrap();
    fd.write_all(b"fresh\n").unwrap();
    drop(fd);

    assert_eq!(
        proj.hydrations(),
        before,
        "O_TRUNC open hydrated the old blob (§38.7)"
    );

    // The new content reads back; this read serves from the overlay copy-up, not
    // a baseline materialization, so it also leaves the counter unchanged.
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "fresh\n");
    assert_eq!(
        proj.hydrations(),
        before,
        "reading the freshly-truncated overlay file should not hydrate a baseline blob"
    );

    mount.unmount();
}

// ---------------------------------------------------------------------------
// 5. §38.6 — 100 concurrent reads of one not-yet-materialized file must perform
//    exactly ONE underlying retrieval. Coalescing is not implemented yet, so
//    this asserts the CORRECT target (== 1) and is ignored until §38.6 lands.
// ---------------------------------------------------------------------------

#[test]
fn hundred_concurrent_reads_coalesce_to_one_retrieval() {
    let body: &[u8] = b"shared baseline blob read by a hundred threads at once\n";
    let fx = Fixture::seed(&[("shared.bin", body)]);
    let proj = fx.open_projection();
    let mount = mount_ready(Arc::clone(&proj), &fx.mnt);

    let before = proj.hydrations();
    let target = Arc::new(fx.mnt.join("shared.bin"));

    let handles: Vec<_> = (0..100)
        .map(|_| {
            let target = Arc::clone(&target);
            std::thread::spawn(move || std::fs::read(&*target).unwrap())
        })
        .collect();
    for h in handles {
        let got = h.join().unwrap();
        assert_eq!(got, body, "a concurrent reader saw wrong bytes");
    }

    assert_eq!(
        proj.hydrations() - before,
        1,
        "100 concurrent reads must cause exactly one object retrieval (§38.6)"
    );

    mount.unmount();
}

// ---------------------------------------------------------------------------
// 6. §38.8 — many 4 KiB writes at increasing offsets into a large file go
//    through as in-place pwrites (no full-file rewrite); final size + spot bytes
//    are correct.
// ---------------------------------------------------------------------------

#[test]
fn many_4k_writes_into_large_file_in_place() {
    use std::os::unix::fs::FileExt;

    const BLK: usize = 4096;
    const BLOCKS: usize = 1024; // 4 MiB total
    const TOTAL: u64 = (BLK * BLOCKS) as u64;

    let fx = Fixture::seed(&[("placeholder", b"x\n")]);
    let proj = fx.open_projection();
    let mount = mount_ready(Arc::clone(&proj), &fx.mnt);

    let target = fx.mnt.join("data.bin");

    // Create the file, then size it up with a single set_len (setattr size); this
    // never materializes a baseline blob (the file is overlay-only) and lets us
    // pwrite into the middle without rewriting the whole file.
    let fd = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&target)
        .unwrap();
    fd.set_len(TOTAL).unwrap();

    // Each block's first 8 bytes encode its index; pwrite at the block offset.
    for i in 0..BLOCKS {
        let mut blk = vec![0u8; BLK];
        blk[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let off = (i * BLK) as u64;
        let n = fd.write_at(&blk, off).unwrap();
        assert_eq!(n, BLK, "short pwrite at block {i}");
    }
    fd.sync_all().unwrap();
    drop(fd);

    // Final size is exactly what we sized it to (writes did not grow/shrink it).
    let meta = std::fs::metadata(&target).unwrap();
    assert_eq!(meta.len(), TOTAL, "final size mismatch");

    // Spot-check a handful of blocks by reading just their 8-byte header — a
    // bounded read, not a whole-file slurp.
    let rd = std::fs::File::open(&target).unwrap();
    for &i in &[0usize, 1, 17, 511, 512, 1023] {
        let mut hdr = [0u8; 8];
        rd.read_exact_at(&mut hdr, (i * BLK) as u64).unwrap();
        assert_eq!(
            u64::from_le_bytes(hdr),
            i as u64,
            "block {i} header wrong (in-place pwrite corrupted)"
        );
    }

    mount.unmount();
}
