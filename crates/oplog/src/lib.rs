//! `glm-oplog` — append-only operation log, transactions, and recovery (§13).
//!
//! The durability contract (spec §13):
//!
//! 1. write the new (immutable) view record and fsync it;
//! 2. write the new (immutable) operation record and fsync it;
//! 3. *only then* atomically advance `CURRENT`.
//!
//! Because `CURRENT` is the single source of truth and is advanced last, a
//! crash at any earlier step leaves the previous committed state fully intact;
//! any view/op files written before the crash are simply unreferenced orphans
//! (spec §43). The `desired`/`applied` generation pair detects a workspace whose
//! filesystem projection has not caught up (a *stale* workspace, spec §2.5).
//!
//! This module ships deterministic crash injection ([`CrashPoint`]) so tests can
//! verify the contract at every persistence boundary (spec §50 crash injection).

#![forbid(unsafe_code)]

mod record;
mod view;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use glm_core::{Durability, Error, ErrorCode, OperationId, Result, WorkspaceViewId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use record::{Cause, ExternalSideEffect, Operation};
pub use view::WorkspaceView;

/// Persistence boundaries at which a crash can be deterministically injected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// After the view record is durable, before the operation record.
    AfterViewDurable,
    /// After the operation record is durable, before advancing `CURRENT`.
    AfterOpDurable,
    /// Immediately before the atomic `CURRENT` swap.
    BeforeCurrentSwap,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CurrentState {
    current_op: Option<OperationId>,
    desired_generation: u64,
    applied_generation: u64,
}

/// Metadata for a new operation (the view is supplied separately).
pub struct NewOperation {
    /// What caused it.
    pub cause: Cause,
    /// Human description.
    pub description: String,
    /// Durability reached for the user data this operation seals.
    pub durability: Durability,
    /// External side effects (pushes, etc.).
    pub external_effects: Vec<ExternalSideEffect>,
}

/// A structured recovery report (spec §43 step 9).
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    /// The current operation id, if any.
    pub current_op: Option<OperationId>,
    /// Whether the projection is behind the desired generation.
    pub stale: bool,
    /// Problems found (each a human description).
    pub issues: Vec<String>,
    /// Whether the log is internally consistent.
    pub healthy: bool,
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The operation log rooted at a workspace directory.
pub struct OpLog {
    ops_dir: PathBuf,
    views_dir: PathBuf,
    current_file: PathBuf,
    crash_point: Mutex<Option<CrashPoint>>,
}

impl OpLog {
    /// Open (or initialize) the log under `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<OpLog> {
        let dir = dir.as_ref();
        let ops_dir = dir.join("operations");
        let views_dir = dir.join("views");
        std::fs::create_dir_all(&ops_dir)?;
        std::fs::create_dir_all(&views_dir)?;
        Ok(OpLog {
            ops_dir,
            views_dir,
            current_file: dir.join("CURRENT"),
            crash_point: Mutex::new(None),
        })
    }

    /// Install a crash point for the next transaction (test-only behavior, but
    /// always compiled so behavior is identical in release).
    pub fn set_crash_point(&self, point: Option<CrashPoint>) {
        *self.crash_point.lock().unwrap() = point;
    }

    fn check_crash(&self, point: CrashPoint) -> Result<()> {
        if *self.crash_point.lock().unwrap() == Some(point) {
            return Err(Error::new(
                ErrorCode::Internal,
                format!("simulated crash at {point:?}"),
            ));
        }
        Ok(())
    }

    fn load_current(&self) -> Result<CurrentState> {
        if !self.current_file.exists() {
            return Ok(CurrentState::default());
        }
        let bytes = std::fs::read(&self.current_file)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::new(
                ErrorCode::OverlayCorruption,
                format!("corrupt CURRENT: {e}"),
            )
        })
    }

    fn store_current(&self, state: &CurrentState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| Error::internal(format!("encode CURRENT: {e}")))?;
        atomic_write(&self.current_file, &bytes)
    }

    /// The head operation id.
    pub fn head(&self) -> Result<Option<OperationId>> {
        Ok(self.load_current()?.current_op)
    }

    /// Read an operation record by id.
    pub fn get_op(&self, id: &OperationId) -> Result<Operation> {
        let path = self.ops_dir.join(format!("{}.json", id.to_hex()));
        let bytes = std::fs::read(&path).map_err(|e| {
            Error::new(
                ErrorCode::OverlayCorruption,
                format!("missing operation {}: {e}", id.to_hex()),
            )
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::OverlayCorruption, format!("corrupt op: {e}")))
    }

    /// Read a view record by id.
    pub fn get_view(&self, id: &WorkspaceViewId) -> Result<WorkspaceView> {
        let path = self.views_dir.join(format!("{}.json", id.to_hex()));
        let bytes = std::fs::read(&path).map_err(|e| {
            Error::new(
                ErrorCode::OverlayCorruption,
                format!("missing view {}: {e}", id.to_hex()),
            )
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::OverlayCorruption, format!("corrupt view: {e}")))
    }

    /// The current workspace view (the one `CURRENT` points at), if any.
    pub fn current_view(&self) -> Result<Option<WorkspaceView>> {
        match self.head()? {
            Some(op_id) => {
                let op = self.get_op(&op_id)?;
                Ok(Some(self.get_view(&op.view)?))
            }
            None => Ok(None),
        }
    }

    /// Commit a transaction: persist `view` then a new operation, then advance
    /// `CURRENT` — in that order (spec §13).
    pub fn commit(&self, mut view: WorkspaceView, meta: NewOperation) -> Result<OperationId> {
        let prev = self.load_current()?;
        let parents: Vec<OperationId> = prev.current_op.iter().cloned().collect();

        // Assign the view id (content + nonce) and its parents.
        view.parent_ops = parents.clone();
        let view_id = WorkspaceViewId(gen_id(
            b"view",
            &serde_json::to_vec(&view).unwrap_or_default(),
        ));
        view.id = view_id.clone();

        // (1) durable view
        let view_bytes = serde_json::to_vec_pretty(&view)
            .map_err(|e| Error::internal(format!("encode view: {e}")))?;
        atomic_write(
            &self.views_dir.join(format!("{}.json", view_id.to_hex())),
            &view_bytes,
        )?;
        self.check_crash(CrashPoint::AfterViewDurable)?;

        // (2) durable operation
        let op_id = OperationId(gen_id(b"op", &view_bytes));
        let op = Operation {
            id: op_id.clone(),
            parents,
            view: view_id,
            timestamp_unix: now_unix(),
            user: env_or("USER", "unknown"),
            hostname: env_or("HOSTNAME", "unknown"),
            pid: std::process::id(),
            cause: meta.cause,
            description: meta.description,
            durability: meta.durability,
            external_effects: meta.external_effects,
        };
        let op_bytes = serde_json::to_vec_pretty(&op)
            .map_err(|e| Error::internal(format!("encode op: {e}")))?;
        atomic_write(
            &self.ops_dir.join(format!("{}.json", op_id.to_hex())),
            &op_bytes,
        )?;
        self.check_crash(CrashPoint::AfterOpDurable)?;

        // (3) atomic CURRENT swap (last)
        self.check_crash(CrashPoint::BeforeCurrentSwap)?;
        let next = CurrentState {
            current_op: Some(op_id.clone()),
            desired_generation: view.mount_generation,
            applied_generation: prev.applied_generation,
        };
        self.store_current(&next)?;
        Ok(op_id)
    }

    /// Mark the filesystem projection as caught up to `generation` (spec §13
    /// step 10). Clears staleness when it reaches the desired generation.
    pub fn mark_applied(&self, generation: u64) -> Result<()> {
        let mut state = self.load_current()?;
        state.applied_generation = generation;
        self.store_current(&state)
    }

    /// Whether the projected generation lags the desired one (stale workspace).
    pub fn is_stale(&self) -> Result<bool> {
        let s = self.load_current()?;
        Ok(s.desired_generation != s.applied_generation)
    }

    /// The desired and applied generations.
    pub fn generations(&self) -> Result<(u64, u64)> {
        let s = self.load_current()?;
        Ok((s.desired_generation, s.applied_generation))
    }

    /// Walk the operation history from the head, newest first, up to `limit`.
    pub fn log(&self, limit: usize) -> Result<Vec<Operation>> {
        let mut out = Vec::new();
        let mut cur = self.head()?;
        while let Some(id) = cur {
            if out.len() >= limit {
                break;
            }
            let op = self.get_op(&id)?;
            cur = op.parents.first().cloned();
            out.push(op);
        }
        Ok(out)
    }

    /// Validate the log and report problems without mutating user data (§43).
    pub fn recover(&self) -> Result<RecoveryReport> {
        let mut issues = Vec::new();
        let state = self.load_current().unwrap_or_default();
        if let Some(op_id) = &state.current_op {
            match self.get_op(op_id) {
                Ok(op) => {
                    if let Err(e) = self.get_view(&op.view) {
                        issues.push(format!("view for current op is unreadable: {e}"));
                    }
                }
                Err(e) => issues.push(format!("current operation is unreadable: {e}")),
            }
        }
        let stale = state.desired_generation != state.applied_generation;
        Ok(RecoveryReport {
            current_op: state.current_op,
            stale,
            healthy: issues.is_empty(),
            issues,
        })
    }
}

fn gen_id(prefix: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(prefix);
    h.update(payload);
    h.update(now_nanos().to_le_bytes());
    h.update(ID_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.finalize().to_vec()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| Error::internal("oplog target has no parent"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(target)
        .map_err(|e| Error::internal(format!("oplog rename: {e}")).with_source(e.error))?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::ObjectFormat;
    use glm_core::ObjectId;

    fn base() -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![7; 20],
        }
    }

    fn view_at(gen: u64) -> WorkspaceView {
        let mut v = WorkspaceView::root(WorkspaceViewId(vec![]), Some(base()));
        v.mount_generation = gen;
        v
    }

    fn meta(desc: &str) -> NewOperation {
        NewOperation {
            cause: Cause::Command(desc.to_string()),
            description: desc.to_string(),
            durability: Durability::OperationSealed,
            external_effects: vec![],
        }
    }

    #[test]
    fn commit_and_walk_history() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpLog::open(dir.path()).unwrap();
        assert!(log.head().unwrap().is_none());

        let op1 = log.commit(view_at(0), meta("init")).unwrap();
        let op2 = log.commit(view_at(1), meta("write file")).unwrap();
        assert_eq!(log.head().unwrap(), Some(op2.clone()));

        let history = log.log(10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, op2);
        assert_eq!(history[1].id, op1);
        assert_eq!(history[0].parents, vec![op1]);
    }

    #[test]
    fn current_view_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpLog::open(dir.path()).unwrap();
        log.commit(view_at(3), meta("x")).unwrap();
        let v = log.current_view().unwrap().unwrap();
        assert_eq!(v.mount_generation, 3);
        assert_eq!(v.base_commit, Some(base()));
    }

    #[test]
    fn staleness_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpLog::open(dir.path()).unwrap();
        log.commit(view_at(5), meta("switch")).unwrap();
        // desired=5, applied=0 -> stale until the projection catches up.
        assert!(log.is_stale().unwrap());
        log.mark_applied(5).unwrap();
        assert!(!log.is_stale().unwrap());
    }

    // Crash injection: a crash at any boundary must not advance CURRENT, so the
    // previously committed state survives intact (spec §43, §50).
    fn crash_preserves_previous(point: CrashPoint) {
        let dir = tempfile::tempdir().unwrap();
        let prev_head;
        {
            let log = OpLog::open(dir.path()).unwrap();
            prev_head = log.commit(view_at(0), meta("good op")).unwrap();
            log.set_crash_point(Some(point));
            let err = log.commit(view_at(1), meta("doomed op")).unwrap_err();
            assert!(err.summary.contains("simulated crash"));
        }
        // Reopen as if after a crash.
        let log = OpLog::open(dir.path()).unwrap();
        let report = log.recover().unwrap();
        assert!(report.healthy, "issues: {:?}", report.issues);
        // CURRENT still points at the good op; the doomed op did not take effect.
        assert_eq!(log.head().unwrap(), Some(prev_head.clone()));
        assert_eq!(log.current_view().unwrap().unwrap().mount_generation, 0);
    }

    #[test]
    fn crash_after_view_durable() {
        crash_preserves_previous(CrashPoint::AfterViewDurable);
    }

    #[test]
    fn crash_after_op_durable() {
        crash_preserves_previous(CrashPoint::AfterOpDurable);
    }

    #[test]
    fn crash_before_current_swap() {
        crash_preserves_previous(CrashPoint::BeforeCurrentSwap);
    }

    #[test]
    fn fresh_log_recovers_clean() {
        let dir = tempfile::tempdir().unwrap();
        let log = OpLog::open(dir.path()).unwrap();
        let report = log.recover().unwrap();
        assert!(report.healthy);
        assert!(report.current_op.is_none());
    }
}
