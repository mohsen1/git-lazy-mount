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

/// Sliding-window prune thresholds. When the live window grows past `KEEP_MAX`
/// records (or the on-disk log exceeds `LOG_BYTE_CAP` bytes), the oldest records
/// are dropped down to `KEEP_MIN` and the log is rewritten to that suffix. `seq`
/// stays globally monotonic; the count of dropped records is persisted as `base`
/// so a reopened journal resumes at the same global seq. Hysteresis (max vs min)
/// amortizes the O(window) rewrite over many records.
#[cfg(not(test))]
const KEEP_MAX: usize = 50_000;
#[cfg(not(test))]
const KEEP_MIN: usize = 25_000;
// Small under test so a prune triggers after a handful of (fsync'd) records,
// keeping the unit tests fast while exercising the exact same prune path.
#[cfg(test)]
const KEEP_MAX: usize = 64;
#[cfg(test)]
const KEEP_MIN: usize = 32;
const LOG_BYTE_CAP: u64 = 32 << 20;

// fsync in production (crash durability); a no-op under test so the unit tests,
// which record many entries to drive a prune, don't pay ~hundreds of ms per
// fsync. Tests reopen in-process, so the written bytes are already visible.
#[cfg(not(test))]
fn fsync_data(f: &File) -> std::io::Result<()> {
    f.sync_data()
}
#[cfg(test)]
fn fsync_data(_f: &File) -> std::io::Result<()> {
    Ok(())
}
#[cfg(not(test))]
fn fsync_all(f: &File) -> std::io::Result<()> {
    f.sync_all()
}
#[cfg(test)]
fn fsync_all(_f: &File) -> std::io::Result<()> {
    Ok(())
}

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
    /// Sidecar holding the pruned-record count (`State.base`) as a decimal u64,
    /// so the global seq survives a sliding-window prune across a reopen.
    base_path: PathBuf,
    state: Mutex<State>,
    /// Test-only failure-injection seam: when set, the next [`record`] returns an
    /// error before touching the log, simulating a journal write/fsync failure.
    #[cfg(test)]
    fail_next: std::sync::atomic::AtomicBool,
}

struct State {
    log: File,
    /// The live window of recorded paths. Index `i` corresponds to global seq
    /// `base + i + 1`; the window holds exactly the records for seq in
    /// `(base, cur_seq]` in order, where `cur_seq = base + paths.len()`.
    paths: Vec<Vec<u8>>,
    /// The count of records pruned away (the global seq of the oldest record the
    /// window no longer holds). Persisted to the `base` sidecar so the global
    /// seq survives a sliding-window prune and a reopen.
    base: u64,
}

/// `cur_seq = base + paths.len()`: the global seq of the most recent record. The
/// window `paths` covers exactly seq in `(base, cur_seq]`.
fn cur_seq(st: &State) -> u64 {
    st.base + st.paths.len() as u64
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
        let base_path = dir.join("base");

        // Read the LOG first, then `base` — the reverse of the prune's write order
        // (prune persists `base` BEFORE it renames the shrunk log into place). The
        // hook opens a fresh journal per query in a process separate from the
        // daemon, so this read races the daemon's prune with no shared lock. Reading
        // log-then-base makes every torn snapshot safe: a *new short* log on disk
        // implies the prune already persisted the new `base`, so the later base read
        // observes it (consistent); a still-*old long* log with a new base just
        // OVER-counts `cur_seq` (→ over-report or full-invalidation). The one fatal
        // pairing — old small base + new short log, which would relabel retained
        // records onto already-pruned seqs and drop a change a live token needs — is
        // therefore unreachable. (Reading base-first would expose exactly that.)
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
        // `base` (pruned-record count) read AFTER the log; 0 if absent/malformed —
        // a missing/garbled base only ever makes the global seq *smaller*, which can
        // over-report or full-invalidate but never drop a change a live token needs.
        // `cur_seq` is implicitly `base + paths.len()`.
        let base: u64 = std::fs::read_to_string(&base_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
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
            base_path,
            state: Mutex::new(State { log, paths, base }),
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

    /// The current token (seq = the global record count `base + window len`).
    pub fn current_token(&self) -> Token {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.token_at(cur_seq(&st))
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
        fsync_data(&st.log).map_err(io("fsync journal"))?;
        // The append is now durable; the record is committed regardless of what
        // the prune below does (a prune is pure compaction — it never affects
        // whether this record is retained for a live token).
        st.paths.push(path.to_vec());

        // Sliding-window prune. Trigger on either bound: too many records held in
        // memory, or an on-disk log past its byte cap. On any error the window is
        // left appended-but-unpruned (still correct: `cur_seq = base + len`, just
        // not yet compacted), so a rewrite failure can never drop a committed
        // record nor break the invariant.
        if st.paths.len() > KEEP_MAX || self.log_bytes(&st) > LOG_BYTE_CAP {
            self.prune(&mut st)?;
        }
        Ok(())
    }

    /// On-disk size of `changes.log` (0 if it can't be stat'd — then the prune
    /// falls back to the record-count bound).
    fn log_bytes(&self, _st: &State) -> u64 {
        std::fs::metadata(&self.log_path)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Drop the oldest records down to `KEEP_MIN`, advance `base` by that count,
    /// and rewrite `changes.log` to the retained suffix.
    ///
    /// Crash-safety hinges on the persist ORDER. The committed record is already
    /// in the OLD `changes.log`, which holds every record `(old_base, cur_seq]`,
    /// and `base` only ever shifts the seq LABEL the replay assigns to the log's
    /// records — a larger `base` ages records forward, never backward. A live
    /// token's reply must stay inclusive over `(T.seq, cur_seq]`; the one
    /// forbidden outcome is a reopen whose `(base, log)` pair drops a record a
    /// live token still needs (a false negative). We order the persists so EVERY
    /// crash window can only OVER-report or FULL-INVALIDATE:
    ///
    ///   1. Write the retained suffix to `changes.log.tmp` and fsync it. The old
    ///      `changes.log` is untouched — a crash here loses nothing (orphan tmp).
    ///   2. Publish the NEW (larger) `base` to the sidecar (tmp + fsync + atomic
    ///      rename + dir fsync) BEFORE shrinking the log. If we crash after this
    ///      but before step 3, reopen reads `base = new_base` over the OLD long
    ///      log, so `cur_seq` is over-counted by `drop_n`: tokens below the new
    ///      base full-invalidate (safe), and tokens at/above it slice a log whose
    ///      records are now labelled with LARGER seqs, so the reply is a SUPERSET
    ///      of the truth — over-reporting, never a subset. (Publishing base AFTER
    ///      the swap is the false-negative hazard: a SHORT log with the OLD small
    ///      base would relabel the retained records onto already-pruned seqs.)
    ///   3. Atomically rename `changes.log.tmp` over `changes.log` and fsync the
    ///      dir. Now `base` and the log agree exactly: `cur_seq = base + len`.
    fn prune(&self, st: &mut State) -> Result<()> {
        if st.paths.len() <= KEEP_MIN {
            return Ok(());
        }
        let drop_n = st.paths.len() - KEEP_MIN;
        let new_base = st.base + drop_n as u64;
        let retained = &st.paths[drop_n..];

        // 1. Stage the retained suffix in a sibling temp file and fsync it. The
        //    live `changes.log` is still intact at this point.
        let tmp_log = self.log_path.with_extension("log.tmp");
        {
            let mut f = File::create(&tmp_log).map_err(io("create journal tmp"))?;
            for rec in retained {
                f.write_all(rec).map_err(io("write journal tmp"))?;
                f.write_all(&[0]).map_err(io("write journal tmp"))?;
            }
            fsync_all(&f).map_err(io("fsync journal tmp"))?;
        }

        // 2. Publish the new (larger) base BEFORE shrinking the log. A crash
        //    after this leaves the new floor with the OLD long log: a reopen
        //    rejects tokens below the new base (full-invalidation, safe) while
        //    the still-present older records can only cause over-reporting.
        //    Publishing base AFTER the log swap would be the false-negative
        //    hazard: a short log with the old (small) base would label the
        //    retained records with seqs that collide with already-pruned ones.
        write_u64_atomic(&self.base_path, new_base)?;

        // 3. Swap the shorter log into place and fsync the dir so the rename is
        //    durable. Now base and the log agree exactly.
        std::fs::rename(&tmp_log, &self.log_path).map_err(io("publish journal"))?;
        if let Some(parent) = self.log_path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = fsync_all(&dir);
            }
        }

        // Re-open the append handle on the freshly rewritten log; the old handle
        // pointed at the now-replaced inode.
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(io("reopen journal log"))?;
        st.log = log;
        st.paths.drain(..drop_n);
        st.base = new_base;
        Ok(())
    }

    /// Answer an FSMonitor query for the opaque `prev` token. Returns a
    /// full invalidation for any token this journal cannot place: empty/malformed,
    /// a different workspace/epoch/generation, a future seq, or one pruned away
    /// (`seq < base`, below the sliding window).
    pub fn query(&self, prev: &str) -> Query {
        let st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cur = cur_seq(&st);
        let token = self.token_at(cur);

        // bootstrap: an empty `prev` while the journal is still at the quiescent
        // fresh-mount state (`base == 0 && paths empty`, i.e. `cur == 0`) means
        // "nothing changed since the index was built from HEAD" — return an EMPTY
        // change set so git trusts the freshly `read-tree`'d index WITHOUT
        // hashing/statting every file. This makes the *first* clean `git status`
        // fault 0 blobs. The moment any write advances seq — or a prune advances
        // `base` (so `cur > 0` even with an empty window) — an empty/unknown prev
        // falls back to full invalidation. Guarding on `cur == 0` (not on the old
        // window-length test) is essential: after a prune the window can be empty
        // while `base > 0`, and an empty prev there must NOT be mistaken for the
        // bootstrap; it must full-invalidate.
        if prev.is_empty() {
            return if cur == 0 {
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
        if p.workspace != self.workspace || p.epoch != self.epoch || p.generation != self.generation
        {
            return Query::FullInvalidation { token };
        }
        // A future seq (ahead of `cur`) or a pruned seq (below the window floor
        // `base`) cannot be placed in the live window — eager rescan.
        if p.seq > cur || p.seq < st.base {
            return Query::FullInvalidation { token };
        }
        // Inclusive set of paths with seq in (p.seq, cur]. The window holds seq
        // in (base, cur], so the slice offset is `p.seq - base`.
        let from = (p.seq - st.base) as usize;
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

/// Durably publish a decimal u64 to `dst`: write a sibling temp, fsync it, atomic
/// rename into place, then fsync the parent dir so the rename itself survives a
/// crash. Used for the `base` sidecar during a prune.
fn write_u64_atomic(dst: &Path, value: u64) -> Result<()> {
    let tmp = dst.with_extension("tmp");
    {
        let mut f = File::create(&tmp).map_err(io("create base tmp"))?;
        f.write_all(value.to_string().as_bytes())
            .map_err(io("write base"))?;
        fsync_all(&f).map_err(io("fsync base"))?;
    }
    std::fs::rename(&tmp, dst).map_err(io("publish base"))?;
    if let Some(parent) = dst.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = fsync_all(&dir);
        }
    }
    Ok(())
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

    /// Record `n` distinct paths (`p0..p{n-1}`) so per-seq identity is checkable.
    fn record_distinct(j: &ChangeJournal, n: usize) {
        for i in 0..n {
            j.record(format!("p{i}").as_bytes()).unwrap();
        }
    }

    #[test]
    fn pruned_token_full_invalidates() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        // Cross KEEP_MAX so at least one prune fires (base advances past 1).
        record_distinct(&j, KEEP_MAX + 1);

        // A token at seq 1 (recorded long before the prune) is now below `base`.
        let pruned = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 1,
            generation: 0,
        };
        assert!(
            matches!(j.query(&pruned.encode()), Query::FullInvalidation { .. }),
            "a token below the window floor must full-invalidate"
        );

        // A token taken AFTER the prune still places and yields the exact
        // inclusive change set since it.
        let after = j.current_token();
        record_distinct_from(&j, after.seq as usize, 3);
        match j.query(&after.encode()) {
            Query::Changes { paths, token } => {
                assert_eq!(
                    paths,
                    vec![
                        format!("p{}", after.seq).into_bytes(),
                        format!("p{}", after.seq + 1).into_bytes(),
                        format!("p{}", after.seq + 2).into_bytes(),
                    ]
                );
                assert_eq!(token.seq, after.seq + 3);
            }
            other => panic!("expected changes after prune, got {other:?}"),
        }
    }

    /// Record `count` paths labelled `p{start}..p{start+count-1}` (continuing the
    /// global-seq naming so a retained token's expected slice is predictable).
    fn record_distinct_from(j: &ChangeJournal, start: usize, count: usize) {
        for i in 0..count {
            j.record(format!("p{}", start + i).as_bytes()).unwrap();
        }
    }

    #[test]
    fn inclusive_set_unchanged_by_base() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        record_distinct(&j, KEEP_MAX + 1);
        // Window now starts above 0; take a retained token, append more, and
        // verify the slice math (`from = seq - base`) returns exactly the suffix.
        let t = j.current_token();
        record_distinct_from(&j, t.seq as usize, 5);
        match j.query(&t.encode()) {
            Query::Changes { paths, token } => {
                let want: Vec<Vec<u8>> = (0..5)
                    .map(|i| format!("p{}", t.seq as usize + i).into_bytes())
                    .collect();
                assert_eq!(paths, want, "slice math must be base-relative");
                assert_eq!(token.seq, t.seq + 5);
            }
            other => panic!("expected changes, got {other:?}"),
        }
    }

    #[test]
    fn durable_base_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let global_seq;
        {
            let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
            record_distinct(&j, KEEP_MAX + 1);
            global_seq = j.current_token().seq;
            assert!(global_seq as usize > KEEP_MAX, "a prune must have fired");
        }
        // Reopen: the persisted base must restore the same global seq, and a
        // token retained in the window must still place.
        let j2 = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        assert_eq!(
            j2.current_token().seq,
            global_seq,
            "base survived the reopen, so the global seq is unchanged"
        );
        let t = j2.current_token();
        record_distinct_from(&j2, t.seq as usize, 2);
        match j2.query(&t.encode()) {
            Query::Changes { paths, token } => {
                assert_eq!(
                    paths,
                    vec![
                        format!("p{}", t.seq).into_bytes(),
                        format!("p{}", t.seq + 1).into_bytes(),
                    ]
                );
                assert_eq!(token.seq, global_seq + 2);
            }
            other => panic!("expected retained token to place, got {other:?}"),
        }
        // A token below the restored floor still full-invalidates.
        let pruned = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 1,
            generation: 0,
        };
        assert!(matches!(
            j2.query(&pruned.encode()),
            Query::FullInvalidation { .. }
        ));
    }

    #[test]
    fn cap_and_reset_then_empty_prev_full_invalidates() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        record_distinct(&j, KEEP_MAX + 1);
        // base > 0 now. An empty prev must NOT be mistaken for the seq-0
        // bootstrap (which reports no changes); it must full-invalidate.
        assert!(
            matches!(j.query(""), Query::FullInvalidation { .. }),
            "empty prev after a prune (base>0) must full-invalidate, not bootstrap"
        );
        // The wire reply is the single `/` path.
        assert!(j.query("").encode().ends_with(b"/\0"));
    }

    #[test]
    fn journal_stays_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        let n = 5 * KEEP_MAX;
        record_distinct(&j, n);
        // The in-memory window is bounded by KEEP_MAX regardless of how many
        // records were recorded.
        {
            let st = j
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(
                st.paths.len() <= KEEP_MAX,
                "window len {} must stay <= KEEP_MAX {}",
                st.paths.len(),
                KEEP_MAX
            );
            assert_eq!(
                cur_seq(&st),
                n as u64,
                "global seq must equal the total recorded, independent of pruning"
            );
        }
        assert_eq!(j.current_token().seq, n as u64);
    }

    #[test]
    fn overcount_from_stale_base_never_undercounts() {
        // The only torn (base, log) snapshot reachable from the log-first open
        // order is an OLD (long) log with a NEW (advanced) base: cur_seq is
        // OVER-counted, so a live token over-reports or full-invalidates — never a
        // subset. Construct it directly on disk: a 5-record log with base=2 (as if
        // 2 records were already pruned but the log still holds the long suffix).
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("changes.log"), b"a\0b\0c\0d\0e\0").unwrap();
        std::fs::write(tmp.path().join("base"), b"2").unwrap();
        let j = ChangeJournal::open(tmp.path(), "ws", 1, 0).unwrap();
        assert_eq!(j.current_token().seq, 7, "cur_seq = base 2 + 5 records");
        // a token below base -> full invalidation (eager rescan, safe)
        let below = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 1,
            generation: 0,
        };
        assert!(matches!(
            j.query(&below.encode()),
            Query::FullInvalidation { .. }
        ));
        // a token at base -> the whole retained window (a superset of the truth)
        let at_base = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 2,
            generation: 0,
        };
        match j.query(&at_base.encode()) {
            Query::Changes { paths, .. } => assert_eq!(paths.len(), 5),
            o => panic!("expected changes, got {o:?}"),
        }
        // a token at cur_seq -> empty (nothing since the latest)
        let at_cur = Token {
            workspace: "ws".into(),
            epoch: 1,
            seq: 7,
            generation: 0,
        };
        assert!(
            matches!(j.query(&at_cur.encode()), Query::Changes { paths, .. } if paths.is_empty())
        );
    }
}
