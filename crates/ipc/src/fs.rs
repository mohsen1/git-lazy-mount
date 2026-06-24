//! Per-inode filesystem operations carried daemon-ward (ADR 0008).
//!
//! FSKit modules are mandatorily sandboxed and cannot run `git`, so the macOS
//! extension does not serve callbacks in-process — it forwards each `FSVolume`
//! operation to the unsandboxed per-user daemon, which owns the workspace and
//! all `git` subprocesses. These are the wire shapes for that hot path (a
//! companion to the control protocol in the crate root). Names, link targets,
//! and file data are the **exact recorded bytes** (spec §41), never assumed
//! UTF-8.
//!
//! The transport is the daemon's (a length-framed local socket); these types
//! are transport-neutral `serde` shapes so they can be exercised — and
//! round-trip-tested — independently.

use serde::{Deserialize, Serialize};

/// Entry kind, mirroring the engine's `EntryKind` in a transport-neutral form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsKind {
    /// Regular file.
    File,
    /// Regular file with the executable bit set.
    Executable,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Submodule / gitlink (surfaces as a directory).
    Gitlink,
}

/// Neutral file attributes carried over IPC (mirrors `glm_fs_common::FileAttr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsAttr {
    /// Inode number.
    pub ino: u64,
    /// Inode generation.
    pub generation: u64,
    /// Exact size in bytes (0 for directories).
    pub size: u64,
    /// Entry kind.
    pub kind: FsKind,
    /// POSIX `st_mode` (type + permission bits).
    pub mode: u32,
}

/// One directory entry returned by [`FsRequest::Enumerate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    /// Child inode.
    pub ino: u64,
    /// Exact recorded name bytes.
    pub name: Vec<u8>,
    /// The child's attributes.
    pub attr: FsAttr,
}

/// A per-inode filesystem operation (the FSKit-extension → daemon hot path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fsop", rename_all = "kebab-case")]
pub enum FsRequest {
    /// Resolve `name` within `parent`.
    Lookup {
        /// Parent directory inode.
        parent: u64,
        /// Exact recorded name bytes.
        name: Vec<u8>,
    },
    /// Fetch attributes for `ino`.
    GetAttr {
        /// Target inode.
        ino: u64,
    },
    /// Read `size` bytes from `ino` at `offset`.
    Read {
        /// Target inode.
        ino: u64,
        /// Byte offset.
        offset: u64,
        /// Maximum bytes to read.
        size: u32,
    },
    /// Read a symbolic link's target.
    Readlink {
        /// Target inode.
        ino: u64,
    },
    /// List the children of `ino`.
    Enumerate {
        /// Directory inode.
        ino: u64,
    },
    /// Create (or replace) an empty regular file.
    Create {
        /// Parent directory inode.
        parent: u64,
        /// Exact recorded name bytes.
        name: Vec<u8>,
        /// Whether the new file is executable.
        executable: bool,
    },
    /// Create a symbolic link.
    Symlink {
        /// Parent directory inode.
        parent: u64,
        /// Exact recorded name bytes.
        name: Vec<u8>,
        /// Exact recorded link target bytes.
        target: Vec<u8>,
    },
    /// Write `data` to `ino` at `offset`.
    Write {
        /// Target inode.
        ino: u64,
        /// Byte offset.
        offset: u64,
        /// Bytes to write.
        data: Vec<u8>,
    },
    /// Truncate / extend `ino` to `len` bytes.
    Truncate {
        /// Target inode.
        ino: u64,
        /// New length.
        len: u64,
    },
    /// Set or clear the executable bit on `ino`.
    SetExecutable {
        /// Target inode.
        ino: u64,
        /// Desired executable state.
        executable: bool,
    },
    /// Remove `name` from `parent`.
    Remove {
        /// Parent directory inode.
        parent: u64,
        /// Exact recorded name bytes.
        name: Vec<u8>,
    },
    /// Rename `name` under `parent` to `new_name` under `new_parent`.
    Rename {
        /// Source parent inode.
        parent: u64,
        /// Source name bytes.
        name: Vec<u8>,
        /// Destination parent inode.
        new_parent: u64,
        /// Destination name bytes.
        new_name: Vec<u8>,
    },
    /// Drop `nlookup` kernel references to `ino`.
    Forget {
        /// Target inode.
        ino: u64,
        /// Reference count to drop.
        nlookup: u64,
    },
}

/// The reply to an [`FsRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Adjacently tagged: discriminator in `ok`, payload in `value`. (Newtype
// variants here wrap non-map types — `Data(Vec<u8>)`, `Written(u32)` — which an
// internally-tagged representation can't encode.)
#[serde(tag = "ok", content = "value", rename_all = "kebab-case")]
pub enum FsResponse {
    /// Attributes (lookup / getattr / create / symlink).
    Attr(FsAttr),
    /// File or link-target bytes (read / readlink).
    Data(Vec<u8>),
    /// Directory listing (enumerate).
    Entries(Vec<FsEntry>),
    /// Bytes written (write).
    Written(u32),
    /// Success with no payload (truncate / set-executable / remove / rename / forget).
    Done,
    /// A failure: a POSIX errno the extension returns to the kernel, plus a
    /// human-readable message for logs.
    Err {
        /// POSIX errno.
        errno: i32,
        /// Diagnostic message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_request_roundtrips_with_non_utf8_name() {
        let req = FsRequest::Lookup {
            parent: 1,
            name: vec![0xff, 0x2f, 0x00, b'a'], // deliberately not UTF-8
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: FsRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn fs_response_variants_roundtrip() {
        let cases = vec![
            FsResponse::Attr(FsAttr {
                ino: 9,
                generation: 1,
                size: 42,
                kind: FsKind::Executable,
                mode: 0o755,
            }),
            FsResponse::Data(vec![1, 2, 3, 0, 255]),
            FsResponse::Entries(vec![FsEntry {
                ino: 3,
                name: b"src".to_vec(),
                attr: FsAttr {
                    ino: 3,
                    generation: 0,
                    size: 0,
                    kind: FsKind::Dir,
                    mode: 0o40755,
                },
            }]),
            FsResponse::Written(6),
            FsResponse::Done,
            FsResponse::Err {
                errno: 2,
                message: "no such file".into(),
            },
        ];
        for c in cases {
            let json = serde_json::to_string(&c).unwrap();
            let back: FsResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(c, back);
        }
    }
}
