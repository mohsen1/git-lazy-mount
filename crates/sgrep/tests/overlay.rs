//! Overlay logic: slug parsing, change detection, stale-drop + local merge.

use std::process::Command;

use regex::RegexBuilder;
use sgrep::overlay::{apply, glm_changed, grep_local, locally_changed, parse_github_slug};
use sgrep::provider::Match;

fn re(p: &str) -> regex::Regex {
    RegexBuilder::new(p).build().unwrap()
}

fn m(path: &str, line: u64, text: &str) -> Match {
    Match {
        path: path.into(),
        line,
        text: text.into(),
    }
}

#[test]
fn github_slug_parsing() {
    let ok = |u| parse_github_slug(u).unwrap();
    assert_eq!(
        ok("https://github.com/microsoft/TypeScript.git"),
        "microsoft/TypeScript"
    );
    assert_eq!(ok("git@github.com:colinhacks/zod.git"), "colinhacks/zod");
    assert_eq!(ok("https://github.com/a/b/"), "a/b");
    assert_eq!(ok("ssh://git@github.com/o/r"), "o/r");
    assert_eq!(ok("https://github.com/owner/repo/tree/main"), "owner/repo"); // first two only
    assert_eq!(parse_github_slug("https://gitlab.com/x/y.git"), None);
    assert_eq!(parse_github_slug("not a url"), None);
    assert_eq!(parse_github_slug("https://github.com/onlyowner"), None);
}

#[test]
fn grep_local_refuses_path_traversal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ok.ts"), "NEEDLE\n").unwrap();
    let out = grep_local(
        dir.path(),
        &[
            "../escape.ts".into(),
            "/etc/hostname".into(),
            "ok.ts".into(),
        ],
        &re("NEEDLE"),
    );
    assert_eq!(out, vec![m("ok.ts", 1, "NEEDLE")]);
}

#[test]
fn glm_changed_reads_journal_no_faults() {
    let dir = tempfile::tempdir().unwrap();
    let mount = dir.path().join("mnt");
    let gitdir = dir.path().join("git");
    std::fs::create_dir_all(&mount).unwrap();
    std::fs::create_dir_all(gitdir.join("glm-fsmonitor")).unwrap();
    // synthetic `.git` gitfile + the NUL-separated change journal.
    std::fs::write(
        mount.join(".git"),
        format!("gitdir: {}\n", gitdir.display()),
    )
    .unwrap();
    std::fs::write(
        gitdir.join("glm-fsmonitor").join("changes.log"),
        b"src/b.ts\0src/a.ts\0src/a.ts\0".as_slice(),
    )
    .unwrap();
    // deduped + sorted, read straight from the log (no git, no file reads).
    assert_eq!(
        glm_changed(&mount),
        Some(vec!["src/a.ts".into(), "src/b.ts".into()])
    );
    // a plain directory (no gitfile/journal) → fall back path.
    assert_eq!(glm_changed(dir.path()), None);
}

#[test]
fn locally_changed_keeps_non_ascii_names() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(p)
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@e"]);
    git(&["config", "user.name", "t"]);
    // Cyrillic name (no NFC/NFD ambiguity across platforms).
    std::fs::write(p.join("файл.ts"), "x\n").unwrap();
    let changed = locally_changed(p);
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(
        !changed[0].is_ascii(),
        "non-ASCII path preserved verbatim: {:?}",
        changed[0]
    );
}

#[test]
fn overlay_drops_stale_remote_and_adds_local() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("changed.ts"),
        "old removed line\nNEEDLE here\n",
    )
    .unwrap();
    let remote = vec![
        m("unchanged.ts", 3, "NEEDLE in remote"),
        m("changed.ts", 9, "stale NEEDLE from the index"),
    ];
    let out = apply(
        remote,
        dir.path(),
        &["changed.ts".to_string()],
        &re("NEEDLE"),
    );
    // changed.ts is searched locally (line 2), its stale remote hit is dropped;
    // the unchanged file keeps its remote hit.
    assert_eq!(
        out,
        vec![
            m("unchanged.ts", 3, "NEEDLE in remote"),
            m("changed.ts", 2, "NEEDLE here"),
        ]
    );
}

#[test]
fn overlay_dedup_preserves_remote_order() {
    let dir = tempfile::tempdir().unwrap();
    let remote = vec![
        m("b.ts", 1, "NEEDLE"),
        m("a.ts", 1, "NEEDLE"),
        m("b.ts", 1, "NEEDLE"),
    ];
    let out = apply(remote, dir.path(), &[], &re("NEEDLE"));
    assert_eq!(out, vec![m("b.ts", 1, "NEEDLE"), m("a.ts", 1, "NEEDLE")]);
}

#[test]
fn overlay_removed_match_disappears() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("changed.ts"), "no longer matches\n").unwrap();
    let remote = vec![m("changed.ts", 4, "NEEDLE was here")];
    let out = apply(
        remote,
        dir.path(),
        &["changed.ts".to_string()],
        &re("NEEDLE"),
    );
    assert!(
        out.is_empty(),
        "a locally-removed match must not show: {out:?}"
    );
}

#[test]
fn grep_local_skips_missing_and_numbers_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.ts"), "x\nfoo\nbar\nfoo\n").unwrap();
    let out = grep_local(dir.path(), &["a.ts".into(), "gone.ts".into()], &re("foo"));
    assert_eq!(out, vec![m("a.ts", 2, "foo"), m("a.ts", 4, "foo")]);
}

#[test]
fn locally_changed_detects_modified_and_untracked() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(p)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@e"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(p.join("tracked.ts"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(p.join("tracked.ts"), "hello\nworld\n").unwrap();
    std::fs::write(p.join("new.ts"), "n\n").unwrap();
    let mut changed = locally_changed(p);
    changed.sort();
    assert_eq!(
        changed,
        vec!["new.ts".to_string(), "tracked.ts".to_string()]
    );
}
