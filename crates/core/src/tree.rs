//! Parsed Git tree objects.

use serde::{Deserialize, Serialize};

use crate::mode::GitMode;
use crate::object_id::ObjectId;

/// A single entry within a Git tree.
///
/// `name` is raw bytes (a single path component; never contains `/` or NUL).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// The component name, raw bytes.
    pub name: Vec<u8>,
    /// The entry's mode.
    pub mode: GitMode,
    /// The object the entry points at (a blob, subtree, or commit for gitlinks).
    pub object_id: ObjectId,
}

impl TreeEntry {
    /// Lossy display of the entry name for humans/logs.
    pub fn name_display(&self) -> String {
        String::from_utf8_lossy(&self.name).into_owned()
    }
}

/// A parsed tree object: the object id it was parsed from plus its entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeObject {
    /// The tree's own object id.
    pub id: ObjectId,
    /// Entries, in Git's canonical sort order.
    pub entries: Vec<TreeEntry>,
}

impl TreeObject {
    /// Find an entry by exact name bytes.
    pub fn entry(&self, name: &[u8]) -> Option<&TreeEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Number of directly-contained entries (used to assert `readdir` is
    /// O(entries in this directory)).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
