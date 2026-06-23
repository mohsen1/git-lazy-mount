//! `glm-projection` — the filesystem-projection abstraction (Milestone 1).
//!
//! A [`Projection`] receives desired-state changes and reflects them into the
//! operating system's view (FUSE invalidations, ProjFS placeholder updates,
//! etc.). The desired/applied generation split (spec §2.5) lets a crash between
//! "metadata committed" and "projection applied" be *detected* (a stale
//! workspace) rather than silently corrupting state.
//!
//! [`InMemoryProjection`] is the backend-independent test projection used by the
//! core (Milestone 1) and by model-based tests; real kernel backends live in
//! `glm-fs-fuse` / `glm-fs-fskit` / `glm-fs-projfs`.

#![forbid(unsafe_code)]

use std::sync::Mutex;

use glm_core::RepoPath;

/// A desired-state change the projection must reflect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionChange {
    /// A path was created.
    Created(RepoPath),
    /// A path's content/metadata changed.
    Modified(RepoPath),
    /// A path was removed.
    Removed(RepoPath),
    /// A path was renamed.
    Renamed {
        /// Old path.
        from: RepoPath,
        /// New path.
        to: RepoPath,
    },
}

/// Reflects workspace desired-state changes into an OS-visible filesystem.
pub trait Projection: Send + Sync {
    /// Apply a single change to the projected view.
    fn apply(&self, change: ProjectionChange);
    /// Invalidate cached attributes/contents for a path (e.g. after a fetch).
    fn invalidate(&self, path: &RepoPath);
    /// Record that the projection has caught up to `generation` (spec §2.5).
    fn set_applied_generation(&self, generation: u64);
    /// The last generation fully applied by this projection.
    fn applied_generation(&self) -> u64;
}

/// A test/headless projection that records what it was asked to do.
#[derive(Default)]
pub struct InMemoryProjection {
    changes: Mutex<Vec<ProjectionChange>>,
    invalidations: Mutex<Vec<RepoPath>>,
    applied: Mutex<u64>,
}

impl InMemoryProjection {
    /// Create an empty in-memory projection.
    pub fn new() -> InMemoryProjection {
        InMemoryProjection::default()
    }

    /// All changes applied so far.
    pub fn changes(&self) -> Vec<ProjectionChange> {
        self.changes.lock().unwrap().clone()
    }

    /// All paths invalidated so far.
    pub fn invalidations(&self) -> Vec<RepoPath> {
        self.invalidations.lock().unwrap().clone()
    }
}

impl Projection for InMemoryProjection {
    fn apply(&self, change: ProjectionChange) {
        self.changes.lock().unwrap().push(change);
    }
    fn invalidate(&self, path: &RepoPath) {
        self.invalidations.lock().unwrap().push(path.clone());
    }
    fn set_applied_generation(&self, generation: u64) {
        *self.applied.lock().unwrap() = generation;
    }
    fn applied_generation(&self) -> u64 {
        *self.applied.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn records_changes_and_generation() {
        let proj = InMemoryProjection::new();
        proj.apply(ProjectionChange::Created(p("a")));
        proj.apply(ProjectionChange::Renamed {
            from: p("a"),
            to: p("b"),
        });
        proj.invalidate(&p("b"));
        proj.set_applied_generation(5);
        assert_eq!(proj.changes().len(), 2);
        assert_eq!(proj.invalidations(), vec![p("b")]);
        assert_eq!(proj.applied_generation(), 5);
    }
}
