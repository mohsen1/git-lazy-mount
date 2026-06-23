//! Integration tests against a real `git`, proving the core laziness and
//! Git-interop claims (spec §53 MVP criteria 1, 4, 10, 15, 17, 18).

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
    // fetching it — that would fatally terminate the session (see
    // docs/feasibility/git-object-fetching.md). The provider fetches first.
    store.fetch_objects(std::slice::from_ref(&blob)).unwrap();
    let (info, bytes) = session.contents(&blob).unwrap().expect("present now");
    assert_eq!(info.kind, "blob");
    assert_eq!(bytes, b"content\n");
    assert!(session.is_alive());
}
