//! The immutable workspace view record (spec §2.4, §11).

use glm_core::{ObjectId, OperationId, WorkspaceViewId};
use serde::{Deserialize, Serialize};

/// A transactional, immutable snapshot of workspace-identifying state.
///
/// A view is written once and never mutated (spec §13). The current view
/// pointer advances only after the view and its operation are durable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    /// This view's id.
    pub id: WorkspaceViewId,
    /// The base commit the working tree is derived from.
    pub base_commit: Option<ObjectId>,
    /// The workspace-private head commit (protected from GC by a keep-ref).
    pub workspace_head: Option<ObjectId>,
    /// The public branch this workspace is attached to, if any.
    pub attached_branch: Option<String>,
    /// The expected current value of the attached branch (for CAS; spec §14).
    pub attached_branch_expected: Option<ObjectId>,
    /// Monotonic mount generation (spec §2.5, §19).
    pub mount_generation: u64,
    /// The operations that produced this view (usually one; more after a
    /// reconciliation/merge of divergent operation heads).
    pub parent_ops: Vec<OperationId>,
    /// Version of the path-mapping configuration in effect.
    pub path_mapping_version: u32,
    /// Version of the filter context in effect (invalidates filtered caches).
    pub filter_context_version: u32,
    /// Digest of the staged delta at this view (detects stage changes).
    pub stage_digest: Option<String>,
    /// Digest of the overlay entry set at this view (detects overlay changes).
    pub overlay_digest: Option<String>,
}

impl WorkspaceView {
    /// A fresh root view for a newly created workspace at `base`.
    pub fn root(id: WorkspaceViewId, base_commit: Option<ObjectId>) -> WorkspaceView {
        WorkspaceView {
            id,
            base_commit: base_commit.clone(),
            workspace_head: base_commit,
            attached_branch: None,
            attached_branch_expected: None,
            mount_generation: 0,
            parent_ops: Vec::new(),
            path_mapping_version: 1,
            filter_context_version: 1,
            stage_digest: None,
            overlay_digest: None,
        }
    }
}
