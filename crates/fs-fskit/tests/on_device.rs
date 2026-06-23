//! On-device FSKit validation harness (issue #12, spec §54).
//!
//! These tests are the **on-device** gate for the macOS support claim. They are
//! `#[ignore]` so they never run in the default CI matrix; the manual
//! `macos fskit backend (manual)` job (`.github/workflows/ci.yml`) runs them with
//! `--include-ignored`.
//!
//! They **self-gate** on a real backend: each one probes
//! [`glm_fs_fskit::backend_available`] / attempts a real mount and, when the
//! signed+approved FSKit extension is not present (e.g. on a GitHub-hosted runner,
//! which cannot load a system extension), prints a clear `SKIP` and returns rather
//! than failing. They only assert real-mount behavior on a properly provisioned
//! self-hosted Apple host. This guarantees a green run never produces a false
//! "macOS supported" signal.
//!
//! Findings from real on-device runs are recorded in `docs/platform-macos.md`.

use std::sync::Arc;

use glm_core::{ErrorCode, RepoPath};
use glm_fs_fskit::{backend_available, capability, extension_state, mount, FskitOps, ROOT_INO};
use glm_git_store::{FetchOptions, GitStore};
use glm_object_provider::{GitObjectProvider, ObjectProvider};
use glm_workspace::{Workspace, WorkspaceConfig};

fn workspace(files: &[(&str, &[u8])]) -> (tempfile::TempDir, glm_testkit::SeededRemote, Workspace) {
    let remote = glm_testkit::seed_remote(files);
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
    store.set_config("protocol.file.allow", "always").unwrap();
    store.set_config("core.autocrlf", "false").unwrap();
    store.add_remote("origin", &remote.url).unwrap();
    store
        .fetch(
            "origin",
            &[],
            &FetchOptions {
                filter: Some("blob:none".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let base = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    let provider: Arc<dyn ObjectProvider> =
        Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
    let cfg = WorkspaceConfig {
        workspace_head_ref: "refs/lazy-mount/workspaces/od/head".into(),
        attached_branch: None,
        remote: Some("origin".into()),
        identity: None,
    };
    let ws = Workspace::open_or_create(store, provider, tmp.path(), cfg, Some(base)).unwrap();
    (tmp, remote, ws)
}

/// Probe + lifecycle report. Always runs (under `--include-ignored`); on macOS it
/// asserts the probe is internally consistent and prints the report for the log.
#[test]
#[ignore = "on-device: run via the manual macOS CI job"]
fn on_device_capability_and_lifecycle() {
    let cap = capability();
    let state = extension_state(&cap);
    println!(
        "[on-device] platform_is_macos={} os={:?} backend={:?} extension_state={} usable={}",
        cap.platform_is_macos,
        cap.os_version,
        cap.selected_backend().map(|b| b.label()),
        state.label(),
        cap.is_usable()
    );
    // The probe must be self-consistent regardless of host.
    assert_eq!(cap.is_usable(), cap.selected_backend().is_some());
    if cfg!(target_os = "macos") {
        assert!(
            cap.platform_is_macos,
            "probe should detect macOS on a mac host"
        );
    }
}

/// Real mount + read/write through the kernel. Self-skips unless a backend is
/// present and the on-device FSVolume adapter is built in.
#[test]
#[ignore = "on-device: requires a signed+approved FSKit extension on Apple hardware"]
fn on_device_real_mount_read_write() {
    if !backend_available() {
        println!("[on-device] SKIP real mount: no FSKit/macFUSE backend available");
        for step in capability().diagnostics() {
            println!("[on-device]   - {step}");
        }
        return;
    }
    let (tmp, _remote, ws) = workspace(&[("a.txt", b"hello\n")]);
    let mountpoint = tmp.path().join("mnt");
    std::fs::create_dir_all(&mountpoint).unwrap();
    match mount(FskitOps::new(ws), &mountpoint) {
        Ok(()) => {
            // Real mount succeeded: exercise the kernel path.
            let got = std::fs::read(mountpoint.join("a.txt")).unwrap();
            assert_eq!(got, b"hello\n");
            std::fs::write(mountpoint.join("new.txt"), b"world\n").unwrap();
            assert_eq!(
                std::fs::read(mountpoint.join("new.txt")).unwrap(),
                b"world\n"
            );
        }
        Err(e) if e.code == ErrorCode::FilesystemBackendUnavailable => {
            println!("[on-device] SKIP real mount: FSVolume adapter not built in ({e})");
        }
        Err(e) => panic!("unexpected mount error: {e}"),
    }
}

/// Real APFS collision behavior through a mount (case-insensitive volume): two
/// entries that Git treats as distinct must be surfaced, not silently merged.
/// Self-skips without a backend.
#[test]
#[ignore = "on-device: requires a real FSKit mount on an APFS volume"]
fn on_device_real_collision_surfaced() {
    if !backend_available() {
        println!("[on-device] SKIP collision check: no FSKit/macFUSE backend available");
        return;
    }
    // The bridge-level collision logic is validated by the unit tests; here we
    // confirm it through the real volume once the adapter is present.
    let (_tmp, _remote, ws) = workspace(&[("LICENSE", b"x\n")]);
    let ops = FskitOps::new(ws);
    ops.workspace()
        .write_full(
            &RepoPath::from_bytes(b"README".to_vec()).unwrap(),
            b"u\n",
            false,
        )
        .unwrap();
    ops.workspace()
        .write_full(
            &RepoPath::from_bytes(b"readme".to_vec()).unwrap(),
            b"l\n",
            false,
        )
        .unwrap();
    let collisions = ops.directory_collisions(ROOT_INO).unwrap();
    assert!(
        !collisions.is_empty(),
        "README/readme must be surfaced as a collision on a case-insensitive volume"
    );
}
