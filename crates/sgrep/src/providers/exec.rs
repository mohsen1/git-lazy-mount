//! A runtime "plugin" provider: shell out to a user-supplied command.
//!
//! Set `SGREP_EXEC_CMD` to a command line; `sgrep` runs it with `sh -c`,
//! exporting the query as environment variables and parsing the command's stdout
//! as ripgrep-style `path:line:text` lines. This lets a new search backend be
//! plugged in with a script — no recompile.
//!
//! Exported to the command:
//! `SGREP_PATTERN`, `SGREP_REPO`, `SGREP_REV`, `SGREP_FILE`, `SGREP_COUNT`,
//! `SGREP_CASE` (`yes`/`no`), `SGREP_LITERAL` (`1`/`0`).

use std::process::Command;

use crate::provider::{Match, Query, SearchError, SearchProvider};

/// A command-backed search provider.
pub struct Exec {
    cmd: String,
}

impl Exec {
    /// Construct from an explicit command line.
    pub fn new(cmd: impl Into<String>) -> Self {
        Self { cmd: cmd.into() }
    }

    /// Build from `SGREP_EXEC_CMD`.
    pub fn from_env() -> Result<Self, SearchError> {
        let cmd = std::env::var("SGREP_EXEC_CMD")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                SearchError::Config("set SGREP_EXEC_CMD to your search command".to_string())
            })?;
        Ok(Self::new(cmd))
    }
}

impl SearchProvider for Exec {
    fn name(&self) -> &'static str {
        "exec"
    }

    fn search(&self, q: &Query) -> Result<Vec<Match>, SearchError> {
        let out = Command::new("sh")
            .arg("-c")
            .arg(&self.cmd)
            .env("SGREP_PATTERN", &q.pattern)
            .env("SGREP_REPO", &q.repo)
            .env("SGREP_REV", q.rev.as_deref().unwrap_or(""))
            .env("SGREP_FILE", q.file_filter.as_deref().unwrap_or(""))
            .env("SGREP_COUNT", q.max_results.to_string())
            .env("SGREP_CASE", if q.case_insensitive { "no" } else { "yes" })
            .env("SGREP_LITERAL", if q.literal { "1" } else { "0" })
            .output()
            .map_err(|e| SearchError::Transport(format!("exec `{}`: {e}", self.cmd)))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(SearchError::Transport(format!(
                "exec command failed ({}): {}",
                out.status,
                err.lines().next().unwrap_or("").trim()
            )));
        }
        Ok(parse_grep_lines(
            &String::from_utf8_lossy(&out.stdout),
            q.max_results,
        ))
    }
}

/// Parse ripgrep-style `path:line:text` output (caps at `max`).
pub fn parse_grep_lines(s: &str, max: usize) -> Vec<Match> {
    let mut out = Vec::new();
    for line in s.lines() {
        if line.is_empty() {
            continue;
        }
        let mut it = line.splitn(3, ':');
        match (it.next(), it.next(), it.next()) {
            (Some(path), Some(num), Some(text)) => {
                if let Ok(n) = num.parse::<u64>() {
                    out.push(Match {
                        path: path.to_string(),
                        line: n,
                        text: text.to_string(),
                    });
                }
            }
            // A bare path (e.g. `-l` style output) → line 0.
            (Some(path), None, _) => out.push(Match {
                path: path.to_string(),
                line: 0,
                text: String::new(),
            }),
            _ => {}
        }
        if out.len() >= max {
            break;
        }
    }
    out
}
