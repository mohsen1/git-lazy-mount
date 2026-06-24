//! Stable inode/file identity for the lifetime of a workspace.
//!
//! Guarantees enforced here:
//! * repeated lookup of the same logical path returns the same identity;
//! * an open handle survives a namespace rename (identity moves with content);
//! * an open handle survives `unlink` until the final `forget`/close;
//! * inode **numbers are never reused**, so a stale kernel reference can never
//!   be mistaken for a newly created file (a per-inode generation is also
//!   carried for backends that surface it).

use std::collections::HashMap;
use std::sync::Mutex;

use glm_core::RepoPath;

/// The root inode number (FUSE convention).
pub const ROOT_INO: u64 = 1;

/// A live inode entry.
#[derive(Debug, Clone)]
struct InodeEntry {
    /// The current path, or `None` if unlinked (handle still open).
    path: Option<RepoPath>,
    /// Kernel lookup reference count (FUSE `lookup`/`forget`).
    lookups: u64,
    /// Generation assigned at allocation.
    generation: u64,
}

/// Maps logical paths to stable inode numbers and back.
pub struct InodeTable {
    inner: Mutex<Inner>,
}

struct Inner {
    next_ino: u64,
    generation: u64,
    path_to_ino: HashMap<RepoPath, u64>,
    entries: HashMap<u64, InodeEntry>,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    /// Create a table with the root pre-allocated at [`ROOT_INO`].
    pub fn new() -> InodeTable {
        let mut entries = HashMap::new();
        entries.insert(
            ROOT_INO,
            InodeEntry {
                path: Some(RepoPath::root()),
                lookups: 1,
                generation: 1,
            },
        );
        let mut path_to_ino = HashMap::new();
        path_to_ino.insert(RepoPath::root(), ROOT_INO);
        InodeTable {
            inner: Mutex::new(Inner {
                next_ino: ROOT_INO + 1,
                generation: 1,
                path_to_ino,
                entries,
            }),
        }
    }

    /// Look up (allocating if needed) the inode for `path`, incrementing its
    /// kernel reference count. Returns `(ino, generation)`.
    pub fn lookup(&self, path: &RepoPath) -> (u64, u64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(&ino) = inner.path_to_ino.get(path) {
            let gen = {
                let e = inner.entries.get_mut(&ino).expect("entry exists");
                e.lookups += 1;
                e.generation
            };
            return (ino, gen);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        let generation = inner.generation;
        inner.entries.insert(
            ino,
            InodeEntry {
                path: Some(path.clone()),
                lookups: 1,
                generation,
            },
        );
        inner.path_to_ino.insert(path.clone(), ino);
        (ino, generation)
    }

    /// The current path for an inode, if it is still linked.
    pub fn path_of(&self, ino: u64) -> Option<RepoPath> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&ino)
            .and_then(|e| e.path.clone())
    }

    /// Drop `n` kernel references (FUSE `forget`). Memory for an unlinked inode
    /// is released once its references reach zero; its number is never reused.
    pub fn forget(&self, ino: u64, n: u64) {
        if ino == ROOT_INO {
            return;
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = inner.entries.get_mut(&ino) {
            e.lookups = e.lookups.saturating_sub(n);
            if e.lookups == 0 && e.path.is_none() {
                inner.entries.remove(&ino);
            }
        }
    }

    /// Rename: the same inode now answers to `new` (identity preserved; spec
    ///). Open handles remain valid.
    pub fn rename(&self, old: &RepoPath, new: &RepoPath) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ino) = inner.path_to_ino.remove(old) {
            // Evict any prior occupant of the destination name.
            if let Some(prev) = inner.path_to_ino.insert(new.clone(), ino) {
                if let Some(e) = inner.entries.get_mut(&prev) {
                    e.path = None;
                }
            }
            if let Some(e) = inner.entries.get_mut(&ino) {
                e.path = Some(new.clone());
            }
        }
    }

    /// Unlink: the name disappears from the namespace, but the inode stays alive
    /// (its number reserved) until the final `forget` (open-unlink semantics).
    pub fn unlink(&self, path: &RepoPath) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ino) = inner.path_to_ino.remove(path) {
            if let Some(e) = inner.entries.get_mut(&ino) {
                e.path = None;
                if e.lookups == 0 {
                    inner.entries.remove(&ino);
                }
            }
        }
    }

    /// Advance the generation counter (e.g. on a view switch). Subsequently
    /// allocated inodes carry the new generation; existing ones keep theirs so
    /// open handles are unaffected.
    pub fn bump_generation(&self) -> u64 {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.generation += 1;
        inner.generation
    }

    /// Whether an inode number is currently allocated.
    pub fn is_live(&self, ino: u64) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .contains_key(&ino)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn repeated_lookup_is_stable() {
        let t = InodeTable::new();
        let (a, _) = t.lookup(&p("src/lib.rs"));
        let (b, _) = t.lookup(&p("src/lib.rs"));
        assert_eq!(a, b);
        assert_ne!(a, ROOT_INO);
    }

    #[test]
    fn rename_preserves_identity() {
        let t = InodeTable::new();
        let (ino, _) = t.lookup(&p("old.txt"));
        t.rename(&p("old.txt"), &p("new.txt"));
        assert_eq!(t.path_of(ino), Some(p("new.txt")));
        // Looking up the new name yields the same inode.
        let (ino2, _) = t.lookup(&p("new.txt"));
        assert_eq!(ino, ino2);
    }

    #[test]
    fn open_unlink_keeps_inode_until_forget() {
        let t = InodeTable::new();
        let (ino, _) = t.lookup(&p("doomed")); // lookups = 1 (an open handle)
        t.unlink(&p("doomed"));
        // The namespace entry is gone, but the inode survives for the open handle.
        assert!(t.is_live(ino));
        assert_eq!(t.path_of(ino), None);
        // Final forget releases it.
        t.forget(ino, 1);
        assert!(!t.is_live(ino));
    }

    #[test]
    fn inode_numbers_are_not_reused() {
        let t = InodeTable::new();
        let (a, _) = t.lookup(&p("a"));
        t.unlink(&p("a"));
        t.forget(a, 1);
        let (b, _) = t.lookup(&p("b"));
        assert_ne!(a, b, "a freed inode number must not be reused");
    }

    #[test]
    fn generation_bumps_for_new_allocations_only() {
        let t = InodeTable::new();
        let (ino_old, gen_old) = t.lookup(&p("keep"));
        let new_gen = t.bump_generation();
        // Existing inode keeps its generation (open handles unaffected).
        let (ino_again, gen_again) = t.lookup(&p("keep"));
        assert_eq!(ino_old, ino_again);
        assert_eq!(gen_old, gen_again);
        // A newly allocated inode carries the new generation.
        let (_, gen_new) = t.lookup(&p("fresh"));
        assert_eq!(gen_new, new_gen);
        assert!(gen_new > gen_old);
    }
}
