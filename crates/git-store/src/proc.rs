//! Subprocess execution helpers and Git stderr classification.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use glm_core::{Error, ErrorCode, Result};

/// Outcome of a finished Git subprocess.
pub(crate) struct Run {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status_ok: bool,
}

/// Run a command to completion, optionally feeding `stdin`, capturing both
/// streams. Never inherits a terminal (so credential prompts cannot appear).
pub(crate) fn run(mut cmd: Command, stdin: Option<&[u8]>) -> Result<Run> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        Error::new(ErrorCode::Internal, format!("failed to spawn git: {e}")).with_source(e)
    })?;

    if let Some(data) = stdin {
        let mut sink = child
            .stdin
            .take()
            .ok_or_else(|| Error::internal("git stdin unavailable"))?;
        // Write in a scope so the pipe closes (EOF) before we wait.
        sink.write_all(data)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("git stdin write: {e}")))?;
        drop(sink);
    }

    let Output {
        status,
        stdout,
        stderr,
    } = child
        .wait_with_output()
        .map_err(|e| Error::new(ErrorCode::Internal, format!("git wait: {e}")).with_source(e))?;

    Ok(Run {
        stdout,
        stderr,
        status_ok: status.success(),
    })
}

/// Run a command that must succeed; map a failure to a classified error.
pub(crate) fn run_checked(cmd: Command, stdin: Option<&[u8]>, what: &str) -> Result<Vec<u8>> {
    let r = run(cmd, stdin)?;
    if r.status_ok {
        Ok(r.stdout)
    } else {
        Err(classify(&r.stderr, what))
    }
}

/// Classify a Git stderr blob into a structured error.
///
/// Diagnostics are redacted: we keep only the first few lines and never echo
/// full URLs (which can carry tokens) verbatim into the summary.
pub(crate) fn classify(stderr: &[u8], what: &str) -> Error {
    let text = String::from_utf8_lossy(stderr);
    let low = text.to_lowercase();

    let (code, action): (ErrorCode, Option<&str>) = if low.contains("authentication failed")
        || low.contains("could not read username")
        || low.contains("could not read password")
        || low.contains("terminal prompts disabled")
        || low.contains("permission denied (publickey")
    {
        (
            ErrorCode::Authentication,
            Some("refresh credentials (e.g. `git lazy-mount doctor`) and retry"),
        )
    } else if (low.contains("filter") && low.contains("not"))
        || low.contains("unexpected 'filter'")
        || low.contains("does not support")
    {
        (
            ErrorCode::UnsupportedRemoteCapability,
            Some("retry without the partial-clone filter, or use --allow-full-object-clone"),
        )
    } else if low.contains("could not resolve host")
        || low.contains("could not connect")
        || low.contains("connection timed out")
        || low.contains("network is unreachable")
        || low.contains("unable to access")
    {
        (
            ErrorCode::OfflineMissingObject,
            Some("check network connectivity and retry"),
        )
    } else if low.contains("cannot lock ref")
        || low.contains("but expected")
        || low.contains("fetch first")
        || low.contains("non-fast-forward")
        || low.contains("stale info")
    {
        (
            ErrorCode::ConcurrentBranchMovement,
            Some("the ref moved underneath us; re-read and retry"),
        )
    } else {
        (ErrorCode::Internal, None)
    };

    let first_lines: String = text.lines().take(3).collect::<Vec<_>>().join("; ");
    let mut err = Error::new(code, format!("git {what} failed: {first_lines}"));
    if let Some(a) = action {
        err = err.with_action(a);
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::ErrorCode;

    #[test]
    fn classifies_auth() {
        let e = classify(b"fatal: Authentication failed for 'https://x/y'", "fetch");
        assert_eq!(e.code, ErrorCode::Authentication);
        assert!(e.retryable);
    }

    #[test]
    fn classifies_offline() {
        let e = classify(
            b"fatal: unable to access 'https://x': Could not resolve host: x",
            "fetch",
        );
        assert_eq!(e.code, ErrorCode::OfflineMissingObject);
    }

    #[test]
    fn classifies_cas_failure() {
        let e = classify(
            b"error: cannot lock ref 'refs/heads/main': is at X but expected Y",
            "update-ref",
        );
        assert_eq!(e.code, ErrorCode::ConcurrentBranchMovement);
    }

    #[test]
    fn classifies_filter_unsupported() {
        let e = classify(b"fatal: server does not support filter", "fetch");
        assert_eq!(e.code, ErrorCode::UnsupportedRemoteCapability);
    }
}
