//! Sourcegraph provider — queries the streaming search API
//! (`/.api/search/stream`) and parses its server-sent-events response.
//!
//! Configured from the environment, matching the `src` CLI:
//! - `SRC_ENDPOINT` — default `https://sourcegraph.com`
//! - `SRC_ACCESS_TOKEN` — optional; public repos work without it.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::provider::{Match, Query, SearchError, SearchProvider};

const DEFAULT_ENDPOINT: &str = "https://sourcegraph.com";
const USER_AGENT: &str = concat!("sgrep/", env!("CARGO_PKG_VERSION"), " (+git-lazy-mount)");
/// Hard cap on the whole streamed response, so a hostile/buggy endpoint can't
/// OOM us.
const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;
/// Cap on a single SSE line (one `data:` payload).
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// A Sourcegraph search backend.
pub struct Sourcegraph {
    endpoint: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl Sourcegraph {
    /// Build from an explicit endpoint + optional token.
    pub fn new(endpoint: impl Into<String>, token: Option<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(120))
            .build();
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    /// Build from `SRC_ENDPOINT` / `SRC_ACCESS_TOKEN`.
    pub fn from_env() -> Result<Self, SearchError> {
        let endpoint = std::env::var("SRC_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let token = std::env::var("SRC_ACCESS_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self::new(endpoint, token))
    }

    /// Render a [`Query`] as a Sourcegraph query string.
    pub fn build_query(q: &Query) -> String {
        let mut parts = Vec::new();
        parts.push("context:global".to_string());
        let mut repo = format!("repo:^github\\.com/{}$", regex::escape(&q.repo));
        if let Some(rev) = &q.rev {
            repo.push('@');
            repo.push_str(rev);
        }
        parts.push(repo);
        if let Some(f) = &q.file_filter {
            parts.push(format!("file:{f}"));
        }
        parts.push(format!(
            "case:{}",
            if q.case_insensitive { "no" } else { "yes" }
        ));
        parts.push(format!("count:{}", q.max_results));
        parts.push(format!(
            "patterntype:{}",
            if q.literal { "literal" } else { "regexp" }
        ));
        parts.push(q.pattern.clone());
        parts.join(" ")
    }
}

impl SearchProvider for Sourcegraph {
    fn name(&self) -> &'static str {
        "sourcegraph"
    }

    fn search(&self, q: &Query) -> Result<Vec<Match>, SearchError> {
        let query = Self::build_query(q);
        let url = format!("{}/.api/search/stream", self.endpoint);
        let mut req = self
            .agent
            .get(&url)
            .set("Accept", "text/event-stream")
            .set("User-Agent", USER_AGENT)
            .query("q", &query)
            .query("display", &q.max_results.to_string());
        if let Some(timeout_secs) = q.timeout_secs {
            req = req.timeout(Duration::from_secs(timeout_secs));
        }
        if let Some(t) = &self.token {
            req = req.set("Authorization", &format!("token {t}"));
        }
        let resp = req.call().map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                let hint = body.lines().next().unwrap_or("").trim();
                SearchError::Transport(format!("HTTP {code} from {url}: {hint}"))
            }
            ureq::Error::Transport(t) => {
                let msg = t.to_string();
                if q.timeout_secs.is_some() && msg.contains("timed out") {
                    SearchError::Transport(format!(
                        "search timed out after {}s; narrow the pattern or add --file",
                        q.timeout_secs.unwrap_or_default()
                    ))
                } else {
                    SearchError::Transport(msg)
                }
            }
        })?;
        parse_stream(BufReader::new(resp.into_reader()), q.max_results)
    }
}

/// Parse the SSE body of a streaming search into matches (caps at `max`).
///
/// Reads line by line with a per-line byte cap so a newline-less payload can't
/// buffer unbounded memory.
pub fn parse_stream<R: BufRead>(mut reader: R, max: usize) -> Result<Vec<Match>, SearchError> {
    let mut out = Vec::new();
    let mut event = String::new();
    let mut buf = Vec::new();
    let mut total = 0usize;
    loop {
        let n = read_capped_line(&mut reader, &mut buf, MAX_LINE_BYTES)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n);
        if total > MAX_STREAM_BYTES {
            return Err(SearchError::Protocol(
                "response exceeds size limit".to_string(),
            ));
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(ev) = line.strip_prefix("event: ") {
            event = ev.trim().to_string();
        } else if let Some(data) = line.strip_prefix("data: ") {
            match event.as_str() {
                "matches" => extract_matches(data, &mut out)?,
                "error" | "alert" => {
                    // Surface a server-side error/alert as a protocol failure.
                    return Err(SearchError::Protocol(data.trim().to_string()));
                }
                _ => {}
            }
            if out.len() >= max {
                out.truncate(max);
                break;
            }
        }
    }
    Ok(out)
}

/// Read one `\n`-terminated line into `buf`, erroring if it exceeds `cap` bytes.
/// Returns the number of bytes read (0 at EOF).
fn read_capped_line<R: BufRead>(
    r: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> Result<usize, SearchError> {
    use std::io::ErrorKind;
    buf.clear();
    loop {
        let chunk = match r.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(SearchError::Transport(e.to_string())),
        };
        if chunk.is_empty() {
            return Ok(buf.len()); // EOF
        }
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..=i]);
            let consumed = i + 1;
            r.consume(consumed);
            return Ok(buf.len());
        }
        buf.extend_from_slice(chunk);
        let consumed = chunk.len();
        r.consume(consumed);
        if buf.len() > cap {
            return Err(SearchError::Protocol(
                "response line exceeds size limit".to_string(),
            ));
        }
    }
}

/// Extract content matches from one `data:` payload (a JSON array).
pub fn extract_matches(data: &str, out: &mut Vec<Match>) -> Result<(), SearchError> {
    let arr: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| SearchError::Protocol(format!("bad matches JSON: {e}")))?;
    for m in arr.as_array().into_iter().flatten() {
        if m.get("type").and_then(|v| v.as_str()) != Some("content") {
            continue;
        }
        let Some(path) = m.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        // Prefer the newer "chunkMatches" shape, else the older "lineMatches".
        // They are mutually exclusive per the API; treating them as such avoids
        // double-counting on transitional responses that echo both.
        if let Some(cms) = m.get("chunkMatches").and_then(|v| v.as_array()) {
            for cm in cms {
                let base = cm
                    .get("contentStart")
                    .and_then(|c| c.get("line"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let content = cm.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let lines: Vec<&str> = content.split('\n').collect();
                for r in cm
                    .get("ranges")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    let abs = r
                        .get("start")
                        .and_then(|s| s.get("line"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(base);
                    let rel = abs.saturating_sub(base) as usize;
                    let text = lines.get(rel).copied().unwrap_or("");
                    out.push(Match {
                        path: path.to_string(),
                        line: abs.saturating_add(1),
                        text: trim_eol(text).to_string(),
                    });
                }
            }
        } else if let Some(lms) = m.get("lineMatches").and_then(|v| v.as_array()) {
            for lm in lms {
                let line = lm
                    .get("lineNumber")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1);
                let text = lm.get("line").and_then(|v| v.as_str()).unwrap_or("");
                out.push(Match {
                    path: path.to_string(),
                    line,
                    text: trim_eol(text).to_string(),
                });
            }
        }
    }
    Ok(())
}

fn trim_eol(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn capped_line_reads_one_line() {
        let mut buf = Vec::new();
        let mut cur = Cursor::new(b"hello\nworld\n".to_vec());
        assert_eq!(read_capped_line(&mut cur, &mut buf, 100).unwrap(), 6);
        assert_eq!(&buf, b"hello\n");
    }

    #[test]
    fn capped_line_rejects_oversized() {
        let mut buf = Vec::new();
        let res = read_capped_line(&mut Cursor::new(vec![b'a'; 100]), &mut buf, 10);
        assert!(res.is_err(), "a line over the cap must error");
    }

    #[test]
    fn capped_line_eof_returns_zero() {
        let mut buf = Vec::new();
        assert_eq!(
            read_capped_line(&mut Cursor::new(Vec::new()), &mut buf, 10).unwrap(),
            0
        );
    }
}
