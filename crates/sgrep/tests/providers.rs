//! Provider tests: Sourcegraph SSE parsing + query building, and the exec plugin.

use std::io::Cursor;

use sgrep::provider::{Query, SearchProvider};
use sgrep::providers::exec::{parse_grep_lines, Exec};
use sgrep::providers::sourcegraph::{extract_matches, parse_stream, Sourcegraph};

fn q(pattern: &str) -> Query {
    Query {
        repo: "microsoft/TypeScript".into(),
        rev: None,
        pattern: pattern.into(),
        file_filter: Some(r"\.ts$".into()),
        case_insensitive: false,
        literal: false,
        max_results: 50,
        timeout_secs: None,
    }
}

#[test]
fn line_matches_1based_and_eol_trimmed() {
    let sse = concat!(
        "event: filters\ndata: []\n\n",
        "event: matches\ndata: [{\"type\":\"content\",\"path\":\"src/a.ts\",",
        "\"lineMatches\":[{\"line\":\"export function createProgram() {\",\"lineNumber\":41}]}]\n\n",
        "event: matches\ndata: [{\"type\":\"content\",\"path\":\"src/b.ts\",",
        "\"lineMatches\":[{\"line\":\"  createProgram();\\r\",\"lineNumber\":9}]}]\n\n",
        "event: done\ndata: {}\n",
    );
    let m = parse_stream(Cursor::new(sse), 1000).unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!((m[0].path.as_str(), m[0].line), ("src/a.ts", 42));
    assert_eq!(m[0].text, "export function createProgram() {");
    assert_eq!((m[1].path.as_str(), m[1].line), ("src/b.ts", 10));
    assert_eq!(m[1].text, "  createProgram();"); // trailing \r removed
}

#[test]
fn chunk_matches_via_ranges() {
    let data = r#"[{"type":"content","path":"x.ts","chunkMatches":[{"content":"line9\nline10 MATCH\nline11","contentStart":{"line":8},"ranges":[{"start":{"line":9}}]}]}]"#;
    let mut out = Vec::new();
    extract_matches(data, &mut out).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(
        (out[0].path.as_str(), out[0].line, out[0].text.as_str()),
        ("x.ts", 10, "line10 MATCH")
    );
}

#[test]
fn non_content_and_pathonly_ignored() {
    let data = r#"[{"type":"repo","repository":"r"},{"type":"content","path":"p"}]"#;
    let mut out = Vec::new();
    extract_matches(data, &mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn both_shapes_do_not_double_count() {
    // A transitional payload carrying both shapes for one entry must count once.
    let data = r#"[{"type":"content","path":"p.ts","chunkMatches":[{"content":"MATCH","contentStart":{"line":0},"ranges":[{"start":{"line":0}}]}],"lineMatches":[{"line":"MATCH","lineNumber":0}]}]"#;
    let mut out = Vec::new();
    extract_matches(data, &mut out).unwrap();
    assert_eq!(
        out.len(),
        1,
        "must prefer chunkMatches, not double-count: {out:?}"
    );
}

#[test]
fn server_error_event_surfaced() {
    let sse = "event: error\ndata: {\"msg\":\"boom\"}\n";
    let err = parse_stream(Cursor::new(sse), 10).unwrap_err();
    assert!(format!("{err}").contains("boom"), "{err}");
}

#[test]
fn count_cap_truncates() {
    let sse = "event: matches\ndata: [{\"type\":\"content\",\"path\":\"a\",\"lineMatches\":[{\"line\":\"x\",\"lineNumber\":0},{\"line\":\"y\",\"lineNumber\":1},{\"line\":\"z\",\"lineNumber\":2}]}]\n";
    assert_eq!(parse_stream(Cursor::new(sse), 2).unwrap().len(), 2);
}

#[test]
fn query_building() {
    let s = Sourcegraph::build_query(&q("createProgram"));
    for needle in [
        r"repo:^github\.com/microsoft/TypeScript$",
        r"file:\.ts$",
        "case:yes",
        "count:50",
        "patterntype:regexp",
        "createProgram",
        "context:global",
    ] {
        assert!(s.contains(needle), "query {s:?} missing {needle:?}");
    }
}

#[test]
fn exec_plugin_runs_and_parses() {
    let p = Exec::new("printf 'found.ts:7:%s here\\n' \"$SGREP_PATTERN\"");
    let m = p
        .search(&Query {
            pattern: "ZZZ".into(),
            ..q("ZZZ")
        })
        .unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].path.as_str(), m[0].line), ("found.ts", 7));
    assert!(m[0].text.contains("ZZZ"), "{:?}", m[0].text);
}

#[test]
fn exec_grep_line_parsing() {
    let out = parse_grep_lines("a.ts:12:hello\nb.ts:3:foo:bar\n\nc.ts\n", 100);
    assert_eq!(out.len(), 3);
    assert_eq!((out[0].line, out[0].text.as_str()), (12, "hello"));
    assert_eq!(out[1].text, "foo:bar"); // colons in the match text are preserved
    assert_eq!((out[2].path.as_str(), out[2].line), ("c.ts", 0)); // bare path
}

#[test]
#[ignore = "hits the live Sourcegraph public API"]
fn live_sourcegraph() {
    let p = Sourcegraph::from_env().unwrap();
    let m = p.search(&q("export function createProgram")).unwrap();
    assert!(!m.is_empty());
    assert!(m.iter().any(|x| x.path.contains("program.ts")), "{m:?}");
}
