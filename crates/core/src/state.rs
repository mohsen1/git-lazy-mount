//! Orthogonal per-path state (spec §2.6, §12).
//!
//! The spec is emphatic: do **not** collapse these dimensions into one
//! `hydrated` boolean. A path's *source*, its *repository semantics*, its
//! *residency* (how much is locally present), and its *durability* are
//! independent axes. This module gives each its own type.

use serde::{Deserialize, Serialize};

/// Where a path's working-tree content currently comes from (spec §11 ordering).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Backed by the committed base tree.
    Base,
    /// Backed by an overlay object (locally written bytes).
    Overlay,
    /// A tombstone: the path is deleted in the working tree.
    Tombstone,
    /// A structured conflict record.
    Conflict,
    /// A submodule gitlink.
    Gitlink,
    /// An untracked path served from a native-disk redirection.
    NativeRedirect,
}

/// The semantic status of a path relative to HEAD/stage (spec §12, §11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticStatus {
    /// Identical to its committed form.
    Clean,
    /// Content differs.
    Modified,
    /// Newly added (not in base).
    New,
    /// Deleted relative to base.
    Deleted,
    /// Only the executable bit / mode changed.
    ModeChanged,
    /// The entry type changed (e.g. file -> symlink).
    TypeChanged,
    /// Renamed from another path.
    Renamed,
    /// Copied from another path.
    Copied,
    /// In a conflicted state.
    Conflicted,
}

/// How much of a path is locally present (spec §12 "Residency").
///
/// Every axis is independent. Crucially, "not resident" never implies a size of
/// zero and never implies "clean" or "modified".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residency {
    /// Whether this path's directory tree metadata is cached.
    pub tree_metadata_cached: bool,
    /// Whether the raw Git blob is cached locally.
    pub raw_blob_cached: bool,
    /// Whether the filtered working-tree content is cached locally.
    pub filtered_content_cached: bool,
    /// Whether an inode has been loaded for this path.
    pub inode_loaded: bool,
    /// Whether the OS has a placeholder/projection present.
    pub os_placeholder_present: bool,
    /// Whether overlay bytes exist on disk for this path.
    pub overlay_bytes_present: bool,
}

impl Default for Residency {
    fn default() -> Self {
        Residency::unloaded()
    }
}

impl Residency {
    /// Nothing materialized yet.
    pub fn unloaded() -> Self {
        Residency {
            tree_metadata_cached: false,
            raw_blob_cached: false,
            filtered_content_cached: false,
            inode_loaded: false,
            os_placeholder_present: false,
            overlay_bytes_present: false,
        }
    }

    /// Whether *any* byte content (raw, filtered, or overlay) is locally present.
    /// This is what "materialized" loosely means — and it is NOT the same as
    /// "modified" (see [`SemanticStatus`]).
    pub fn is_materialized(&self) -> bool {
        self.raw_blob_cached || self.filtered_content_cached || self.overlay_bytes_present
    }
}

/// Durability level reached by a mutation (spec §12 "Durability", ordered).
///
/// Ordering matters: a higher level implies all lower guarantees. The current
/// view pointer is only advanced once the relevant records reach
/// [`Durability::MetadataCommitted`] (spec §13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Durability {
    /// Exists only in process memory.
    InMemory,
    /// Appended to the journal but not yet fsynced.
    Journaled,
    /// File data has been fsynced.
    DataFsynced,
    /// State records fsynced and `CURRENT` advanced.
    MetadataCommitted,
    /// Wrapped into a sealed, immutable operation-log entry.
    OperationSealed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residency_default_is_unloaded() {
        let r = Residency::default();
        assert!(!r.is_materialized());
        assert!(!r.raw_blob_cached);
    }

    #[test]
    fn materialized_is_not_modified() {
        // A file can be fully resident yet semantically clean.
        let mut r = Residency::unloaded();
        r.overlay_bytes_present = true;
        assert!(r.is_materialized());
        // SemanticStatus is a separate axis; residency says nothing about it.
        let _ = SemanticStatus::Clean;
    }

    #[test]
    fn durability_is_ordered() {
        assert!(Durability::InMemory < Durability::Journaled);
        assert!(Durability::MetadataCommitted < Durability::OperationSealed);
    }
}
