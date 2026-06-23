//! Parse and build raw Git tree objects, byte-exactly (spec §17: arbitrary
//! non-UTF-8 names).
//!
//! A raw tree object (the bytes `git cat-file <tree>` prints, header already
//! stripped) is a concatenation of entries:
//!
//! ```text
//! <octal-mode-ascii> SP <name-bytes> NUL <raw-oid-bytes>
//! ```
//!
//! Building the canonical byte stream requires Git's tree sort order, in which
//! a subtree entry sorts as if its name had a trailing `/`.

use glm_core::{Error, ErrorCode, GitMode, ObjectFormat, ObjectId, Result, TreeEntry, TreeObject};

/// Parse raw tree-object bytes into a [`TreeObject`].
pub fn parse_tree(id: ObjectId, bytes: &[u8], format: &ObjectFormat) -> Result<TreeObject> {
    let oid_len = format.raw_len().ok_or_else(|| {
        Error::new(
            ErrorCode::UnsupportedOperation,
            format!("cannot parse trees for object format '{}'", format.name()),
        )
    })?;
    let mut entries = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // mode: ascii digits up to SP
        let sp = bytes[i..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| corrupt("missing space after mode"))?;
        let mode_str =
            std::str::from_utf8(&bytes[i..i + sp]).map_err(|_| corrupt("non-ascii mode"))?;
        let mode = GitMode::parse_octal(mode_str)
            .ok_or_else(|| corrupt(&format!("unknown mode {mode_str}")))?;
        i += sp + 1;

        // name: bytes up to NUL
        let nul = bytes[i..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| corrupt("missing NUL after name"))?;
        let name = bytes[i..i + nul].to_vec();
        i += nul + 1;

        // oid: raw bytes of oid_len
        if i + oid_len > bytes.len() {
            return Err(corrupt("truncated oid"));
        }
        let oid_bytes = bytes[i..i + oid_len].to_vec();
        i += oid_len;

        entries.push(TreeEntry {
            name,
            mode,
            object_id: ObjectId {
                format: format.clone(),
                bytes: oid_bytes,
            },
        });
    }
    Ok(TreeObject { id, entries })
}

/// Build the canonical raw byte stream for a tree, sorting entries in Git order.
///
/// The caller hashes the result (`git hash-object -w -t tree`) to obtain the
/// tree object id; producing the exact byte stream Git would produce guarantees
/// the resulting id matches a native `git write-tree`.
pub fn build_tree_bytes(mut entries: Vec<TreeEntry>) -> Vec<u8> {
    entries.sort_by(|a, b| git_name_cmp(&a.name, a.mode, &b.name, b.mode));
    let mut out = Vec::new();
    for e in &entries {
        out.extend_from_slice(e.mode.as_octal().as_bytes());
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.object_id.bytes);
    }
    out
}

/// Git's tree-entry name comparison: compare bytes, but a tree entry behaves as
/// if its name ended in `/`.
fn git_name_cmp(a: &[u8], am: GitMode, b: &[u8], bm: GitMode) -> std::cmp::Ordering {
    let a_slash = matches!(am, GitMode::Tree);
    let b_slash = matches!(bm, GitMode::Tree);
    let n = a.len().min(b.len());
    match a[..n].cmp(&b[..n]) {
        std::cmp::Ordering::Equal => {
            let ac = a
                .get(n)
                .copied()
                .or(if a_slash { Some(b'/') } else { None });
            let bc = b
                .get(n)
                .copied()
                .or(if b_slash { Some(b'/') } else { None });
            ac.cmp(&bc)
        }
        other => other,
    }
}

fn corrupt(msg: &str) -> Error {
    Error::new(
        ErrorCode::LocalObjectCorruption,
        format!("malformed tree: {msg}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![byte; 20],
        }
    }

    #[test]
    fn build_then_parse_roundtrips() {
        let entries = vec![
            TreeEntry {
                name: b"b.txt".to_vec(),
                mode: GitMode::Regular,
                object_id: oid(2),
            },
            TreeEntry {
                name: b"a.txt".to_vec(),
                mode: GitMode::Executable,
                object_id: oid(1),
            },
            TreeEntry {
                name: b"sub".to_vec(),
                mode: GitMode::Tree,
                object_id: oid(3),
            },
        ];
        let bytes = build_tree_bytes(entries.clone());
        let parsed = parse_tree(oid(99), &bytes, &ObjectFormat::Sha1).unwrap();
        // Sorted: a.txt, b.txt, sub
        assert_eq!(parsed.entries[0].name, b"a.txt");
        assert_eq!(parsed.entries[1].name, b"b.txt");
        assert_eq!(parsed.entries[2].name, b"sub");
        assert_eq!(parsed.entries[2].mode, GitMode::Tree);
    }

    #[test]
    fn non_utf8_name_preserved() {
        let entries = vec![TreeEntry {
            name: vec![0xff, 0xfe, b'x'],
            mode: GitMode::Regular,
            object_id: oid(7),
        }];
        let bytes = build_tree_bytes(entries);
        let parsed = parse_tree(oid(0), &bytes, &ObjectFormat::Sha1).unwrap();
        assert_eq!(parsed.entries[0].name, vec![0xff, 0xfe, b'x']);
    }

    #[test]
    fn tree_sorts_with_trailing_slash() {
        // "foo" (file) vs "foo" (tree): file < tree because '\0' < '/'... actually
        // git compares "foo" vs "foo/", so the file (shorter) sorts first.
        let entries = vec![
            TreeEntry {
                name: b"foo".to_vec(),
                mode: GitMode::Tree,
                object_id: oid(2),
            },
            TreeEntry {
                name: b"foo-bar".to_vec(),
                mode: GitMode::Regular,
                object_id: oid(1),
            },
        ];
        let bytes = build_tree_bytes(entries);
        let parsed = parse_tree(oid(0), &bytes, &ObjectFormat::Sha1).unwrap();
        // "foo" + "/" = "foo/" ; compare with "foo-bar": '/' (0x2f) vs '-' (0x2d)
        // => "foo-bar" sorts before "foo/".
        assert_eq!(parsed.entries[0].name, b"foo-bar");
        assert_eq!(parsed.entries[1].name, b"foo");
    }

    #[test]
    fn rejects_truncated() {
        let bad = b"100644 a\0\x01\x02"; // oid too short
        assert!(parse_tree(oid(0), bad, &ObjectFormat::Sha1).is_err());
    }
}
