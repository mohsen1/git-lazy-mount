//! Provider integration tests against real git (spec §53 criteria 4, 5, 21).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use glm_core::{FetchPolicy, FetchPriority};
use glm_git_store::{FetchOptions, GitStore};
use glm_object_provider::{Fetcher, GitFetcher, GitObjectProvider, ObjectProvider};

/// A fetcher that counts invocations and sleeps, widening the coalescing window.
struct CountingFetcher {
    inner: GitFetcher,
    calls: Arc<AtomicUsize>,
    objects: Arc<AtomicUsize>,
    delay: Duration,
}

impl Fetcher for CountingFetcher {
    fn fetch(&self, oids: &[glm_core::ObjectId]) -> glm_core::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.objects.fetch_add(oids.len(), Ordering::SeqCst);
        std::thread::sleep(self.delay);
        self.inner.fetch(oids)
    }
}

fn lazy_store(remote: &glm_testkit::SeededRemote) -> (tempfile::TempDir, GitStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
    store.set_config("protocol.file.allow", "always").unwrap();
    store.add_remote("origin", &remote.url).unwrap();
    store
        .fetch(
            "origin",
            &[],
            &FetchOptions {
                filter: Some("blob:none".into()),
                ..Default::default()
            },
        )
        .unwrap();
    (tmp, store)
}

fn root_blob(store: &GitStore, name: &[u8]) -> glm_core::ObjectId {
    let head = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    let root = store
        .rev_parse(&format!("{}^{{tree}}", head.to_hex()))
        .unwrap()
        .unwrap();
    let tree = store.read_tree(&root, false).unwrap();
    tree.entry(name).unwrap().object_id.clone()
}

#[test]
fn cache_only_read_never_fetches() {
    let remote = glm_testkit::seed_remote(&[("a.txt", b"hello\n")]);
    let (_tmp, store) = lazy_store(&remote);
    let blob = root_blob(&store, b"a.txt");

    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CountingFetcher {
        inner: GitFetcher::new(store.clone()),
        calls: calls.clone(),
        objects: Arc::new(AtomicUsize::new(0)),
        delay: Duration::ZERO,
    };
    let provider = GitObjectProvider::new(store, Box::new(fetcher));

    // CacheOnly must error (object absent) and must NOT invoke the fetcher.
    let err = provider
        .raw_blob(&blob, FetchPolicy::CacheOnly)
        .unwrap_err();
    assert_eq!(err.code, glm_core::ErrorCode::OfflineMissingObject);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn allow_network_fetches_then_reads() {
    let remote = glm_testkit::seed_remote(&[("a.txt", b"payload\n")]);
    let (_tmp, store) = lazy_store(&remote);
    let blob = root_blob(&store, b"a.txt");
    let provider = GitObjectProvider::with_git_fetcher(store);

    let bytes = provider.raw_blob(&blob, FetchPolicy::AllowNetwork).unwrap();
    assert_eq!(bytes, b"payload\n");
    assert!(provider.is_present(&blob));
    let m = provider.metrics();
    assert_eq!(m.objects_fetched, 1);
    assert_eq!(m.blob_reads, 1);
}

#[test]
fn coalesces_100_concurrent_reads_into_one_fetch() {
    let remote = glm_testkit::seed_remote(&[("big.bin", b"0123456789abcdef\n")]);
    let (_tmp, store) = lazy_store(&remote);
    let blob = root_blob(&store, b"big.bin");

    let calls = Arc::new(AtomicUsize::new(0));
    let objects = Arc::new(AtomicUsize::new(0));
    let fetcher = CountingFetcher {
        inner: GitFetcher::new(store.clone()),
        calls: calls.clone(),
        objects: objects.clone(),
        delay: Duration::from_millis(80), // widen the coalescing window
    };
    let provider = Arc::new(GitObjectProvider::new(store, Box::new(fetcher)));

    let mut handles = Vec::new();
    for _ in 0..100 {
        let p = provider.clone();
        let oid = blob.clone();
        handles.push(std::thread::spawn(move || {
            p.raw_blob(&oid, FetchPolicy::AllowNetwork).unwrap()
        }));
    }
    for h in handles {
        assert_eq!(h.join().unwrap(), b"0123456789abcdef\n");
    }

    // Exactly one underlying fetch for 100 concurrent readers of one object.
    assert_eq!(calls.load(Ordering::SeqCst), 1, "fetch must be coalesced");
    assert_eq!(objects.load(Ordering::SeqCst), 1);
    let m = provider.metrics();
    assert_eq!(m.fetch_invocations, 1);
    assert_eq!(m.objects_fetched, 1);
    assert_eq!(m.blob_reads, 100);
}

#[test]
fn ensure_objects_batches_distinct_objects() {
    let remote = glm_testkit::seed_remote(&[("a", b"aaa"), ("b", b"bbb"), ("c", b"ccc")]);
    let (_tmp, store) = lazy_store(&remote);
    let a = root_blob(&store, b"a");
    let b = root_blob(&store, b"b");
    let c = root_blob(&store, b"c");

    let calls = Arc::new(AtomicUsize::new(0));
    let objects = Arc::new(AtomicUsize::new(0));
    let fetcher = CountingFetcher {
        inner: GitFetcher::new(store.clone()),
        calls: calls.clone(),
        objects: objects.clone(),
        delay: Duration::ZERO,
    };
    let provider = GitObjectProvider::new(store, Box::new(fetcher));

    let r = provider
        .ensure_objects(&[a, b, c], FetchPriority::Interactive)
        .unwrap();
    assert_eq!(r.fetched, 3);
    // Three distinct objects fetched in a single invocation (batched).
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(objects.load(Ordering::SeqCst), 3);
}
