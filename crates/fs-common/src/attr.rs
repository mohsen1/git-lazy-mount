//! Backend-neutral file attributes (spec §28).
//!
//! Git tracks only file type and the executable bit; it does *not* track owner,
//! most permission bits, or timestamps. We therefore expose **stable synthetic**
//! metadata for clean unmaterialized entries and never treat a synthetic
//! timestamp difference as a modification (spec §28). Each backend converts this
//! to its native attribute shape.

use glm_workspace::EntryKind;

/// A neutral file-attribute view for projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileAttr {
    /// Inode number.
    pub ino: u64,
    /// Inode generation.
    pub generation: u64,
    /// Exact size in bytes (0 for directories).
    pub size: u64,
    /// Entry kind.
    pub kind: EntryKind,
    /// POSIX `st_mode` (type + conventional permission bits).
    pub unix_mode: u32,
}

impl FileAttr {
    /// Build attributes for `kind` with an exact `size`.
    pub fn new(ino: u64, generation: u64, kind: EntryKind, size: u64) -> FileAttr {
        FileAttr {
            ino,
            generation,
            size,
            kind,
            unix_mode: unix_mode_of(kind),
        }
    }

    /// Whether this entry is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Gitlink)
    }
}

fn unix_mode_of(kind: EntryKind) -> u32 {
    match kind {
        EntryKind::File { executable: false } => 0o100644,
        EntryKind::File { executable: true } => 0o100755,
        EntryKind::Symlink => 0o120777,
        EntryKind::Dir | EntryKind::Gitlink => 0o040755,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_match_kind() {
        let exec = FileAttr::new(2, 1, EntryKind::File { executable: true }, 10);
        assert_eq!(exec.unix_mode, 0o100755);
        let dir = FileAttr::new(3, 1, EntryKind::Dir, 0);
        assert!(dir.is_dir());
        assert_eq!(dir.unix_mode & 0o040000, 0o040000);
    }
}
