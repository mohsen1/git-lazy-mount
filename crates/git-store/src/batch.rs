//! A long-lived `git cat-file --batch-command` session (spec §5.3).
//!
//! One process answers many `info`/`contents` requests without re-spawning Git
//! per object. The session runs with `GIT_NO_LAZY_FETCH=1`: it serves only what
//! is *locally present* and reports everything else as `missing`. Network
//! retrieval is the fetch scheduler's job, never a side effect of a read.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use glm_core::{Error, ErrorCode, ObjectFormat, ObjectId, Result};

/// Metadata for an object (`info` reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Git object type (`blob`, `tree`, `commit`, `tag`).
    pub kind: String,
    /// Raw object size in bytes.
    pub size: u64,
}

/// A stateful cat-file session. `!Sync`; wrap in a mutex to share.
///
/// **Contract:** only query objects known to be locally present. Because the
/// session runs with `GIT_NO_LAZY_FETCH=1`, asking for a *promisor* object that
/// is missing locally makes Git terminate the process (it refuses the lazy
/// fetch). The [`ObjectProvider`](../../glm_object_provider/index.html) is the
/// residency authority and must guarantee presence before reading. A death is
/// surfaced as an error so the owner can respawn.
pub struct BatchSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    format: ObjectFormat,
    alive: bool,
}

impl BatchSession {
    /// Spawn a session against the given bare git dir. Always `GIT_NO_LAZY_FETCH`.
    pub fn spawn(git_dir: &Path, format: ObjectFormat) -> Result<BatchSession> {
        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(git_dir)
            .args(["cat-file", "--batch-command", "--buffer"])
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // Never let the long-lived cat-file session inherit a FUSE mount
        // descriptor (see proc::harden_fds).
        crate::proc::harden_fds(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
            Error::new(ErrorCode::Internal, format!("spawn cat-file: {e}")).with_source(e)
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(BatchSession {
            child,
            stdin,
            stdout,
            format,
            alive: true,
        })
    }

    /// Whether the underlying process is still usable.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    fn send(&mut self, line: &str) -> Result<()> {
        if !self.alive {
            return Err(Error::new(ErrorCode::Internal, "cat-file session is dead"));
        }
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.write_all(b"flush\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| Error::new(ErrorCode::Internal, format!("cat-file write: {e}")))
    }

    fn read_header(&mut self) -> Result<String> {
        let mut line = Vec::new();
        let n = self
            .stdout
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("cat-file read: {e}")))?;
        if n == 0 {
            self.alive = false;
            return Err(Error::new(
                ErrorCode::Internal,
                "cat-file session terminated (likely queried for a missing promisor object; \
                 the object provider must confirm residency before reading)",
            ));
        }
        while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }

    /// Query object metadata. `Ok(None)` means locally missing.
    pub fn info(&mut self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.send(&format!("info {}", oid.to_hex()))?;
        let header = self.read_header()?;
        parse_info_header(&header)
    }

    /// Read full object contents. `Ok(None)` means locally missing.
    pub fn contents(&mut self, oid: &ObjectId) -> Result<Option<(ObjectInfo, Vec<u8>)>> {
        self.send(&format!("contents {}", oid.to_hex()))?;
        let header = self.read_header()?;
        let info = match parse_info_header(&header)? {
            Some(i) => i,
            None => return Ok(None),
        };
        let mut buf = vec![0u8; info.size as usize];
        self.stdout
            .read_exact(&mut buf)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("cat-file body: {e}")))?;
        // Consume the trailing newline cat-file appends after contents.
        let mut nl = [0u8; 1];
        let _ = self.stdout.read_exact(&mut nl);
        Ok(Some((info, buf)))
    }

    /// The object format this session speaks.
    pub fn format(&self) -> &ObjectFormat {
        &self.format
    }
}

impl Drop for BatchSession {
    fn drop(&mut self) {
        // Closing stdin lets git exit; then reap to avoid a zombie.
        let _ = self.stdin.write_all(b"quit\n");
        let _ = self.child.wait();
    }
}

fn parse_info_header(header: &str) -> Result<Option<ObjectInfo>> {
    // `<oid> <type> <size>` or `<oid> missing`.
    let mut parts = header.split(' ');
    let _oid = parts.next();
    let kind = match parts.next() {
        Some(k) => k,
        None => {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("bad cat-file header: {header:?}"),
            ))
        }
    };
    if kind == "missing" {
        return Ok(None);
    }
    let size: u64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
        Error::new(
            ErrorCode::Internal,
            format!("bad cat-file size: {header:?}"),
        )
    })?;
    Ok(Some(ObjectInfo {
        kind: kind.to_string(),
        size,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_missing() {
        let r = parse_info_header("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 missing").unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn parse_present() {
        let r = parse_info_header("abc123 blob 42").unwrap().unwrap();
        assert_eq!(r.kind, "blob");
        assert_eq!(r.size, 42);
    }
}
