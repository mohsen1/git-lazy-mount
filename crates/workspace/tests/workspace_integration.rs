//! Workspace integration tests (spec §53 criteria 6–10, 13–18).

use std::sync::Arc;

use glm_core::{FetchPolicy, GitMode, ObjectId, RepoPath};
use glm_git_store::{FetchOptions, GitStore, Identity};
use glm_object_provider::{GitObjectProvider, ObjectProvider};
use glm_workspace::{EntryKind, StatusCode, Workspace, WorkspaceConfig};

const POLICY: FetchPolicy = FetchPolicy::AllowNetwork;

fn p(s: &str) -> RepoPath {
    RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
}

fn ident() -> Identity {
    Identity {
        name: "Test".into(),
        email: "test@example.com".into(),
        date: Some("@1700000000 +0000".into()),
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    // Keep the remote alive so lazy fetches during the test succeed.
    _remote: glm_testkit::SeededRemote,
    store: GitStore,
    ws: Workspace,
}

fn harness(files: &[(&str, &[u8])]) -> (Harness, ObjectId) {
    let remote = glm_testkit::seed_remote(files);
    harness_from(remote)
}

fn harness_from(remote: glm_testkit::SeededRemote) -> (Harness, ObjectId) {
    let tmp = tempfile::tempdir().unwrap();
    let store = GitStore::init_bare(tmp.path().join("git"), None).unwrap();
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
    let base = store
        .resolve_ref("refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    // Create the attached local branch at base.
    store
        .update_ref_cas("refs/heads/main", &base, None)
        .unwrap();

    let provider: Arc<dyn ObjectProvider> =
        Arc::new(GitObjectProvider::with_git_fetcher(store.clone()));
    let cfg = WorkspaceConfig {
        workspace_head_ref: "refs/lazy-mount/workspaces/test/head".into(),
        attached_branch: Some("refs/heads/main".into()),
        remote: Some("origin".into()),
        identity: Some(Identity {
            name: "Test".into(),
            email: "test@example.com".into(),
            date: Some("@1700000000 +0000".into()),
        }),
    };
    let ws =
        Workspace::open_or_create(store.clone(), provider, tmp.path(), cfg, Some(base.clone()))
            .unwrap();
    (
        Harness {
            _tmp: tmp,
            _remote: remote,
            store,
            ws,
        },
        base,
    )
}

#[test]
fn status_reports_overlay_changes_without_fetching_blobs() {
    let (h, _base) = harness(&[("a.txt", b"alpha\n"), ("src/lib.rs", b"x\n")]);
    let before = h.ws.provider().metrics();

    // Write a new file and modify an existing one.
    h.ws.write_full(&p("new.txt"), b"hello", false).unwrap();

    let status = h.ws.status(POLICY).unwrap();
    let after = h.ws.provider().metrics();

    let new = status.iter().find(|e| e.path == p("new.txt")).unwrap();
    assert_eq!(new.worktree, StatusCode::Added);
    assert_eq!(new.index, StatusCode::Unmodified);

    // status must not fetch blobs and must not write objects.
    assert_eq!(after.objects_fetched, before.objects_fetched);
    assert_eq!(after.blob_reads, before.blob_reads);
}

#[test]
fn stage_commit_preserves_unstaged_changes() {
    let (h, base) = harness(&[("a.txt", b"original\n")]);

    // Stage a change to a.txt.
    h.ws.write_full(&p("a.txt"), b"modified\n", false).unwrap();
    h.ws.stage_path(&p("a.txt"), POLICY).unwrap();

    // Make a further unstaged change to a different file.
    h.ws.write_full(&p("b.txt"), b"unstaged\n", false).unwrap();

    let out = h.ws.commit("change a", POLICY).unwrap();
    assert_ne!(out.commit, base);
    assert!(out.branch_advanced);

    // The commit tree contains the modified a.txt.
    let tree = h
        .store
        .rev_parse(&format!("{}^{{tree}}", out.commit.to_hex()))
        .unwrap()
        .unwrap();
    let t = h.ws.provider().tree(&tree, POLICY).unwrap();
    let a = t.entry(b"a.txt").unwrap();
    assert_eq!(
        h.store.read_blob_raw(&a.object_id, true).unwrap(),
        b"modified\n"
    );

    // The unstaged b.txt change is preserved in the working tree (criterion 16).
    let status = h.ws.status(POLICY).unwrap();
    let b = status.iter().find(|e| e.path == p("b.txt")).unwrap();
    assert_eq!(b.worktree, StatusCode::Added);
    // a.txt is now clean (committed and dematerialized).
    assert!(status.iter().all(|e| e.path != p("a.txt")));
}

#[test]
fn rename_clean_file_does_not_fetch_blob() {
    let (h, _base) = harness(&[("keep.txt", b"unchanged content\n")]);
    let before = h.ws.provider().metrics();

    h.ws.rename(&p("keep.txt"), &p("moved.txt"), POLICY)
        .unwrap();

    let after = h.ws.provider().metrics();
    // The blob was never fetched or read (criterion 10, spec §53.10).
    assert_eq!(after.objects_fetched, before.objects_fetched);
    assert_eq!(after.blob_reads, before.blob_reads);

    let status = h.ws.status(POLICY).unwrap();
    assert_eq!(
        status
            .iter()
            .find(|e| e.path == p("keep.txt"))
            .unwrap()
            .worktree,
        StatusCode::Deleted
    );
    assert_eq!(
        status
            .iter()
            .find(|e| e.path == p("moved.txt"))
            .unwrap()
            .worktree,
        StatusCode::Added
    );
}

#[test]
fn truncate_to_zero_does_not_fetch_old_content() {
    let (h, _base) = harness(&[("big.txt", b"lots of bytes here\n")]);
    let before = h.ws.provider().metrics();

    h.ws.truncate(&p("big.txt"), 0, POLICY).unwrap();

    let after = h.ws.provider().metrics();
    assert_eq!(
        after.objects_fetched, before.objects_fetched,
        "must not fetch old content"
    );
    assert_eq!(h.ws.read_file(&p("big.txt"), POLICY).unwrap(), b"");
}

#[test]
fn partial_overwrite_preserves_untouched_bytes() {
    let (h, _base) = harness(&[("data.txt", b"AAAAAAAAAA")]); // 10 bytes
                                                              // Overwrite 3 bytes at offset 2.
    h.ws.write_at(&p("data.txt"), 2, b"BBB", POLICY).unwrap();
    assert_eq!(
        h.ws.read_file(&p("data.txt"), POLICY).unwrap(),
        b"AABBBAAAAA"
    );
}

#[test]
fn commit_detects_concurrent_branch_movement() {
    let (h, base) = harness(&[("a.txt", b"v1\n")]);

    // Someone else advances refs/heads/main behind our back.
    let base_tree = h
        .store
        .rev_parse(&format!("{}^{{tree}}", base.to_hex()))
        .unwrap()
        .unwrap();
    let side = h
        .store
        .commit_tree(&glm_git_store::CommitParams {
            tree: base_tree,
            parents: vec![base.clone()],
            message: "concurrent".into(),
            // Supply identity explicitly: CI runners have no global git user
            // (relying on ambient config fails with "Author identity unknown").
            author: Some(ident()),
            committer: Some(ident()),
            sign: false,
        })
        .unwrap();
    h.store
        .update_ref_cas("refs/heads/main", &side, Some(&base))
        .unwrap();

    // Our commit: the private head advances, but the attached branch CAS fails.
    h.ws.write_full(&p("a.txt"), b"v2\n", false).unwrap();
    h.ws.stage_path(&p("a.txt"), POLICY).unwrap();
    let out = h.ws.commit("our change", POLICY).unwrap();

    assert!(!out.branch_advanced);
    assert!(out.divergence.is_some());
    assert_eq!(
        out.divergence.as_ref().unwrap().code,
        glm_core::ErrorCode::ConcurrentBranchMovement
    );
    // The workspace commit is still reachable via the private head ref.
    assert_eq!(
        h.store
            .resolve_ref("refs/lazy-mount/workspaces/test/head")
            .unwrap()
            .unwrap(),
        out.commit
    );
    // The public branch was NOT silently overwritten (spec §14).
    assert_eq!(
        h.store.resolve_ref("refs/heads/main").unwrap().unwrap(),
        side
    );
}

#[test]
fn commit_reuses_subtrees_and_passes_fsck() {
    // The base tree contains a subdirectory; committing a root-level change must
    // reuse the subtree entry and emit a valid tree (mode 40000, not 040000).
    let (h, _base) = harness(&[("README.md", b"v1\n"), ("src/lib.rs", b"fn main() {}\n")]);
    h.ws.write_full(&p("README.md"), b"v2\n", false).unwrap();
    h.ws.stage_path(&p("README.md"), POLICY).unwrap();
    let out = h.ws.commit("touch readme", POLICY).unwrap();

    let tree = h
        .store
        .rev_parse(&format!("{}^{{tree}}", out.commit.to_hex()))
        .unwrap()
        .unwrap();
    let t = h.ws.provider().tree(&tree, POLICY).unwrap();
    assert_eq!(t.entry(b"src").unwrap().mode, GitMode::Tree);

    // `git fsck` must accept the new commit and its tree.
    let fsck_ok = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(h.store.git_dir())
        .args(["fsck", "--connectivity-only", &out.commit.to_hex()])
        .output()
        .unwrap()
        .status
        .success();
    assert!(fsck_ok);
}

#[test]
fn crlf_filter_applied_faithfully() {
    // .gitattributes forces eol=crlf for *.txt; the repo stores LF.
    let (h, _base) = harness(&[
        (".gitattributes", b"*.txt text eol=crlf\n"),
        ("doc.txt", b"line1\nline2\n"),
    ]);
    // The faithful working-tree read yields CRLF (matches a real checkout),
    // resolved via --attr-source from the workspace base (criterion 20, §25).
    let content = h.ws.read_file(&p("doc.txt"), POLICY).unwrap();
    assert_eq!(content, b"line1\r\nline2\r\n");

    // The raw blob is still LF — filtering is not baked into the object.
    let entry =
        h.ws.resolve_base_entry(&p("doc.txt"), POLICY)
            .unwrap()
            .unwrap();
    let raw = h.ws.provider().raw_blob(&entry.object_id, POLICY).unwrap();
    assert_eq!(raw, b"line1\nline2\n");
}

#[test]
fn symlink_write_commit_and_read() {
    let (h, _base) = harness(&[("target.txt", b"data\n")]);

    h.ws.write_symlink(&p("link"), b"target.txt").unwrap();
    assert_eq!(
        h.ws.lookup(&p("link"), POLICY).unwrap(),
        Some(EntryKind::Symlink)
    );

    h.ws.stage_path(&p("link"), POLICY).unwrap();
    let out = h.ws.commit("add symlink", POLICY).unwrap();

    // The commit records a symlink (mode 120000).
    let tree = h
        .store
        .rev_parse(&format!("{}^{{tree}}", out.commit.to_hex()))
        .unwrap()
        .unwrap();
    let t = h.ws.provider().tree(&tree, POLICY).unwrap();
    assert_eq!(t.entry(b"link").unwrap().mode, GitMode::Symlink);

    // Reading the committed symlink yields the raw target bytes (no filtering).
    assert_eq!(h.ws.read_file(&p("link"), POLICY).unwrap(), b"target.txt");
}

#[test]
fn executable_bit_change_without_fetch() {
    let (h, _base) = harness(&[("run.sh", b"#!/bin/sh\necho hi\n")]);
    let before = h.ws.provider().metrics();
    h.ws.set_executable(&p("run.sh"), true, POLICY).unwrap();
    let after = h.ws.provider().metrics();
    assert_eq!(after.objects_fetched, before.objects_fetched);

    h.ws.stage_path(&p("run.sh"), POLICY).unwrap();
    let out = h.ws.commit("make executable", POLICY).unwrap();
    let tree = h
        .store
        .rev_parse(&format!("{}^{{tree}}", out.commit.to_hex()))
        .unwrap()
        .unwrap();
    let t = h.ws.provider().tree(&tree, POLICY).unwrap();
    assert_eq!(
        t.entry(b"run.sh").unwrap().mode,
        glm_core::GitMode::Executable
    );
}

#[test]
fn switch_clean_and_refuse_when_dirty() {
    use glm_workspace::ResetMode;
    let (h, base) = harness(&[("a.txt", b"v1\n")]);

    // Advance to a new commit.
    h.ws.write_full(&p("a.txt"), b"v2\n", false).unwrap();
    h.ws.stage_path(&p("a.txt"), POLICY).unwrap();
    let out = h.ws.commit("v2", POLICY).unwrap();
    assert_eq!(h.ws.base_commit(), Some(out.commit.clone()));

    // Clean switch back to the original base.
    assert!(h.ws.is_clean());
    h.ws.switch(base.clone()).unwrap();
    assert_eq!(h.ws.base_commit(), Some(base.clone()));
    // The working tree reflects the original content again.
    assert_eq!(h.ws.read_file(&p("a.txt"), POLICY).unwrap(), b"v1\n");

    // A dirty workspace refuses to switch.
    h.ws.write_full(&p("a.txt"), b"dirty\n", false).unwrap();
    let err = h.ws.switch(out.commit).unwrap_err();
    assert_eq!(err.code, glm_core::ErrorCode::DirtyWorkspaceConflict);

    // reset --hard is honestly reported as unimplemented (not a silent no-op).
    let err = h.ws.reset(ResetMode::Hard, base).unwrap_err();
    assert_eq!(err.code, glm_core::ErrorCode::UnsupportedOperation);
}

#[test]
fn reset_mixed_clears_stage_keeps_worktree() {
    use glm_workspace::ResetMode;
    let (h, base) = harness(&[("a.txt", b"v1\n")]);
    h.ws.write_full(&p("a.txt"), b"v2\n", false).unwrap();
    h.ws.stage_path(&p("a.txt"), POLICY).unwrap();

    // Mixed reset to the same base: stage cleared, working tree preserved.
    h.ws.reset(ResetMode::Mixed, base).unwrap();
    let status = h.ws.status(POLICY).unwrap();
    let a = status.iter().find(|e| e.path == p("a.txt")).unwrap();
    assert_eq!(a.index, StatusCode::Unmodified); // unstaged
    assert_eq!(a.worktree, StatusCode::Modified); // working change kept
}

#[test]
fn branch_lists_local_branches() {
    let (h, _base) = harness(&[("a", b"b")]);
    let branches = h.ws.list_branches().unwrap();
    assert!(branches.iter().any(|(name, _)| name == "refs/heads/main"));
}
