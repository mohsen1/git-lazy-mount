//! NSFileCoordination cooperation model (issue #9, spec §41).
//!
//! macOS coordinates file access through `NSFileCoordinator`: a *coordinated
//! writer* gets exclusive access to a path while *coordinated readers* wait, so
//! Finder and document-based apps never observe a half-written file. The FSKit
//! `FSVolume` adapter receives the coordination intent for each operation; this
//! module is the backend-independent serialization model it uses to honor that
//! intent — no coordinated read ever overlaps an in-flight write to the same
//! path, and coordinated writes to a path are mutually exclusive.
//!
//! The engine already writes file content atomically (the overlay persists via a
//! temp-file rename), so a single read never sees torn *bytes*; this adds the
//! cross-operation ordering `NSFileCoordinator` expects (e.g. a document package
//! whose parts must move together). The on-device adapter wraps each `FskitOps`
//! callback in [`Coordinator::coordinate`] with the intent it was handed; wiring
//! to the real `NSFileCoordinator` and Finder/document-app validation is
//! on-device (tracked by issue #12).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use glm_core::RepoPath;

/// The access intent a coordinated operation declares (mirrors the
/// `NSFileCoordinator` reading/writing intents the adapter receives).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// A coordinated read (shared).
    Read,
    /// A coordinated content write (exclusive).
    Write,
    /// A coordinated delete (exclusive).
    Delete,
    /// A coordinated move/rename (exclusive).
    Move,
}

impl Intent {
    /// Whether this intent mutates and therefore needs exclusive access.
    pub fn is_write(&self) -> bool {
        matches!(self, Intent::Write | Intent::Delete | Intent::Move)
    }
}

/// Per-path reader/writer coordination so coordinated readers never observe an
/// in-flight write and coordinated writers are mutually exclusive (issue #9).
#[derive(Default)]
pub struct Coordinator {
    locks: Mutex<HashMap<RepoPath, Arc<RwLock<()>>>>,
}

impl Coordinator {
    /// A fresh coordinator.
    pub fn new() -> Coordinator {
        Coordinator::default()
    }

    fn lock_for(&self, path: &RepoPath) -> Arc<RwLock<()>> {
        let mut map = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(path.clone())
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    /// Run `f` under a coordinated **read** (shared) lock for `path`.
    pub fn coordinate_read<R>(&self, path: &RepoPath, f: impl FnOnce() -> R) -> R {
        let lock = self.lock_for(path);
        let _guard = lock.read().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// Run `f` under a coordinated **write** (exclusive) lock for `path`.
    pub fn coordinate_write<R>(&self, path: &RepoPath, f: impl FnOnce() -> R) -> R {
        let lock = self.lock_for(path);
        let _guard = lock.write().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// Dispatch by declared [`Intent`]: writes/deletes/moves are exclusive,
    /// reads are shared.
    pub fn coordinate<R>(&self, path: &RepoPath, intent: Intent, f: impl FnOnce() -> R) -> R {
        if intent.is_write() {
            self.coordinate_write(path, f)
        } else {
            self.coordinate_read(path, f)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::thread;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn intents_classify_correctly() {
        assert!(Intent::Write.is_write());
        assert!(Intent::Delete.is_write());
        assert!(Intent::Move.is_write());
        assert!(!Intent::Read.is_write());
    }

    #[test]
    fn coordinated_writers_are_mutually_exclusive() {
        let coord = Arc::new(Coordinator::new());
        let path = p("doc");
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let coord = coord.clone();
                let path = path.clone();
                let active = active.clone();
                let max_seen = max_seen.clone();
                thread::spawn(move || {
                    coord.coordinate_write(&path, || {
                        let n = active.fetch_add(1, SeqCst) + 1;
                        max_seen.fetch_max(n, SeqCst);
                        thread::yield_now();
                        active.fetch_sub(1, SeqCst);
                    });
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_seen.load(SeqCst),
            1,
            "coordinated writers on the same path must never run concurrently"
        );
    }

    #[test]
    fn coordinated_read_never_overlaps_an_inflight_write() {
        let coord = Arc::new(Coordinator::new());
        let path = p("doc");
        let writing = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicBool::new(false));

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let coord = coord.clone();
                let path = path.clone();
                let writing = writing.clone();
                let torn = torn.clone();
                thread::spawn(move || {
                    if i % 2 == 0 {
                        coord.coordinate(&path, Intent::Write, || {
                            writing.store(true, SeqCst);
                            thread::yield_now();
                            writing.store(false, SeqCst);
                        });
                    } else {
                        coord.coordinate(&path, Intent::Read, || {
                            if writing.load(SeqCst) {
                                torn.store(true, SeqCst);
                            }
                        });
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(
            !torn.load(SeqCst),
            "a coordinated read observed an in-flight write (torn read)"
        );
    }

    #[test]
    fn distinct_paths_do_not_serialize() {
        // Writers to *different* paths can proceed concurrently — coordination is
        // per-path, not a global lock.
        let coord = Coordinator::new();
        let a = p("a");
        let b = p("b");
        // Acquire a write on `a`, then a write on `b` nested inside it: if the
        // coordinator serialized across paths this would deadlock; it does not.
        coord.coordinate_write(&a, || {
            coord.coordinate_write(&b, || {});
        });
    }
}
