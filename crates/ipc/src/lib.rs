//! `glm-ipc` — the versioned daemon control protocol (spec §39).
//!
//! These are the wire message types for the per-user daemon's control channel
//! (Unix domain socket on Linux/macOS, a secured local mechanism on Windows).
//! The protocol is explicitly versioned so client and daemon can detect
//! mismatches. The transport itself is provided by the daemon; this crate
//! defines only the (serde) message shapes so they can evolve under test.

#![forbid(unsafe_code)]

use glm_core::ErrorJson;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The control protocol version. Bump on any incompatible message change.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    /// Protocol version the client speaks.
    pub protocol_version: u32,
    /// Correlation id (echoed in the response).
    pub id: u64,
    /// The operation to perform.
    pub op: RequestOp,
}

/// The control operations (a representative subset of spec §39).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum RequestOp {
    /// Mount a local repository at a mountpoint.
    Mount {
        /// Local repository path or URL.
        source: String,
        /// Mountpoint path.
        mountpoint: String,
    },
    /// Unmount a mountpoint.
    Unmount {
        /// Mountpoint path.
        mountpoint: String,
    },
    /// Workspace status for a mountpoint.
    Status {
        /// Mountpoint path.
        mountpoint: String,
    },
    /// A synchronization barrier of the given mode.
    SyncBarrier {
        /// Mountpoint path.
        mountpoint: String,
        /// Barrier mode: `no-wait` | `best-effort` | `barrier`.
        mode: String,
    },
    /// The current workspace generation.
    WorkspaceGeneration {
        /// Mountpoint path.
        mountpoint: String,
    },
    /// Resolve a path's Git object id / content hash (SCM-aware API; spec §39).
    HashLookup {
        /// Mountpoint path.
        mountpoint: String,
        /// Repo-relative path (escaped).
        path: String,
    },
    /// Prefetch content for a set of paths.
    Prefetch {
        /// Mountpoint path.
        mountpoint: String,
        /// Escaped repo-relative paths.
        paths: Vec<String>,
    },
    /// Cache statistics for a mountpoint.
    CacheStats {
        /// Mountpoint path.
        mountpoint: String,
    },
    /// Refresh credentials for a repository (re-auth without remounting).
    CredentialRefresh {
        /// Repository id.
        repo_id: String,
    },
    /// Daemon health.
    Health,
}

/// A response envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    /// Protocol version the daemon speaks.
    pub protocol_version: u32,
    /// Correlation id from the request.
    pub id: u64,
    /// The result.
    pub result: ResponseResult,
}

/// Either a successful payload or a structured error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseResult {
    /// Success with an operation-specific JSON payload.
    Ok(Value),
    /// A structured error (spec §47).
    Err(ErrorJson),
}

impl Request {
    /// Build a request at the current protocol version.
    pub fn new(id: u64, op: RequestOp) -> Request {
        Request {
            protocol_version: PROTOCOL_VERSION,
            id,
            op,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let req = Request::new(
            7,
            RequestOp::Status {
                mountpoint: "/work/repo".into(),
            },
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn response_error_roundtrips() {
        let resp = Response {
            protocol_version: PROTOCOL_VERSION,
            id: 7,
            result: ResponseResult::Err(ErrorJson {
                code: "offline_missing_object".into(),
                summary: "x".into(),
                retryable: true,
                recommended_action: None,
                workspace_id: None,
                operation_id: None,
                causes: vec![],
                context: vec![],
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }
}
