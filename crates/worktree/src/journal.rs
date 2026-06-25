//! Durable change journal + FSMonitor v2 token.
//!
//! Git's FSMonitor v2 hook is given `(version, previous_token)` and must return a
//! new token, a NUL, then the relative paths that changed since that token. The
//! response must be **inclusive** — false positives are fine, false negatives
//! are not. When continuity cannot be proven, the answer is the single
//! path `/` (full invalidation).
//!
//! The token identifies `workspace : epoch : seq : projection-generation`
//!. The journal is **durable** (an append log replayed on open) so the
//! daemon can answer queries across restarts; a process-local `Mutex<Vec<…>>` is
//! explicitly *not* sufficient. The in-memory state is a disposable
//! cache rebuilt from the log.
//!
//! NOTE: bumping `epoch` on a *detected* crash/discontinuity (so stale tokens get
//! `/`) is a later refinement; this slice preserves the epoch across a clean
//! reopen and returns `/` for any token it cannot place.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, Result};

const TOKEN_PREFIX: &str = "glm1";

/// The journal directory inside an admin gitdir. The serve daemon writes the log
/// here; the `core.fsmonitor` hook reads it.
pub fn journal_dir(gitdir: &Path) -> PathBuf {
    gitdir.join("glm-fsmonitor")
}

/// A stable per-workspace id derived from the admin gitdir path. The serve daemon
/// and the FSMonitor hook derive it identically, so their tokens match without a
/// shared metadata file. The journal's `epoch`/`generation` are fixed (1/0): the
/// durable log preserves continuity across remount, and a reset/short log is
/// caught by the seq comparison in [`ChangeJournal::query`] (→ full invalidation).
pub fn workspace_id(gitdir: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    gitdir.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// A parsed FSMonitor token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Opaque workspace id.
    pub workspace: String,
    /// Journal epoch (incremented when continuity is lost).
    pub epoch: u64,
    /// Monotonic change sequence.
    pub seq: u64,
    /// Projection generation (bumped when the baseline advances).
    pub generation: u64,
}

impl Token {
    /// Render as the opaque wire token `glm1:ws:epoch:seq:gen`.
    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            TOKEN_PREFIX, self.workspace, self.epoch, self.seq, self.generation
        )
    }

    /// Parse a wire token; `None` if malformed or not ours.
    pub fn parse(s: &str) -> Option<Token> {
        let mut it = s.split(':');
        if it.next()? != TOKEN_PREFIX {
            return None;
        }
        let workspace = it.next()?.to_string();
        let epoch = it.next()?.parse().ok()?;
        let seq = it.next()?.parse().ok()?;
        let generation = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(Token {
            workspace,
            epoch,
            seq,
            generation,
        })
    }
}

/// The answer to an FSMonitor query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// Continuity is broken — git must treat everything as possibly changed.
    FullInvalidation { token: Token },
    /// The inclusive set of paths changed since the queried token.
    Changes { token: Token, paths: Vec<Vec<u8>> },
}

impl Query {
    /// Serialize to the FSMonitor v2 wire reply: `token \0 path \0 path …`. A
    /// full invalidation is the single path `/`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Query::FullInvalidation { token } => {
                out.extend_from_slice(token.encode().as_bytes());
                out.push(0);
                out.extend_from_slice(b"/");
                out.push(0);
            }
            Query::Changes { token, paths } => {
                out.extend_from_slice(token.encode().as_bytes());
                out.push(0);
                for p in paths {
                    out.extend_from_slice(p);
                    out.push(0);
                }
            }
        }
        out
    }
}

/// A durable, append-only change journal for one workspace.
pub struct ChangeJournal {
    workspace: String,
    epoch: u64,
    generation: u64,
    log_path: PathBuf,
    state: Mutex<State>,
    /// Test-only failure-injection seam: when set, the next [`record`] returns an
    /// error before touching the log, simulating a journal write/fsync failure.
    #[cfg(test)]
    fail_next: std::sync::atomic::AtomicBool,
}

struct State {
    log: File,
    /// All recorded paths; index `i` corresponds to seq `i + 1`. (Bounded by
    /// compaction in a later refinement; kept whole here.)
    paths: Vec<Vec<u8>>,
}

impl ChangeJournal {
    /// Open (creating if absent) the journal for `workspace` rooted at `dir`,
    /// replaying the append log. `epoch`/`generation` identify this incarnation.
    pub fn open(
        dir: impl Into<PathBuf>,
        workspace: impl Into<String>,
        epoch: u64,
        generation: u64,
    ) -> Result<ChangeJournal> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(io("create journal dir"))?;
        let log_path = dir.join("changes.log");

        // Replay the existing log (NUL-separated path records).
        let mut paths = Vec::new();
        if let Ok(mut f) = File::open(&log_path) {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(io("read journal"))?;
            for rec in buf.split(|&b| b == 0) {
                if !rec.is_empty() {
                    paths.push(rec.to_vec());
                }
            }
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(io("open journal log"))?;

        Ok(ChangeJournal {
            workspace: workspace.into(),
            epoch,
            generation,
            log_path,
            state: Mutex::new(State { log, paths }),
            #[cfg(test)]
            fail_next: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Test-only: arm the next [`record`] call to fail before any write, so a
    /// caller can verify it fails the operation rather than applying an
    /// un-journaled mutation.
    #[cfg(test)]
    pub fn fail_next_record(&self) {
        self.fail_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn token_at(&self, seq: u64) -> Token {
        Token {
            workspace: self.workspace.clone(),
            epoch: self.epoch,
            seq,
            generation: self.generation,
        }
    }

    /// The current token (seq = number of records).
    pub fn current_token(&self) -> Token {
        let seq = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paths
            .len() as u64;
        self.token_at(seq)
    }

    /// Record that `path` changed (durably appended; fsynced). Inclusive — over-
    /// reporting is safe.
    pub fn record(&self, path: &[u8]) -> Result<()> {
        #[cfg(test)]
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(Error::new(ErrorCode::Internal, "injected journal failure"));
        }
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        st.log.write_all(path).map_err(io("append journal"))?;
        st.log.write_all(&[0]).map_err(io("append journal"))?;
        st.log.sync_data().map_err(io("fsync journal"))?;
        st.paths.push(path.to_vec());
        Ok(())
    }

    /// Answer an FSMonitor query for the opaque `prev` token. Returns a
    /// full invalidation for any token this journal cannot place: empty/malformed,
    /// a different workspace/epoch/generation, a future seq, or one compacted
    /// away.
    pub fn query(&self, prev: &str) -> Query {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cur_seq = st.paths.len() as u64;
        let token = self.token_at(cur_seq);

        // bootstrap: an empty `prev` while the journal is still at seq 0 (no
        // worktree write recorded) means "nothing changed since the index was
        // built from HEAD" — return an EMPTY change set so git trusts the freshly
        // `read-tree`'d index WITHOUT hashing/statting every file. This makes the
        // *first* clean `git status` fault 0 blobs. The moment any write
        // advances seq, an empty/unknown prev falls back to full invalidation.
        if prev.is_empty() {
            return if cur_seq == 0 {
                Query::Changes {
                    token,
                    paths: Vec::new(),
                }
            } else {
                Query::FullInvalidation { token }
            };
        }

        let Some(p) = Token::parse(prev) else {
            return Query::FullInvalidation { token };
        };
        if p.workspace != self.workspace
            || p.epoch != self.epoch
            || p.generation != self.generation
            || p.seq > cur_seq
        {
            return Query::FullInvalidation { token };
        }
        // Inclusive set of paths with seq in (p.seq, cur_seq].
        let from = p.seq as usize;
        let mut paths: Vec<Vec<u8>> = st.paths[from..].to_vec();
        paths.sort();
        paths.dedup();
        Query::Changes { token, paths }
    }

    /// The on-disk log path (diagnostics).
    pub fn log_path(&self) -> &std::path::Path {
        &self.log_path
    }
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::new(ErrorCode::Internal, format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrips() {
        let t = Token {
            workspace: "ws1".into(),
            epoch: 3,
            seq: 42,
            generation: 7,
        };
        assert_eq!(Token::parse(&t.encode()), Some(t));
        assert_eq!(Token::parse(""), None);
        assert_eq!(Token::parse("glm1:ws:1:2"), None); // too few fields
        assert_eq!(Token::parse("other:ws:1:2:3"), None);
    }

    #[test]
    fn changes_since_token_are_inclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        let t0 = j.current_token();
        j.record(b"a.txt").unwrap();
        j.record(b"dir/b.txt").unwrap();
        match j.query(&t0.encode()) {
            Query::Changes { paths, token } => {
                assert_eq!(paths, vec![b"a.txt".to_vec(), b"dir/b.txt".to_vec()]);
                assert_eq!(token.seq, 2);
            }
            other => panic!("expected changes, got {other:?}"),
        }
        // querying with the latest token yields no changes
        let t2 = j.current_token();
        assert!(matches!(j.query(&t2.encode()), Query::Changes { paths, .. } if paths.is_empty()));
    }

    #[test]
    fn full_invalidation_on_unplaceable_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        j.record(b"x").unwrap();
        // empty / malformed
        assert!(matches!(j.query(""), Query::FullInvalidation { .. }));
        // different workspace
        assert!(matches!(
            j.query("glm1:other:1:0:0"),
            Query::FullInvalidation { .. }
        ));
        // different epoch
        assert!(matches!(
            j.query("glm1:ws:2:0:0"),
            Query::FullInvalidation { .. }
        ));
        // future seq
        assert!(matches!(
            j.query("glm1:ws:1:99:0"),
            Query::FullInvalidation { .. }
        ));
        // generation changed (baseline advanced)
        assert!(matches!(
            j.query("glm1:ws:1:0:5"),
            Query::FullInvalidation { .. }
        ));
        // the wire reply for a full invalidation is `token \0 / \0`
        let enc = j.query("").encode();
        assert!(enc.ends_with(b"/\0"));
    }

    #[test]
    fn empty_prev_at_seq_zero_is_the_bootstrap_no_changes() {
        //: a fresh journal (no writes recorded) answers an empty prev with
        // an EMPTY change set, so git trusts the freshly read-tree'd index without
        // hashing — the first clean `git status` faults 0 blobs.
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        match j.query("") {
            Query::Changes { paths, token } => {
                assert!(paths.is_empty(), "bootstrap must report no changes");
                assert_eq!(token.seq, 0, "bootstrap token is at seq 0");
            }
            other => panic!("bootstrap must be empty changes, got {other:?}"),
        }
        // The wire reply is the token + a NUL with NO trailing `/` path.
        let enc = j.query("").encode();
        assert!(
            enc.ends_with(b"\0") && !enc.ends_with(b"/\0"),
            "bootstrap reply has no paths"
        );
        // After a write, empty prev is no longer quiescent → full invalidation.
        j.record(b"f.txt").unwrap();
        assert!(matches!(j.query(""), Query::FullInvalidation { .. }));
    }

    #[test]
    fn journal_is_durable_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
            j.record(b"one").unwrap();
            j.record(b"two").unwrap();
        }
        let j2 = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        assert_eq!(j2.current_token().seq, 2, "replayed seq survives reopen");
        let t0 = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 0,
            generation: 0,
        };
        match j2.query(&t0.encode()) {
            Query::Changes { paths, .. } => {
                assert_eq!(paths, vec![b"one".to_vec(), b"two".to_vec()])
            }
            other => panic!("expected durable changes, got {other:?}"),
        }
    }
}
