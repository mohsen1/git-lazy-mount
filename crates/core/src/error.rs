//! Typed error model with stable machine-readable codes.
//!
//! Every error carries a stable [`ErrorCode`], a human summary, a `retryable`
//! flag, an optional recommended action, optional workspace/operation ids, a
//! redacted diagnostic breadcrumb trail, and a causal chain. The FS layer maps
//! codes to errno *without losing* the structured diagnostic.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{OperationId, WorkspaceId};

/// Stable, machine-readable error category. The string form (`as_str`) is part
/// of the public contract and must not change for a given variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Invalid configuration / arguments.
    Configuration,
    /// The remote does not support a required capability (e.g. a filter).
    UnsupportedRemoteCapability,
    /// Authentication is required or has expired.
    Authentication,
    /// Object is unavailable because we are offline.
    OfflineMissingObject,
    /// The remote genuinely does not have the requested object.
    RemoteMissingObject,
    /// A locally stored object failed integrity verification.
    LocalObjectCorruption,
    /// A clean/smudge filter failed.
    FilterFailure,
    /// A Git LFS operation failed.
    LfsFailure,
    /// A repository path is invalid.
    InvalidRepositoryPath,
    /// A path cannot be represented on this platform (collision/reserved name).
    PlatformPathCollision,
    /// The FUSE filesystem backend is unavailable.
    FilesystemBackendUnavailable,
    /// The workspace's applied generation is behind the desired one.
    StaleWorkspace,
    /// A branch moved underneath us (compare-and-swap failed).
    ConcurrentBranchMovement,
    /// An operation requires a clean workspace but it is dirty.
    DirtyWorkspaceConflict,
    /// Overlay storage is corrupt.
    OverlayCorruption,
    /// A mount lifecycle precondition was violated.
    MountLifecycle,
    /// The requested operation is not supported (by design).
    UnsupportedOperation,
    /// A configured resource limit was exceeded.
    ResourceLimit,
    /// A path does not exist (maps to `ENOENT`).
    NotFound,
    /// A path already exists where a fresh one was required (maps to `EEXIST`).
    AlreadyExists,
    /// An unexpected internal invariant violation (bug).
    Internal,
}

impl ErrorCode {
    /// The stable string code.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Configuration => "configuration",
            ErrorCode::UnsupportedRemoteCapability => "unsupported_remote_capability",
            ErrorCode::Authentication => "authentication",
            ErrorCode::OfflineMissingObject => "offline_missing_object",
            ErrorCode::RemoteMissingObject => "remote_missing_object",
            ErrorCode::LocalObjectCorruption => "local_object_corruption",
            ErrorCode::FilterFailure => "filter_failure",
            ErrorCode::LfsFailure => "lfs_failure",
            ErrorCode::InvalidRepositoryPath => "invalid_repository_path",
            ErrorCode::PlatformPathCollision => "platform_path_collision",
            ErrorCode::FilesystemBackendUnavailable => "filesystem_backend_unavailable",
            ErrorCode::StaleWorkspace => "stale_workspace",
            ErrorCode::ConcurrentBranchMovement => "concurrent_branch_movement",
            ErrorCode::DirtyWorkspaceConflict => "dirty_workspace_conflict",
            ErrorCode::OverlayCorruption => "overlay_corruption",
            ErrorCode::MountLifecycle => "mount_lifecycle",
            ErrorCode::UnsupportedOperation => "unsupported_operation",
            ErrorCode::ResourceLimit => "resource_limit",
            ErrorCode::NotFound => "not_found",
            ErrorCode::AlreadyExists => "already_exists",
            ErrorCode::Internal => "internal",
        }
    }

    /// Default retryability for this category. Individual errors may override.
    pub fn default_retryable(&self) -> bool {
        matches!(
            self,
            ErrorCode::Authentication
                | ErrorCode::OfflineMissingObject
                | ErrorCode::ConcurrentBranchMovement
                | ErrorCode::ResourceLimit
        )
    }

    /// The errno a filesystem callback should surface for this category.
    /// (Numeric, to avoid a `libc` dependency in `core`.)
    pub fn errno(&self) -> i32 {
        // Values are the standard Linux errno numbers; the FS layer is free to
        // remap per-platform, but these are the defaults.
        match self {
            ErrorCode::Authentication => 13,               // EACCES
            ErrorCode::OfflineMissingObject => 5,          // EIO
            ErrorCode::RemoteMissingObject => 2,           // ENOENT
            ErrorCode::LocalObjectCorruption => 5,         // EIO
            ErrorCode::OverlayCorruption => 5,             // EIO
            ErrorCode::FilterFailure => 5,                 // EIO
            ErrorCode::LfsFailure => 5,                    // EIO
            ErrorCode::InvalidRepositoryPath => 22,        // EINVAL
            ErrorCode::PlatformPathCollision => 22,        // EINVAL
            ErrorCode::FilesystemBackendUnavailable => 19, // ENODEV
            ErrorCode::UnsupportedOperation => 95,         // EOPNOTSUPP
            ErrorCode::ResourceLimit => 28,                // ENOSPC
            ErrorCode::NotFound => 2,                      // ENOENT
            ErrorCode::AlreadyExists => 17,                // EEXIST
            ErrorCode::StaleWorkspace => 116,              // ESTALE
            ErrorCode::DirtyWorkspaceConflict => 39,       // ENOTEMPTY (closest)
            ErrorCode::ConcurrentBranchMovement => 11,     // EAGAIN
            ErrorCode::Configuration => 22,                // EINVAL
            ErrorCode::UnsupportedRemoteCapability => 95,  // EOPNOTSUPP
            ErrorCode::MountLifecycle => 16,               // EBUSY
            ErrorCode::Internal => 5,                      // EIO
        }
    }
}

/// The crate-wide result type.
pub type Result<T> = std::result::Result<T, Error>;

/// A structured error.
#[derive(Debug)]
pub struct Error(Box<ErrorRepr>);

/// The boxed representation behind [`Error`]. Boxing keeps `Result<T, Error>`
/// pointer-sized, so the common `Ok` path is cheap and `clippy::result_large_err`
/// stays satisfied. Field access on [`Error`] works transparently via `Deref`.
#[derive(Debug)]
pub struct ErrorRepr {
    /// Stable category code.
    pub code: ErrorCode,
    /// One-line human summary (must not contain secrets).
    pub summary: String,
    /// Whether retrying the operation may succeed.
    pub retryable: bool,
    /// A concrete action the user can take, if any.
    pub recommended_action: Option<String>,
    /// Owning workspace, if known.
    pub workspace_id: Option<WorkspaceId>,
    /// Owning operation, if known.
    pub operation_id: Option<OperationId>,
    /// Redacted diagnostic breadcrumbs (most recent last).
    pub context: Vec<String>,
    /// Underlying cause, if any.
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl std::ops::Deref for Error {
    type Target = ErrorRepr;
    fn deref(&self) -> &ErrorRepr {
        &self.0
    }
}

impl Error {
    /// Construct a new error from a code and summary, using the code's default
    /// retryability.
    pub fn new(code: ErrorCode, summary: impl Into<String>) -> Self {
        Error(Box::new(ErrorRepr {
            code,
            summary: summary.into(),
            retryable: code.default_retryable(),
            recommended_action: None,
            workspace_id: None,
            operation_id: None,
            context: Vec::new(),
            source: None,
        }))
    }

    /// Convenience constructor for [`ErrorCode::Internal`].
    pub fn internal(summary: impl Into<String>) -> Self {
        Error::new(ErrorCode::Internal, summary)
    }

    /// Convenience constructor for [`ErrorCode::UnsupportedOperation`].
    pub fn unsupported(summary: impl Into<String>) -> Self {
        Error::new(ErrorCode::UnsupportedOperation, summary)
    }

    /// Override retryability.
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.0.retryable = retryable;
        self
    }

    /// Attach a recommended action.
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.0.recommended_action = Some(action.into());
        self
    }

    /// Attach the owning workspace id.
    pub fn with_workspace(mut self, id: WorkspaceId) -> Self {
        self.0.workspace_id = Some(id);
        self
    }

    /// Attach the owning operation id.
    pub fn with_operation(mut self, id: OperationId) -> Self {
        self.0.operation_id = Some(id);
        self
    }

    /// Push a diagnostic breadcrumb (caller must ensure it is redacted).
    pub fn context(mut self, ctx: impl Into<String>) -> Self {
        self.0.context.push(ctx.into());
        self
    }

    /// Attach an underlying cause.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.0.source = Some(Box::new(source));
        self
    }

    /// The errno the FS layer should surface, preserving `self` for the daemon
    /// diagnostic.
    pub fn errno(&self) -> i32 {
        self.code.errno()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.summary)?;
        if let Some(action) = &self.recommended_action {
            write!(f, " (action: {action})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(ErrorCode::Internal, format!("io error: {e}")).with_source(e)
    }
}

/// JSON-serializable projection of an [`Error`] for `--json` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorJson {
    /// Stable code.
    pub code: String,
    /// Human summary.
    pub summary: String,
    /// Whether retrying may help.
    pub retryable: bool,
    /// Recommended action, if any.
    pub recommended_action: Option<String>,
    /// Owning workspace id (string form), if any.
    pub workspace_id: Option<String>,
    /// Owning operation id (hex), if any.
    pub operation_id: Option<String>,
    /// Causal chain, summarized.
    pub causes: Vec<String>,
    /// Redacted diagnostic breadcrumbs.
    pub context: Vec<String>,
}

impl Error {
    /// Render to the JSON projection.
    pub fn to_json(&self) -> ErrorJson {
        let mut causes = Vec::new();
        let mut cur: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(self);
        while let Some(c) = cur {
            causes.push(c.to_string());
            cur = c.source();
        }
        ErrorJson {
            code: self.code.as_str().to_string(),
            summary: self.summary.clone(),
            retryable: self.retryable,
            recommended_action: self.recommended_action.clone(),
            workspace_id: self.workspace_id.as_ref().map(|w| w.0.clone()),
            operation_id: self.operation_id.as_ref().map(|o| o.to_hex()),
            causes,
            context: self.context.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_strings() {
        assert_eq!(
            ErrorCode::OfflineMissingObject.as_str(),
            "offline_missing_object"
        );
        assert_eq!(ErrorCode::StaleWorkspace.as_str(), "stale_workspace");
    }

    #[test]
    fn builder_sets_fields() {
        let e = Error::new(ErrorCode::Authentication, "token expired")
            .with_action("run: git lazy-mount doctor")
            .context("during fetch of refs/heads/main");
        assert!(e.retryable); // auth is retryable by default
        assert_eq!(
            e.recommended_action.as_deref(),
            Some("run: git lazy-mount doctor")
        );
        assert_eq!(e.errno(), 13); // EACCES
        assert_eq!(e.context.len(), 1);
    }

    #[test]
    fn json_includes_causes() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let e = Error::new(ErrorCode::LocalObjectCorruption, "bad object").with_source(io);
        let j = e.to_json();
        assert_eq!(j.code, "local_object_corruption");
        assert_eq!(j.causes.len(), 1);
        assert!(j.causes[0].contains("nope"));
    }

    #[test]
    fn offline_maps_to_eio() {
        assert_eq!(ErrorCode::OfflineMissingObject.errno(), 5);
        assert!(ErrorCode::OfflineMissingObject.default_retryable());
    }
}
