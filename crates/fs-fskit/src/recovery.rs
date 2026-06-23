//! Mount recovery after a daemon or extension restart (issue #11, spec §39, §41).
//!
//! If the FSKit extension or the controlling daemon restarts, the mount must
//! re-attach to a **consistent** state with no data loss. This rides on the
//! engine's crash-safe operation log (`glm-oplog`) and the persisted overlay /
//! stage: re-opening the workspace replays the log to its last sealed view, and
//! uncommitted working-tree edits survive because the overlay is durable.
//!
//! [`reattach`] drives the FSKit re-attach path through `Recovering → Mounted`
//! and returns a fresh [`FskitOps`] — fresh because, after a re-attach, the
//! kernel re-issues `lookup` for the paths it still references, so inode identity
//! is rebuilt on demand (numbers are never reused; spec §19). The induced-restart
//! validation through a real FSKit mount is on-device (issue #12).

use glm_core::{ObjectId, Result};
use glm_platform::validate::AppleVolume;
use glm_workspace::Workspace;

use crate::FskitOps;

/// The lifecycle phases a recovering FSKit mount drives through (a focused mirror
/// of `glm_daemon::MountState` for the re-attach path; spec §39).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPhase {
    /// Replaying the operation log to a consistent view.
    Recovering,
    /// Re-attached and serving again.
    Mounted,
    /// Recovery found an unrecoverable inconsistency; needs attention.
    Failed,
}

/// The outcome of a re-attach.
#[derive(Clone, Debug)]
pub struct ReattachReport {
    /// The operation log recovered to a healthy, consistent state.
    pub healthy: bool,
    /// The workspace was behind its sealed generation (a crash mid-operation).
    pub stale: bool,
    /// The base commit the mount re-attached at.
    pub base: Option<ObjectId>,
    /// Diagnostic notes from operation-log recovery.
    pub issues: Vec<String>,
    /// The phases driven through, in order (always starts at `Recovering`).
    pub phases: Vec<RecoveryPhase>,
}

/// Re-attach `ws` after the FSKit extension or controlling daemon restarted,
/// driving `Recovering → Mounted` (or `Failed`). Crash-safe via the operation
/// log; uncommitted overlay edits survive with no data loss.
pub fn reattach(ws: Workspace, volume: AppleVolume) -> Result<(FskitOps, ReattachReport)> {
    let mut phases = vec![RecoveryPhase::Recovering];
    let recovered = ws.oplog().recover()?;
    let base = ws.base_commit();
    phases.push(if recovered.healthy {
        RecoveryPhase::Mounted
    } else {
        RecoveryPhase::Failed
    });
    let report = ReattachReport {
        healthy: recovered.healthy,
        stale: recovered.stale,
        base,
        issues: recovered.issues,
        phases,
    };
    Ok((FskitOps::with_volume(ws, volume), report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use glm_core::{FetchPolicy, RepoPath};
    use glm_fs_common::ROOT_INO;
    use glm_git_store::{FetchOptions, GitStore};
    use glm_object_provider::{GitObjectProvider, ObjectProvider};
    use glm_workspace::WorkspaceConfig;

    const POLICY: FetchPolicy = FetchPolicy::AllowNetwork;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    /// A workspace whose on-disk state (store + ws_dir) outlives any single
    /// `FskitOps`, so we can simulate an extension/daemon restart by dropping the
    /// ops + workspace and re-opening from the same directories.
    struct Persistent {
        _tmp: tempfile::TempDir,
        _remote: glm_testkit::SeededRemote,
        store_dir: std::path::PathBuf,
        ws_dir: std::path::PathBuf,
    }

    impl Persistent {
        fn new(files: &[(&str, &[u8])]) -> Persistent {
            let remote = glm_testkit::seed_remote(files);
            let tmp = tempfile::tempdir().unwrap();
            let store_dir = tmp.path().join("git");
            let ws_dir = tmp.path().join("ws");
            let store = GitStore::init_bare(&store_dir, None).unwrap();
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
            // First open creates the workspace + seals the root view.
            let _ = open_ws(&store_dir, &ws_dir, Some(base));
            Persistent {
                _tmp: tmp,
                _remote: remote,
                store_dir,
                ws_dir,
            }
        }

        fn open(&self) -> Workspace {
            open_ws(&self.store_dir, &self.ws_dir, None)
        }
    }

    fn open_ws(
        store_dir: &std::path::Path,
        ws_dir: &std::path::Path,
        base: Option<ObjectId>,
    ) -> Workspace {
        let store = GitStore::open(store_dir).unwrap();
        let provider: Arc<dyn ObjectProvider> =
            Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
        let cfg = WorkspaceConfig {
            workspace_head_ref: "refs/lazy-mount/workspaces/t/head".into(),
            attached_branch: None,
            remote: Some("origin".into()),
            identity: None,
        };
        Workspace::open_or_create(store, provider, ws_dir, cfg, base).unwrap()
    }

    #[test]
    fn reattach_recovers_consistent_state_without_data_loss() {
        let persistent = Persistent::new(&[("a.txt", b"base\n")]);

        // Session 1: make an uncommitted edit through the FSKit write callbacks.
        {
            let ops = FskitOps::new(persistent.open());
            let attr = ops.create(ROOT_INO, b"draft.txt", false).unwrap();
            ops.write(attr.ino, 0, b"unsaved work\n").unwrap();
            // The extension/daemon now "crashes": drop everything.
        }

        // Session 2: re-attach from the same on-disk state.
        let (ops, report) = reattach(persistent.open(), AppleVolume::CaseInsensitive).unwrap();
        assert!(report.healthy, "oplog should recover cleanly");
        assert_eq!(
            report.phases,
            vec![RecoveryPhase::Recovering, RecoveryPhase::Mounted]
        );
        assert!(report.base.is_some());

        // No data loss: the uncommitted draft survives the restart (durable
        // overlay), and the base file is still readable.
        let draft = ops.lookup(ROOT_INO, b"draft.txt").unwrap();
        assert_eq!(ops.read(draft.ino, 0, 1024).unwrap(), b"unsaved work\n");
        let base = ops.lookup(ROOT_INO, b"a.txt").unwrap();
        assert_eq!(ops.read(base.ino, 0, 1024).unwrap(), b"base\n");
    }

    #[test]
    fn reattach_preserves_a_committed_base() {
        let persistent = Persistent::new(&[("a.txt", b"v1\n")]);

        // Session 1: edit, stage, and commit, then "crash".
        let committed = {
            let ws = persistent.open();
            let ops = FskitOps::new(ws);
            let attr = ops.create(ROOT_INO, b"added.txt", false).unwrap();
            ops.write(attr.ino, 0, b"new file\n").unwrap();
            ops.workspace().stage_path(&p("added.txt"), POLICY).unwrap();
            let out = ops.workspace().commit("add file", POLICY).unwrap();
            out.commit
        };

        // Session 2: re-attach; the base must be the committed revision.
        let (ops, report) = reattach(persistent.open(), AppleVolume::CaseInsensitive).unwrap();
        assert!(report.healthy);
        assert_eq!(report.base, Some(committed));
        // The committed file is present after re-attach.
        let added = ops.lookup(ROOT_INO, b"added.txt").unwrap();
        assert_eq!(ops.read(added.ino, 0, 1024).unwrap(), b"new file\n");
    }
}
