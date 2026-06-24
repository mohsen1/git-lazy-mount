//! Daemon-side handler for the per-inode FS operations (ADR 0008).
//!
//! The sandboxed macOS FSKit extension forwards each `FSVolume` callback to the
//! unsandboxed daemon; this maps one [`glm_ipc::fs::FsRequest`] onto [`FskitOps`]
//! and produces an [`glm_ipc::fs::FsResponse`]. It is the same logic whether
//! invoked over the socket (production) or in-process (these tests) — keeping the
//! transport an orthogonal concern.

use glm_fs_common::FileAttr;
use glm_ipc::fs::{FsAttr, FsEntry, FsKind, FsRequest, FsResponse};
use glm_workspace::EntryKind;

use crate::FskitOps;

fn fs_kind(kind: EntryKind) -> FsKind {
    match kind {
        EntryKind::File { executable: true } => FsKind::Executable,
        EntryKind::File { .. } => FsKind::File,
        EntryKind::Dir => FsKind::Dir,
        EntryKind::Symlink => FsKind::Symlink,
        EntryKind::Gitlink => FsKind::Gitlink,
    }
}

fn fs_attr(a: &FileAttr) -> FsAttr {
    FsAttr {
        ino: a.ino,
        generation: a.generation,
        size: a.size,
        kind: fs_kind(a.kind),
        mode: a.unix_mode,
    }
}

fn err(e: &glm_core::Error) -> FsResponse {
    FsResponse::Err {
        errno: e.errno(),
        message: format!("{e}"),
    }
}

impl FskitOps {
    /// Serve one per-inode filesystem request (ADR 0008).
    ///
    /// Infallible at this layer: an engine error becomes
    /// [`FsResponse::Err`]`{ errno, message }`, which the extension returns
    /// straight to the kernel. The exact recorded name/target/data bytes are
    /// preserved end-to-end (spec §41).
    pub fn serve_ipc(&self, req: &FsRequest) -> FsResponse {
        match req {
            FsRequest::Lookup { parent, name } => match self.lookup(*parent, name) {
                Ok(a) => FsResponse::Attr(fs_attr(&a)),
                Err(e) => err(&e),
            },
            FsRequest::GetAttr { ino } => match self.getattr(*ino) {
                Ok(a) => FsResponse::Attr(fs_attr(&a)),
                Err(e) => err(&e),
            },
            FsRequest::Read { ino, offset, size } => match self.read(*ino, *offset, *size) {
                Ok(bytes) => FsResponse::Data(bytes),
                Err(e) => err(&e),
            },
            FsRequest::Readlink { ino } => match self.readlink(*ino) {
                Ok(bytes) => FsResponse::Data(bytes),
                Err(e) => err(&e),
            },
            FsRequest::Enumerate { ino } => match self.enumerate(*ino) {
                Ok(entries) => FsResponse::Entries(
                    entries
                        .iter()
                        .map(|e| FsEntry {
                            ino: e.ino,
                            name: e.name.clone(),
                            attr: fs_attr(&e.attr),
                        })
                        .collect(),
                ),
                Err(e) => err(&e),
            },
            FsRequest::Create {
                parent,
                name,
                executable,
            } => match self.create(*parent, name, *executable) {
                Ok(a) => FsResponse::Attr(fs_attr(&a)),
                Err(e) => err(&e),
            },
            FsRequest::Symlink {
                parent,
                name,
                target,
            } => match self.symlink(*parent, name, target) {
                Ok(a) => FsResponse::Attr(fs_attr(&a)),
                Err(e) => err(&e),
            },
            FsRequest::Write { ino, offset, data } => match self.write(*ino, *offset, data) {
                Ok(n) => FsResponse::Written(n),
                Err(e) => err(&e),
            },
            FsRequest::Truncate { ino, len } => match self.truncate(*ino, *len) {
                Ok(()) => FsResponse::Done,
                Err(e) => err(&e),
            },
            FsRequest::SetExecutable { ino, executable } => {
                match self.set_executable(*ino, *executable) {
                    Ok(()) => FsResponse::Done,
                    Err(e) => err(&e),
                }
            }
            FsRequest::Remove { parent, name } => match self.remove(*parent, name) {
                Ok(()) => FsResponse::Done,
                Err(e) => err(&e),
            },
            FsRequest::Rename {
                parent,
                name,
                new_parent,
                new_name,
            } => match self.rename(*parent, name, *new_parent, new_name) {
                Ok(()) => FsResponse::Done,
                Err(e) => err(&e),
            },
            FsRequest::Forget { ino, nlookup } => {
                self.forget(*ino, *nlookup);
                FsResponse::Done
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glm_git_store::{FetchOptions, GitStore};
    use glm_ipc::fs::{FsKind, FsRequest, FsResponse};
    use glm_object_provider::{GitObjectProvider, ObjectProvider};
    use glm_workspace::{Workspace, WorkspaceConfig};

    use crate::FskitOps;

    fn ops_with(
        files: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, glm_testkit::SeededRemote, FskitOps) {
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
            workspace_head_ref: "refs/lazy-mount/workspaces/ipc/head".into(),
            attached_branch: None,
            remote: Some("origin".into()),
            identity: None,
        };
        let ws = Workspace::open_or_create(store, provider, tmp.path(), cfg, Some(base)).unwrap();
        (tmp, remote, FskitOps::new(ws))
    }

    const ROOT: u64 = glm_fs_common::ROOT_INO;

    #[test]
    fn lookup_read_and_enumerate_over_ipc() {
        let (_t, _r, ops) = ops_with(&[("a.txt", b"hello\n"), ("src/lib.rs", b"x\n")]);

        // Lookup a.txt → Attr.
        let a = match ops.serve_ipc(&FsRequest::Lookup {
            parent: ROOT,
            name: b"a.txt".to_vec(),
        }) {
            FsResponse::Attr(a) => a,
            other => panic!("expected Attr, got {other:?}"),
        };
        assert_eq!(a.size, 6);
        assert_eq!(a.kind, FsKind::File);

        // Read it (lazy hydration through the engine).
        match ops.serve_ipc(&FsRequest::Read {
            ino: a.ino,
            offset: 0,
            size: 64,
        }) {
            FsResponse::Data(d) => assert_eq!(d, b"hello\n"),
            other => panic!("expected Data, got {other:?}"),
        }

        // Enumerate the root.
        match ops.serve_ipc(&FsRequest::Enumerate { ino: ROOT }) {
            FsResponse::Entries(e) => {
                let names: Vec<_> = e.iter().map(|x| x.name.clone()).collect();
                assert!(names.contains(&b"a.txt".to_vec()));
                assert!(names.contains(&b"src".to_vec()));
            }
            other => panic!("expected Entries, got {other:?}"),
        }
    }

    #[test]
    fn create_write_and_error_map_over_ipc() {
        let (_t, _r, ops) = ops_with(&[("a.txt", b"hi\n")]);

        // Create + write a new file.
        let attr = match ops.serve_ipc(&FsRequest::Create {
            parent: ROOT,
            name: b"new.txt".to_vec(),
            executable: false,
        }) {
            FsResponse::Attr(a) => a,
            other => panic!("expected Attr, got {other:?}"),
        };
        match ops.serve_ipc(&FsRequest::Write {
            ino: attr.ino,
            offset: 0,
            data: b"world\n".to_vec(),
        }) {
            FsResponse::Written(n) => assert_eq!(n, 6),
            other => panic!("expected Written, got {other:?}"),
        }
        match ops.serve_ipc(&FsRequest::Read {
            ino: attr.ino,
            offset: 0,
            size: 64,
        }) {
            FsResponse::Data(d) => assert_eq!(d, b"world\n"),
            other => panic!("expected Data, got {other:?}"),
        }

        // A missing lookup maps to an errno (ENOENT = 2), not a panic.
        match ops.serve_ipc(&FsRequest::Lookup {
            parent: ROOT,
            name: b"nope.txt".to_vec(),
        }) {
            FsResponse::Err { errno, .. } => assert_eq!(errno, 2),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
