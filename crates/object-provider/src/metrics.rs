//! Provider metrics (spec §16, §48). Hydration counters back the
//! hydration-budget assertions in tests (spec §50).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Shared, thread-safe counters. Cheap to clone (an `Arc`).
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    tree_reads: AtomicU64,
    blob_reads: AtomicU64,
    filtered_reads: AtomicU64,
    bytes_read: AtomicU64,
    presence_checks: AtomicU64,
    fetch_invocations: AtomicU64,
    objects_fetched: AtomicU64,
    coalesced_waits: AtomicU64,
}

/// An immutable snapshot of the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Number of tree objects parsed.
    pub tree_reads: u64,
    /// Number of raw blob reads served.
    pub blob_reads: u64,
    /// Number of filtered (smudged) reads served.
    pub filtered_reads: u64,
    /// Total content bytes read.
    pub bytes_read: u64,
    /// Number of local presence checks performed.
    pub presence_checks: u64,
    /// Number of times the underlying fetcher was actually invoked.
    pub fetch_invocations: u64,
    /// Number of objects faulted in over the network.
    pub objects_fetched: u64,
    /// Number of callers that waited on an in-flight fetch (were coalesced).
    pub coalesced_waits: u64,
}

impl Metrics {
    pub(crate) fn inc_tree(&self) {
        self.inner.tree_reads.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn inc_blob(&self, bytes: u64) {
        self.inner.blob_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(crate) fn inc_filtered(&self, bytes: u64) {
        self.inner.filtered_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(crate) fn inc_presence_check(&self) {
        self.inner.presence_checks.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn inc_fetch_invocation(&self) {
        self.inner.fetch_invocations.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn add_objects_fetched(&self, n: u64) {
        self.inner.objects_fetched.fetch_add(n, Ordering::Relaxed);
    }
    pub(crate) fn inc_coalesced(&self) {
        self.inner.coalesced_waits.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a snapshot of all counters.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let i = &self.inner;
        MetricsSnapshot {
            tree_reads: i.tree_reads.load(Ordering::Relaxed),
            blob_reads: i.blob_reads.load(Ordering::Relaxed),
            filtered_reads: i.filtered_reads.load(Ordering::Relaxed),
            bytes_read: i.bytes_read.load(Ordering::Relaxed),
            presence_checks: i.presence_checks.load(Ordering::Relaxed),
            fetch_invocations: i.fetch_invocations.load(Ordering::Relaxed),
            objects_fetched: i.objects_fetched.load(Ordering::Relaxed),
            coalesced_waits: i.coalesced_waits.load(Ordering::Relaxed),
        }
    }
}
