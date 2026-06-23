//! Three-tree status types (spec §11): `X` = staged vs HEAD, `Y` = working
//! vs staged.

use glm_core::RepoPath;
use serde::{Deserialize, Serialize};

/// A per-side status code, mirroring Git's porcelain XY model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    /// No change on this side.
    Unmodified,
    /// Content (or mode) differs.
    Modified,
    /// Present here but not on the compared side.
    Added,
    /// Absent here but present on the compared side.
    Deleted,
    /// Entry type changed (e.g. file -> symlink).
    TypeChanged,
}

impl StatusCode {
    /// Single-letter Git-style code.
    pub fn letter(&self) -> char {
        match self {
            StatusCode::Unmodified => '.',
            StatusCode::Modified => 'M',
            StatusCode::Added => 'A',
            StatusCode::Deleted => 'D',
            StatusCode::TypeChanged => 'T',
        }
    }
}

/// One changed path's status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    /// The path.
    pub path: RepoPath,
    /// Staged-vs-HEAD code.
    pub index: StatusCode,
    /// Working-vs-staged code.
    pub worktree: StatusCode,
}

impl StatusEntry {
    /// Whether this entry represents any change at all.
    pub fn is_changed(&self) -> bool {
        self.index != StatusCode::Unmodified || self.worktree != StatusCode::Unmodified
    }
}
