//! `glm-object-provider` — the missing-object provider (spec §16).
//!
//! Responsibilities:
//! * **Residency authority.** Tracks which objects are locally present so the
//!   fragile `cat-file --batch` session (which fatally exits on a missing
//!   *promisor* object) is only ever queried for present objects.
//! * **Request coalescing.** Concurrent requests for the same missing object
//!   trigger exactly one underlying fetch; the rest wait (spec §2.2, §53.5).
//! * **Fetch batching.** Distinct missing objects in one `ensure_objects` call
//!   are fetched in a single invocation.
//! * **Policy enforcement.** A [`FetchPolicy`] of `CacheOnly`/`MustNotFetch`
//!   never touches the network — this is what filesystem callbacks use so a
//!   read cannot trigger a credential prompt (spec §3.13).
//! * **Metrics.** Hydration counters back the budget assertions in tests.
//!
//! Locks are never held across network I/O or subprocess execution (spec §3.19).

#![forbid(unsafe_code)]

mod metrics;

use std::collections::HashSet;
use std::sync::{Condvar, Mutex};

use glm_core::{Error, ErrorCode, FetchPolicy, FetchPriority, RepoPath, Result, TreeObject};
use glm_core::{ObjectId, TreeEntry};
use glm_git_store::{BatchSession, GitStore};

pub use metrics::{Metrics, MetricsSnapshot};

/// Abstracts the network fetch so tests can inject counting/slow fetchers.
/// Only the provider's scheduler is allowed to call this (spec §16).
pub trait Fetcher: Send + Sync {
    /// Fault the given objects into the local store. Should be idempotent.
    fn fetch(&self, oids: &[ObjectId]) -> Result<()>;
}

/// Production fetcher: lazily faults objects via `GitStore`.
pub struct GitFetcher {
    store: GitStore,
}

impl GitFetcher {
    /// Wrap a store.
    pub fn new(store: GitStore) -> Self {
        GitFetcher { store }
    }
}

impl Fetcher for GitFetcher {
    fn fetch(&self, oids: &[ObjectId]) -> Result<()> {
        self.store.fetch_objects(oids)
    }
}

/// Outcome of [`ObjectProvider::ensure_objects`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EnsureResult {
    /// Objects this call faulted in.
    pub fetched: usize,
    /// Objects already present.
    pub already_present: usize,
    /// Objects whose fetch was coalesced into another in-flight request.
    pub coalesced: usize,
}

/// The provider interface (spec §16). Synchronous; concurrency is achieved by
/// calling from multiple threads, with internal coalescing.
pub trait ObjectProvider: Send + Sync {
    /// Resolve and parse a tree object.
    fn tree(&self, id: &ObjectId, policy: FetchPolicy) -> Result<TreeObject>;
    /// Read raw (unfiltered) blob bytes.
    fn raw_blob(&self, id: &ObjectId, policy: FetchPolicy) -> Result<Vec<u8>>;
    /// Read working-tree (smudge-filtered) bytes for a blob at `path`.
    /// `attr_source` is a tree-ish from which `.gitattributes` are resolved
    /// (the workspace base commit); see [`glm_git_store::GitStore::smudge_blob`].
    fn filtered_blob(
        &self,
        id: &ObjectId,
        path: &RepoPath,
        attr_source: Option<&ObjectId>,
        policy: FetchPolicy,
    ) -> Result<Vec<u8>>;
    /// Ensure objects are present locally, coalescing/batching as needed.
    fn ensure_objects(&self, ids: &[ObjectId], priority: FetchPriority) -> Result<EnsureResult>;
    /// Whether the provider knows the object is locally present (cached view).
    fn is_present(&self, id: &ObjectId) -> bool;
    /// Current metrics snapshot.
    fn metrics(&self) -> MetricsSnapshot;
}

#[derive(Default)]
struct State {
    present: HashSet<ObjectId>,
    in_flight: HashSet<ObjectId>,
}

/// The default [`ObjectProvider`] backed by a [`GitStore`] and a [`Fetcher`].
pub struct GitObjectProvider {
    store: GitStore,
    fetcher: Box<dyn Fetcher>,
    session: Mutex<Option<BatchSession>>,
    state: Mutex<State>,
    cond: Condvar,
    metrics: Metrics,
}

impl GitObjectProvider {
    /// Build a provider from a store and a fetcher.
    pub fn new(store: GitStore, fetcher: Box<dyn Fetcher>) -> Self {
        GitObjectProvider {
            store,
            fetcher,
            session: Mutex::new(None),
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
            metrics: Metrics::default(),
        }
    }

    /// Convenience constructor using the production [`GitFetcher`].
    pub fn with_git_fetcher(store: GitStore) -> Self {
        let fetcher = Box::new(GitFetcher::new(store.clone()));
        GitObjectProvider::new(store, fetcher)
    }

    fn mark_present(&self, id: &ObjectId) {
        self.state.lock().unwrap().present.insert(id.clone());
    }

    fn cached_present(&self, id: &ObjectId) -> bool {
        self.state.lock().unwrap().present.contains(id)
    }

    /// Ensure `id` is present locally, fetching if the policy allows. Returns an
    /// appropriate offline/missing error otherwise.
    fn ensure_present_locally(&self, id: &ObjectId, policy: FetchPolicy) -> Result<()> {
        if self.cached_present(id) {
            return Ok(());
        }
        self.metrics.inc_presence_check();
        if self.store.object_exists(id, false)? {
            self.mark_present(id);
            return Ok(());
        }
        if !policy.may_fetch() {
            return Err(offline(id));
        }
        self.ensure_objects(std::slice::from_ref(id), priority_of(policy))?;
        if self.cached_present(id) || self.store.object_exists(id, false)? {
            self.mark_present(id);
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::RemoteMissingObject,
                format!("object {} not found after fetch", id.to_hex()),
            ))
        }
    }

    /// Read a known-present blob via the batch session, falling back to a
    /// one-shot read if the session is unavailable.
    fn read_present_blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let mut guard = self.session.lock().unwrap();
        if guard.as_ref().map(|s| !s.is_alive()).unwrap_or(true) {
            *guard = Some(self.store.batch_session()?);
        }
        let session = guard.as_mut().expect("session present");
        match session.contents(id) {
            Ok(Some((_, bytes))) => Ok(bytes),
            Ok(None) | Err(_) => {
                // Session disagrees or died; respawn lazily and use a one-shot.
                *guard = None;
                drop(guard);
                self.store.read_blob_raw(id, false)
            }
        }
    }
}

impl ObjectProvider for GitObjectProvider {
    fn tree(&self, id: &ObjectId, policy: FetchPolicy) -> Result<TreeObject> {
        // One-shot reads are always safe (a process death is just an error),
        // so we try directly and only fetch on a genuine miss.
        match self.store.read_tree(id, false) {
            Ok(t) => {
                self.mark_present(id);
                self.metrics.inc_tree();
                Ok(t)
            }
            Err(e) if is_missing(&e) => {
                if !policy.may_fetch() {
                    return Err(offline(id));
                }
                self.ensure_objects(std::slice::from_ref(id), priority_of(policy))?;
                let t = self.store.read_tree(id, false)?;
                self.mark_present(id);
                self.metrics.inc_tree();
                Ok(t)
            }
            Err(e) => Err(e),
        }
    }

    fn raw_blob(&self, id: &ObjectId, policy: FetchPolicy) -> Result<Vec<u8>> {
        self.ensure_present_locally(id, policy)?;
        let bytes = self.read_present_blob(id)?;
        self.metrics.inc_blob(bytes.len() as u64);
        Ok(bytes)
    }

    fn filtered_blob(
        &self,
        id: &ObjectId,
        path: &RepoPath,
        attr_source: Option<&ObjectId>,
        policy: FetchPolicy,
    ) -> Result<Vec<u8>> {
        self.ensure_present_locally(id, policy)?;
        let attr_hex = attr_source.map(|o| o.to_hex());
        // Faithful filtering may need attribute blobs (`.gitattributes`) along
        // the path, which can be absent under a blob:none clone. When the policy
        // permits network, let Git fault them in; under cache-only the smudge
        // fails with an offline error (the caller must prefetch attributes).
        let bytes =
            self.store
                .smudge_blob(id, path.as_bytes(), attr_hex.as_deref(), policy.may_fetch())?;
        self.metrics.inc_filtered(bytes.len() as u64);
        Ok(bytes)
    }

    fn ensure_objects(&self, ids: &[ObjectId], _priority: FetchPriority) -> Result<EnsureResult> {
        let mut to_fetch: Vec<ObjectId> = Vec::new();
        let mut wait_for: Vec<ObjectId> = Vec::new();
        let mut already = 0usize;
        {
            let mut st = self.state.lock().unwrap();
            for oid in ids {
                if st.present.contains(oid) {
                    already += 1;
                } else if st.in_flight.contains(oid) {
                    wait_for.push(oid.clone());
                } else {
                    st.in_flight.insert(oid.clone());
                    to_fetch.push(oid.clone());
                }
            }
        }

        let mut fetched = 0usize;
        let mut fetch_err: Option<Error> = None;
        if !to_fetch.is_empty() {
            // Network I/O happens with NO lock held (spec §3.19).
            self.metrics.inc_fetch_invocation();
            match self.fetcher.fetch(&to_fetch) {
                Ok(()) => {
                    fetched = to_fetch.len();
                    self.metrics.add_objects_fetched(fetched as u64);
                }
                Err(e) => fetch_err = Some(e),
            }
            let mut st = self.state.lock().unwrap();
            for oid in &to_fetch {
                st.in_flight.remove(oid);
                if fetch_err.is_none() {
                    st.present.insert(oid.clone());
                }
            }
            self.cond.notify_all();
        }

        if !wait_for.is_empty() {
            let mut st = self.state.lock().unwrap();
            for _ in &wait_for {
                self.metrics.inc_coalesced();
            }
            loop {
                let pending = wait_for.iter().any(|o| st.in_flight.contains(o));
                if !pending {
                    break;
                }
                st = self.cond.wait(st).unwrap();
            }
        }

        if let Some(e) = fetch_err {
            return Err(e);
        }
        Ok(EnsureResult {
            fetched,
            already_present: already,
            coalesced: wait_for.len(),
        })
    }

    fn is_present(&self, id: &ObjectId) -> bool {
        self.cached_present(id)
    }

    fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }
}

fn priority_of(policy: FetchPolicy) -> FetchPriority {
    match policy {
        FetchPolicy::Prefetch => FetchPriority::Prefetch,
        _ => FetchPriority::Interactive,
    }
}

fn is_missing(e: &Error) -> bool {
    matches!(
        e.code,
        ErrorCode::OfflineMissingObject | ErrorCode::RemoteMissingObject
    )
}

fn offline(id: &ObjectId) -> Error {
    Error::new(
        ErrorCode::OfflineMissingObject,
        format!(
            "object {} not present locally and fetch not permitted",
            id.to_hex()
        ),
    )
    .with_action("prefetch while online or rerun without --offline / with network access")
}

/// Convenience: collect the blob ids directly referenced by a tree (one level).
pub fn child_blob_ids(tree: &TreeObject) -> Vec<ObjectId> {
    tree.entries
        .iter()
        .filter(|e| e.mode.is_file())
        .map(|e: &TreeEntry| e.object_id.clone())
        .collect()
}
