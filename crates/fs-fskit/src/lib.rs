//! `glm-fs-fskit` — macOS FSKit backend (spec §41).
//!
//! Status: the **backend-independent logic is built and unit-tested on every
//! platform**; the remaining macOS-specific piece is the on-device FSKit
//! `FSVolume` Swift adapter plus its signed system extension, which can only be
//! validated on real Apple hardware (spec §54, tracked by issue #12). macOS is
//! therefore **not yet labeled supported** — a green default CI never implies a
//! working macOS mount.
//!
//! What lives here, shared with the other platform backends via
//! [`glm_fs_common`] and [`glm_workspace`]:
//!
//! * [`FskitOps`] — the FSKit `FSVolume` callback logic (the macOS analog of
//!   [`glm_fs_fuse::FuseOps`]); every write routes through the same workspace
//!   overlay / staging / operation-log path (no macOS-only semantics).
//! * [`Capability`] — runtime capability detection and installation diagnostics
//!   (the seam the CLI uses to fall back to headless mode).
//! * [`MacBackend`] — the explicit FSKit-vs-macFUSE backend selection; the two
//!   are distinct boundaries that never silently change semantics.
//! * [`collision`] — APFS case-/normalization-collision detection over the
//!   directory the bridge enumerates (issue #7), reusing the platform folding.

#![forbid(unsafe_code)]

mod bridge;
mod capability;
pub mod collision;

use std::path::Path;

use glm_core::{Error, ErrorCode, Result};

pub use bridge::{EnumerateEntry, FskitOps};
pub use capability::{Capability, MacBackend, FSKIT_MODULE_BUNDLE_ID};
pub use collision::{AppleVolume, Collision};

/// Whether a usable FSKit (or macFUSE) backend is available at runtime.
///
/// This now *probes* the host (see [`Capability::detect`]) rather than returning
/// a hardcoded answer: it is `true` only when an installed, approved FSKit
/// extension — or an installed macFUSE — is actually present.
pub fn backend_available() -> bool {
    Capability::detect().is_usable()
}

/// The probed backend capability of this host (capability + diagnostics).
pub fn capability() -> Capability {
    Capability::detect()
}

/// Attempt to mount `ops` at `mountpoint`.
///
/// The on-device FSKit `FSVolume` adapter is not part of this build, so this
/// returns a structured error — but when no backend is available it carries the
/// *concrete, probed* installation/approval steps (issue #6) instead of a bare
/// "not implemented".
pub fn mount(_ops: FskitOps, mountpoint: &Path) -> Result<()> {
    let cap = Capability::detect();
    let (summary, action) = if let Some(backend) = cap.selected_backend() {
        (
            format!(
                "a {} backend is available but the on-device FSVolume adapter is not built into \
                 this binary; cannot kernel-mount at {} yet (tracked by issue #12)",
                backend.label(),
                mountpoint.display()
            ),
            "use the headless CLI; on-device mounting lands with the validation harness"
                .to_string(),
        )
    } else {
        (
            format!(
                "no usable macOS filesystem backend is available; cannot mount at {}",
                mountpoint.display()
            ),
            cap.diagnostics().join("; "),
        )
    };
    Err(Error::new(ErrorCode::FilesystemBackendUnavailable, summary).with_action(action))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_available_matches_capability() {
        assert_eq!(backend_available(), Capability::detect().is_usable());
    }

    #[test]
    fn mount_unavailable_carries_actionable_diagnostics() {
        // Build ops over a throwaway workspace just to exercise `mount`'s error
        // surface; the call must fail with a backend-unavailable code and a
        // non-empty, actionable hint (never a bare "not implemented yet").
        let err = mount_error_for_test();
        assert_eq!(err.code, ErrorCode::FilesystemBackendUnavailable);
        let action = err.recommended_action.clone().unwrap_or_default();
        assert!(!action.is_empty(), "mount failure must recommend an action");
        // On hosts without a backend, the diagnostic must mention the headless
        // fallback so the user is never stuck.
        if !Capability::detect().is_usable() {
            assert!(action.contains("headless"));
        }
    }

    fn mount_error_for_test() -> Error {
        use glm_git_store::{FetchOptions, GitStore};
        use glm_object_provider::{GitObjectProvider, ObjectProvider};
        use glm_workspace::{Workspace, WorkspaceConfig};
        use std::sync::Arc;

        let remote = glm_testkit::seed_remote(&[("a", b"b")]);
        let tmp = tempfile::tempdir().unwrap();
        let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
        store.set_config("protocol.file.allow", "always").unwrap();
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
            workspace_head_ref: "refs/lazy-mount/workspaces/t/head".into(),
            attached_branch: None,
            remote: Some("origin".into()),
            identity: None,
        };
        let ws = Workspace::open_or_create(store, provider, tmp.path(), cfg, Some(base)).unwrap();
        mount(FskitOps::new(ws), Path::new("/tmp/glm-fskit-test")).unwrap_err()
    }
}
