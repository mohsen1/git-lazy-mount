//! Real libfuse loopback-mount validation (spec §40, §53).
//!
//! Compiled only with the `fuse` feature (links libfuse3) and `#[ignore]` so it
//! runs only under the manual `linux-mount` job / in Docker:
//!   `cargo test -p glm-fs-fuse --features fuse -- --include-ignored`
//! It mounts the shared engine through the real kernel and drives it with plain
//! `std::fs` calls — proving lazy hydration, enumeration, and writes work
//! end-to-end against a `blob:none` remote.
#![cfg(feature = "fuse")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use glm_fs_fuse::{spawn_mount, FuseOps};
use glm_git_store::{FetchOptions, GitStore};
use glm_object_provider::{GitObjectProvider, ObjectProvider};
use glm_workspace::{Workspace, WorkspaceConfig};

struct Mounted {
    _store_tmp: tempfile::TempDir,
    _remote: glm_testkit::SeededRemote,
    mnt: tempfile::TempDir,
    mount: Option<glm_fs_fuse::BackgroundMount>,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(m) = self.mount.take() {
            m.unmount();
        }
    }
}

fn mount(files: &[(&str, &[u8])]) -> Mounted {
    let remote = glm_testkit::seed_remote(files);
    let store_tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(store_tmp.path().join("git"), None).unwrap();
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
        workspace_head_ref: "refs/lazy-mount/workspaces/fuse/head".into(),
        attached_branch: None,
        remote: Some("origin".into()),
        identity: None,
    };
    let ws = Workspace::open_or_create(store, provider, store_tmp.path(), cfg, Some(base)).unwrap();

    let mnt = tempfile::tempdir().unwrap();
    let bg = spawn_mount(FuseOps::new(ws), mnt.path()).expect("fuse mount");

    // Wait until the kernel has the mount serving (root readdir succeeds).
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read_dir(mnt.path())
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
        {
            break;
        }
        assert!(Instant::now() < deadline, "mount did not become ready");
        std::thread::sleep(Duration::from_millis(50));
    }

    Mounted {
        _store_tmp: store_tmp,
        _remote: remote,
        mnt,
        mount: Some(bg),
    }
}

#[test]
#[ignore = "real loopback mount: run with --features fuse --include-ignored on Linux"]
fn real_mount_lazy_read_enumerate_and_write() {
    let m = mount(&[
        ("a.txt", b"hello\n"),
        ("src/lib.rs", b"fn main() {}\n"),
        ("big.bin", &[7u8; 4096]),
    ]);
    let root = m.mnt.path();

    // Enumeration through the real kernel readdir.
    let names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "a.txt"), "entries: {names:?}");
    assert!(names.iter().any(|n| n == "src"), "entries: {names:?}");

    // Lazy hydration: a.txt's blob was never fetched (blob:none); reading it
    // through the mount triggers the real fetch and returns the bytes.
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read(root.join("src/lib.rs")).unwrap(),
        b"fn main() {}\n"
    );
    // Exact size is reported via getattr.
    assert_eq!(std::fs::metadata(root.join("big.bin")).unwrap().len(), 4096);
    assert_eq!(std::fs::read(root.join("big.bin")).unwrap().len(), 4096);

    // Ranged read via seek+read.
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(root.join("a.txt")).unwrap();
        f.seek(SeekFrom::Start(2)).unwrap();
        let mut buf = [0u8; 2];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ll");
    }

    // Write a brand-new file through the mount; read it back.
    std::fs::write(root.join("new.txt"), b"written via fuse\n").unwrap();
    assert_eq!(
        std::fs::read(root.join("new.txt")).unwrap(),
        b"written via fuse\n"
    );
    assert!(std::fs::read_dir(root)
        .unwrap()
        .any(|e| e.unwrap().file_name() == "new.txt"));

    // Overwrite an existing (clean, lazily-hydrated) file, then rename it.
    std::fs::write(root.join("a.txt"), b"replaced\n").unwrap();
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"replaced\n");
    std::fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
    assert_eq!(
        std::fs::read(root.join("renamed.txt")).unwrap(),
        b"replaced\n"
    );
    assert!(
        std::fs::metadata(root.join("a.txt")).is_err(),
        "old name gone"
    );

    // Delete a file through the mount.
    std::fs::remove_file(root.join("new.txt")).unwrap();
    assert!(std::fs::metadata(root.join("new.txt")).is_err());
}
