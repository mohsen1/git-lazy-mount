//! Integration tests against a real `git`, proving the core laziness and
//! Git-interop claims.

use glm_core::{GitMode, ObjectFormat, ObjectId, TreeEntry};
use glm_git_store::{CommitParams, FetchOptions, GitStore, Identity};

fn test_identity() -> Identity {
    Identity {
        name: "Test".into(),
        email: "test@example.com".into(),
        date: Some("@1700000000 +0000".into()),
    }
}

/// Build a store that has lazily fetched trees but not blobs (blob:none).
fn lazy_store(remote: &glm_testkit::SeededRemote) -> (tempfile::TempDir, GitStore) {
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
    // Allow file:// transport for the local fixture remote.
    store.set_config("protocol.file.allow", "always").unwrap();
    store.set_config("core.autocrlf", "false").unwrap();
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

#[test]
fn partial_clone_fetches_trees_but_not_blobs() {
    let remote = glm_testkit::seed_remote(&[
        ("a.txt", b"hello world\n"),
        ("src/lib.rs", b"fn main() {}\n"),
    ]);
    let (_tmp, store) = lazy_store(&remote);

    // HEAD resolves without any blob present.
    let head = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .expect("origin/main resolves");
    assert_eq!(head.to_hex(), remote.head_hex);

    // Root tree is present (blob:none keeps trees) — reading it must not fetch.
    let root_tree = store
        .rev_parse(&format!("{}^{{tree}}", head.to_hex()))
        .unwrap()
        .unwrap();
    let tree = store.read_tree(&root_tree, false).unwrap();
    let a = tree.entry(b"a.txt").expect("a.txt in tree");
    assert_eq!(a.mode, GitMode::Regular);

    // The blob is NOT present locally (this is the laziness claim).
    assert!(
        !store.object_exists(&a.object_id, false).unwrap(),
        "blob must be absent after blob:none clone"
    );
    // Reading it cache-only fails with a missing/offline error.
    assert!(store.read_blob_raw(&a.object_id, false).is_err());

    // Fetch just that one object; now it is present and reads correctly.
    store
        .fetch_objects(std::slice::from_ref(&a.object_id))
        .unwrap();
    assert!(store.object_exists(&a.object_id, false).unwrap());
    assert_eq!(
        store.read_blob_raw(&a.object_id, false).unwrap(),
        b"hello world\n"
    );

    // The sibling blob remains absent — we fetched exactly one.
    let sub = tree.entry(b"src").unwrap();
    let sub_tree = store.read_tree(&sub.object_id, false).unwrap();
    let lib = sub_tree.entry(b"lib.rs").unwrap();
    assert!(
        !store.object_exists(&lib.object_id, false).unwrap(),
        "unrelated blob must stay absent"
    );
}

/// Resolve `HEAD`'s root tree oid for a freshly fetched store.
fn root_tree(store: &GitStore) -> ObjectId {
    let head = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .expect("origin/main resolves");
    store
        .rev_parse(&format!("{}^{{tree}}", head.to_hex()))
        .unwrap()
        .unwrap()
}

#[test]
fn read_tree_memoizes_and_serves_repeat_reads_from_cache() {
    // A tree is immutable per oid, so read_tree parses it once and serves later
    // reads of the same oid from the cache — this is what collapses the O(depth)
    // re-reads of a directory walk (the root tree gets re-read thousands of times).
    let remote = glm_testkit::seed_remote(&[
        ("a.txt", b"A\n"),
        ("src/lib.rs", b"fn x() {}\n"),
        ("src/deep/mod.rs", b"// deep\n"),
    ]);
    let (_tmp, store) = lazy_store(&remote);
    let root = root_tree(&store);

    assert_eq!(store.cached_tree_count(), 0, "cache starts cold");
    let first = store.read_tree(&root, false).unwrap();
    assert_eq!(store.cached_tree_count(), 1, "root tree memoized");

    // A repeat read of the same oid is a cache hit: identical, no new entry.
    let again = store.read_tree(&root, false).unwrap();
    assert_eq!(
        first.entries, again.entries,
        "cached read matches the first"
    );
    assert_eq!(
        store.cached_tree_count(),
        1,
        "repeat read does not re-parse"
    );

    // A distinct subtree adds exactly one entry; re-reading it adds none.
    let src = first.entry(b"src").unwrap().object_id.clone();
    store.read_tree(&src, false).unwrap();
    assert_eq!(store.cached_tree_count(), 2);
    store.read_tree(&src, false).unwrap();
    assert_eq!(store.cached_tree_count(), 2, "subtree re-read is cached");
}

#[test]
fn cached_read_matches_a_cold_read() {
    // The batched/cached path must produce exactly the same parse as a brand-new
    // store reading the same tree cold (its own empty cache) — no divergence
    // between the `cat-file --batch` body and a one-shot `cat-file tree`.
    let remote = glm_testkit::seed_remote(&[("a.txt", b"A\n"), ("b/c.txt", b"C\n")]);
    let (_tmp, warm) = lazy_store(&remote);
    let root = root_tree(&warm);

    let via_cache = warm.read_tree(&root, false).unwrap(); // batch + memoize
    let cold = GitStore::open(warm.git_dir()).unwrap(); // fresh, empty cache
    let fresh = cold.read_tree(&root, false).unwrap();
    assert_eq!(via_cache.entries, fresh.entries);
}

#[test]
fn read_tree_on_a_non_tree_errors_and_caches_nothing() {
    // A blob oid is not a tree: the session sees `kind=blob` and we fall back to
    // the one-shot read, which rejects it. The failed read must not poison the
    // cache.
    let remote = glm_testkit::seed_remote(&[("a.txt", b"hello\n")]);
    let (_tmp, store) = lazy_store(&remote);
    let root = root_tree(&store);
    let blob = store
        .read_tree(&root, false)
        .unwrap()
        .entry(b"a.txt")
        .unwrap()
        .object_id
        .clone();
    store.fetch_objects(std::slice::from_ref(&blob)).unwrap(); // make it locally present

    let before = store.cached_tree_count();
    assert!(
        store.read_tree(&blob, false).is_err(),
        "a blob oid is not a tree"
    );
    assert_eq!(
        store.cached_tree_count(),
        before,
        "a failed read caches nothing"
    );
}

#[test]
fn write_tree_commit_and_cas_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();

    let blob = store.hash_blob_raw(b"hello\n", true).unwrap();
    let tree = store
        .write_tree(vec![TreeEntry {
            name: b"f.txt".to_vec(),
            mode: GitMode::Regular,
            object_id: blob.clone(),
        }])
        .unwrap();

    // The written tree is a valid Git tree that reads back identically.
    let parsed = store.read_tree(&tree, false).unwrap();
    assert_eq!(parsed.entry(b"f.txt").unwrap().object_id, blob);

    let commit = store
        .commit_tree(&CommitParams {
            tree: tree.clone(),
            parents: vec![],
            message: "initial".into(),
            author: Some(test_identity()),
            committer: Some(test_identity()),
            sign: false,
        })
        .unwrap();

    // Create the ref via CAS (expected old = null), then verify it resolves.
    store
        .update_ref_cas("refs/heads/work", &commit, None)
        .unwrap();
    assert_eq!(
        store.resolve_ref("refs/heads/work").unwrap().unwrap(),
        commit
    );
}

#[test]
fn cas_detects_concurrent_branch_movement() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
    let blob = store.hash_blob_raw(b"x", true).unwrap();
    let tree = store
        .write_tree(vec![TreeEntry {
            name: b"x".to_vec(),
            mode: GitMode::Regular,
            object_id: blob,
        }])
        .unwrap();
    let c1 = store
        .commit_tree(&CommitParams {
            tree: tree.clone(),
            parents: vec![],
            message: "c1".into(),
            author: Some(test_identity()),
            committer: Some(test_identity()),
            sign: false,
        })
        .unwrap();
    store.update_ref_cas("refs/heads/b", &c1, None).unwrap();

    // A CAS with the wrong expected-old must be rejected as concurrent movement.
    let wrong = ObjectId::null(ObjectFormat::Sha1);
    let err = store
        .update_ref_cas("refs/heads/b", &c1, Some(&wrong))
        .unwrap_err();
    assert_eq!(err.code, glm_core::ErrorCode::ConcurrentBranchMovement);

    // With the correct expected-old it succeeds.
    let c2 = store
        .commit_tree(&CommitParams {
            tree,
            parents: vec![c1.clone()],
            message: "c2".into(),
            author: Some(test_identity()),
            committer: Some(test_identity()),
            sign: false,
        })
        .unwrap();
    store
        .update_ref_cas("refs/heads/b", &c2, Some(&c1))
        .unwrap();
}

/// Convert string args to the `OsString` slice `interop_run` expects.
fn osargs(args: &[&str]) -> Vec<std::ffi::OsString> {
    args.iter().map(std::ffi::OsString::from).collect()
}

#[test]
fn interop_bridge_status_commit_and_lazy_fetch() {
    let remote = glm_testkit::seed_remote(&[
        ("README.md", b"hello world\n"),
        ("src/lib.rs", b"fn main() {}\n"),
    ]);
    let (tmp, store) = lazy_store(&remote);
    let base = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    let base_tree = store
        .rev_parse(&format!("{}^{{tree}}", base.to_hex()))
        .unwrap()
        .unwrap();
    let scratch = tmp.path().join("interop");

    // A "staged" tree = the base tree with a new top-level file added.
    let note = store.hash_blob_raw(b"a note\n", true).unwrap();
    let mut entries = store.read_tree(&base_tree, false).unwrap().entries;
    entries.push(TreeEntry {
        name: b"notes.txt".to_vec(),
        mode: GitMode::Regular,
        object_id: note,
    });
    let staged_tree = store.write_tree(entries).unwrap();

    // `git diff --cached --quiet` exits 1 when the synthesized index differs
    // from HEAD (staged change present), and 0 when it matches the base.
    let dirty = store
        .interop_run(
            &scratch,
            &base,
            Some("main"),
            Some(&staged_tree),
            &osargs(&["diff", "--cached", "--quiet"]),
        )
        .unwrap();
    assert_eq!(dirty.status.code(), Some(1), "staged delta should be seen");
    assert_eq!(dirty.head.as_ref(), Some(&base), "read-only leaves HEAD");

    let clean = store
        .interop_run(
            &scratch,
            &base,
            Some("main"),
            Some(&base_tree),
            &osargs(&["diff", "--cached", "--quiet"]),
        )
        .unwrap();
    assert_eq!(clean.status.code(), Some(0), "index==HEAD is clean");

    // Lazy fetch THROUGH the bridge: a blob absent from the store is faulted in
    // by stock `git cat-file`, landing in the shared store.
    let readme = store
        .read_tree(&base_tree, false)
        .unwrap()
        .entry(b"README.md")
        .unwrap()
        .object_id
        .clone();
    assert!(
        !store.object_exists(&readme, false).unwrap(),
        "blob absent before bridge access"
    );
    let shown = store
        .interop_run(
            &scratch,
            &base,
            Some("main"),
            Some(&staged_tree),
            &osargs(&["cat-file", "blob", &readme.to_hex()]),
        )
        .unwrap();
    assert!(shown.status.success());
    assert!(
        store.object_exists(&readme, false).unwrap(),
        "bridge faulted the blob into the shared store"
    );

    // Native `git commit` of the synthesized index lands in the shared store as
    // an ordinary commit whose parent is the base and whose tree is the staged
    // tree. Identity is supplied via `-c` so no ambient Git identity is needed.
    let committed = store
        .interop_run(
            &scratch,
            &base,
            Some("main"),
            Some(&staged_tree),
            &osargs(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "via interop bridge",
            ]),
        )
        .unwrap();
    assert!(committed.status.success(), "commit should succeed");
    let new = committed.head.expect("HEAD advanced to the new commit");
    assert_ne!(new, base, "commit produced a new commit");
    assert!(
        store.object_exists(&new, false).unwrap(),
        "commit object is in the shared store"
    );
    let parent = store
        .rev_parse(&format!("{}^1", new.to_hex()))
        .unwrap()
        .unwrap();
    assert_eq!(parent, base, "commit's parent is the workspace base");
    let new_tree = store
        .rev_parse(&format!("{}^{{tree}}", new.to_hex()))
        .unwrap()
        .unwrap();
    assert_eq!(new_tree, staged_tree, "commit's tree is the staged tree");
}

#[test]
fn batch_session_serves_local_and_reports_missing() {
    let remote = glm_testkit::seed_remote(&[("a.txt", b"content\n")]);
    let (_tmp, store) = lazy_store(&remote);
    let head = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    let root_tree = store
        .rev_parse(&format!("{}^{{tree}}", head.to_hex()))
        .unwrap()
        .unwrap();
    let a = store.read_tree(&root_tree, false).unwrap();
    let blob = a.entry(b"a.txt").unwrap().object_id.clone();

    let mut session = store.batch_session().unwrap();
    // Tree is present (reading present objects is safe).
    assert!(session.info(&root_tree).unwrap().is_some());

    // An object Git has never heard of is reported cleanly as missing.
    let unknown = ObjectId::parse_hex(
        ObjectFormat::Sha1,
        "0000000000000000000000000000000000000001",
    )
    .unwrap();
    assert!(session.info(&unknown).unwrap().is_none());

    // NOTE: we deliberately do NOT query the promisor-missing blob before
    // fetching it — that would fatally terminate the session. The provider
    // fetches first.
    store.fetch_objects(std::slice::from_ref(&blob)).unwrap();
    let (info, bytes) = session.contents(&blob).unwrap().expect("present now");
    assert_eq!(info.kind, "blob");
    assert_eq!(bytes, b"content\n");
    assert!(session.is_alive());
}
