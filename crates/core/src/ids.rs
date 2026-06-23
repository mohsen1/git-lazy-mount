//! Workspace, operation, repository, and generation identifiers (spec §10).
//!
//! These are deliberately *distinct* newtypes so the type system prevents, say,
//! passing an operation id where a workspace id is expected.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifies an immutable workspace view (spec §2.4, §11).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceViewId(pub Vec<u8>);

/// Identifies an entry in the operation log (spec §13).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OperationId(pub Vec<u8>);

/// Stable identity of a mount/workspace on this machine.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub String);

/// Stable identity of a shared repository store, derived from canonical
/// repository identity *without embedding credentials* (spec §8).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoId(pub String);

/// Monotonic generation counter for a mount's desired/applied filesystem state
/// (spec §2.5, §19). Used to detect stale kernel entries and stale views.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MountGeneration(pub u64);

impl MountGeneration {
    /// The initial generation of a freshly created mount.
    pub const ZERO: MountGeneration = MountGeneration(0);

    /// The next generation after this one.
    pub fn next(self) -> MountGeneration {
        MountGeneration(self.0 + 1)
    }
}

macro_rules! hex_id_display {
    ($t:ty) => {
        impl $t {
            /// Lowercase hex of the underlying bytes.
            pub fn to_hex(&self) -> String {
                hex::encode(&self.0)
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($t), self.to_hex())
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }
    };
}

hex_id_display!(WorkspaceViewId);
hex_id_display!(OperationId);

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorkspaceId({})", self.0)
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Debug for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoId({})", self.0)
    }
}

impl fmt::Display for MountGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen{}", self.0)
    }
}
impl fmt::Debug for MountGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MountGeneration({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_monotonic() {
        let g = MountGeneration::ZERO;
        assert_eq!(g.next(), MountGeneration(1));
        assert!(g < g.next());
    }

    #[test]
    fn distinct_id_types() {
        let v = WorkspaceViewId(vec![0xab, 0xcd]);
        assert_eq!(v.to_hex(), "abcd");
    }
}
