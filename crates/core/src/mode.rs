//! Git tree-entry modes.

use serde::{Deserialize, Serialize};

/// The kinds of entry Git records in a tree.
///
/// Git tracks exactly these; it does *not* track owner/group/most permission
/// bits/mtime/xattrs/ACLs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GitMode {
    /// `100644` — a normal file.
    Regular,
    /// `100755` — an executable file.
    Executable,
    /// `120000` — a symbolic link (blob bytes are the link target).
    Symlink,
    /// `040000` — a subtree (directory).
    Tree,
    /// `160000` — a gitlink (submodule commit pointer).
    Gitlink,
}

impl GitMode {
    /// Parse the octal mode string exactly as it appears in a Git tree object.
    pub fn parse_octal(mode: &str) -> Option<GitMode> {
        match mode.trim_start_matches('0') {
            "100644" | "644" => Some(GitMode::Regular),
            "100755" | "755" => Some(GitMode::Executable),
            "120000" => Some(GitMode::Symlink),
            "40000" | "040000" => Some(GitMode::Tree),
            "160000" => Some(GitMode::Gitlink),
            _ => None,
        }
    }

    /// The canonical octal string Git writes for this mode in a **tree object**.
    ///
    /// Note: trees are serialized as `40000` (no leading zero). Emitting
    /// `040000` produces a "zero-padded file mode" that `git fsck` rejects, so
    /// this exact form is required when hashing tree objects.
    pub fn as_octal(&self) -> &'static str {
        match self {
            GitMode::Regular => "100644",
            GitMode::Executable => "100755",
            GitMode::Symlink => "120000",
            GitMode::Tree => "40000",
            GitMode::Gitlink => "160000",
        }
    }

    /// Whether this entry is a directory-like node (`Tree`).
    pub fn is_tree(&self) -> bool {
        matches!(self, GitMode::Tree)
    }

    /// Whether this entry's content is a regular file blob (regular or exec).
    pub fn is_file(&self) -> bool {
        matches!(self, GitMode::Regular | GitMode::Executable)
    }

    /// POSIX `st_mode` bits suitable for a filesystem `getattr` of this entry.
    /// (Directory/symlink/regular type bits plus a conventional permission set.)
    pub fn to_unix_mode(&self) -> u32 {
        match self {
            GitMode::Regular => 0o100644,
            GitMode::Executable => 0o100755,
            GitMode::Symlink => 0o120777,
            GitMode::Tree => 0o040755,
            // A gitlink projects as a directory mount point.
            GitMode::Gitlink => 0o040755,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_render_roundtrip() {
        for m in [
            GitMode::Regular,
            GitMode::Executable,
            GitMode::Symlink,
            GitMode::Tree,
            GitMode::Gitlink,
        ] {
            assert_eq!(GitMode::parse_octal(m.as_octal()), Some(m));
        }
    }

    #[test]
    fn tree_short_form() {
        // Git emits trees as `40000` (no leading zero) in some plumbing output.
        assert_eq!(GitMode::parse_octal("40000"), Some(GitMode::Tree));
    }

    #[test]
    fn unknown_mode_rejected() {
        assert_eq!(GitMode::parse_octal("100600"), None);
    }
}
