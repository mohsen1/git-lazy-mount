//! `glm-fsmonitor` — incremental changed-path journal and sync barrier (§38).
//!
//! The virtual filesystem observes namespace and data mutations directly, so the
//! journal is the authoritative incremental change feed; an FSMonitor endpoint
//! (for wrapped Git commands) and OS watchers (for native redirections) are
//! advisory inputs, never the source of truth.
//!
//! A status query can request one of three barriers (spec §38): `NoWait`
//! returns immediately, `BestEffort` drains already-delivered events, and
//! `Barrier` waits until all events up to a captured sequence are incorporated.

#![forbid(unsafe_code)]

use std::sync::Mutex;

use glm_core::RepoPath;

/// The kind of change recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// Content or metadata changed.
    Modified,
    /// A new path appeared.
    Created,
    /// A path was removed.
    Removed,
}

/// Synchronization barrier modes for status queries (spec §38).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// Return immediately.
    NoWait,
    /// Incorporate already-delivered events.
    BestEffort,
    /// Wait until all events up to the captured sequence are incorporated.
    Barrier,
}

/// A single journal record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRecord {
    /// Monotonic sequence number.
    pub seq: u64,
    /// The path that changed.
    pub path: RepoPath,
    /// What changed.
    pub kind: ChangeKind,
}

/// An in-memory incremental changed-path journal.
#[derive(Default)]
pub struct ChangedPathJournal {
    inner: Mutex<Vec<ChangeRecord>>,
}

impl ChangedPathJournal {
    /// Create an empty journal.
    pub fn new() -> ChangedPathJournal {
        ChangedPathJournal::default()
    }

    /// Record a change, returning its sequence number.
    pub fn record(&self, path: RepoPath, kind: ChangeKind) -> u64 {
        let mut v = self.inner.lock().unwrap();
        let seq = v.len() as u64 + 1;
        v.push(ChangeRecord { seq, path, kind });
        seq
    }

    /// The latest sequence number (the barrier point for `Barrier` mode).
    pub fn capture_sequence(&self) -> u64 {
        self.inner.lock().unwrap().len() as u64
    }

    /// All changes with sequence strictly greater than `since`.
    pub fn changes_since(&self, since: u64) -> Vec<ChangeRecord> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.seq > since)
            .cloned()
            .collect()
    }

    /// Number of recorded changes.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Whether the journal is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }

    #[test]
    fn records_and_queries_since() {
        let j = ChangedPathJournal::new();
        j.record(p("a"), ChangeKind::Created);
        let cut = j.capture_sequence();
        j.record(p("b"), ChangeKind::Modified);
        j.record(p("a"), ChangeKind::Removed);
        let after = j.changes_since(cut);
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].path, p("b"));
        assert_eq!(after[1].kind, ChangeKind::Removed);
    }
}
