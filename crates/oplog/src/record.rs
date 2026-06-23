//! Operation records (spec §13).

use glm_core::{Durability, OperationId, WorkspaceViewId};
use serde::{Deserialize, Serialize};

/// What caused an operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cause {
    /// A `git lazy-mount` command (with its argv summary).
    Command(String),
    /// A filesystem callback (with a short description).
    Filesystem(String),
    /// Internal maintenance/recovery.
    Internal(String),
}

/// A record of an external side effect that is NOT part of the local atomic
/// transaction (spec §13: pushes are retryable saga steps, not undoable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSideEffect {
    /// Kind of effect, e.g. `push`, `create-remote-branch`.
    pub kind: String,
    /// The target, e.g. `origin refs/heads/main` (redacted; never a URL with
    /// credentials).
    pub target: String,
    /// Saga state: `preflight` | `prepared` | `remote-done` | `acknowledged`.
    pub state: String,
}

/// An append-only operation-log entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// This operation's id.
    pub id: OperationId,
    /// Parent operation ids (the operation DAG; spec §13).
    pub parents: Vec<OperationId>,
    /// The view this operation produced.
    pub view: WorkspaceViewId,
    /// Unix epoch seconds when the operation was recorded.
    pub timestamp_unix: i64,
    /// User identity (best-effort; e.g. `$USER`).
    pub user: String,
    /// Hostname (best-effort).
    pub hostname: String,
    /// Process id that created the operation.
    pub pid: u32,
    /// What caused the operation.
    pub cause: Cause,
    /// Human-readable description.
    pub description: String,
    /// Durability reached (spec §12 durability axis).
    pub durability: Durability,
    /// External side-effect records (spec §13).
    pub external_effects: Vec<ExternalSideEffect>,
}
