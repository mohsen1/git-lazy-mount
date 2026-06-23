//! `glm-stage` — the persistent staged delta (spec §10, §23).
//!
//! The stage is a *third tree* distinct from both HEAD and the writable overlay
//! (spec §11). `git lazy-mount add` records a staged blob here without touching
//! the working overlay; `commit` materializes the staged delta onto HEAD. The
//! stage is stored as a delta against HEAD (changed paths only), so it is
//! O(staged paths), never O(repository).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glm_core::{Error, ErrorCode, GitMode, ObjectId, RepoPath, Result};
use serde::{Deserialize, Serialize};

/// A single staged change at a path, relative to HEAD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StagedChange {
    /// Stage a blob with a mode (add or modify).
    Set {
        /// The staged blob (already clean-filtered and written to the store).
        oid: ObjectId,
        /// The Git mode for the entry.
        mode: GitMode,
    },
    /// Stage a deletion of a path that exists in HEAD.
    Remove,
    /// Intent-to-add: the path is tracked for the next add but has no content
    /// staged yet (`git add -N`).
    IntentToAdd,
}

#[derive(Default, Serialize, Deserialize)]
struct StageData {
    changes: BTreeMap<RepoPath, StagedChange>,
}

/// The staged delta, persisted to a single JSON manifest.
pub struct Stage {
    file: PathBuf,
    data: Mutex<StageData>,
}

impl Stage {
    /// Open (or initialize) the stage rooted under `dir` (uses `dir/index.json`).
    pub fn open(dir: impl AsRef<Path>) -> Result<Stage> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let file = dir.join("index.json");
        let data = if file.exists() {
            let bytes = std::fs::read(&file)?;
            serde_json::from_slice(&bytes).map_err(|e| {
                Error::new(
                    ErrorCode::OverlayCorruption,
                    format!("corrupt stage index: {e}"),
                )
            })?
        } else {
            StageData::default()
        };
        Ok(Stage {
            file,
            data: Mutex::new(data),
        })
    }

    fn persist(&self, data: &StageData) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(data)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("encode stage: {e}")))?;
        let dir = self
            .file
            .parent()
            .ok_or_else(|| Error::internal("stage file has no parent"))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.file).map_err(|e| {
            Error::new(ErrorCode::Internal, format!("persist stage: {e}")).with_source(e.error)
        })?;
        Ok(())
    }

    /// Stage a blob at `path` with `mode` (overwrites any prior staged change).
    pub fn set(&self, path: RepoPath, oid: ObjectId, mode: GitMode) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.changes.insert(path, StagedChange::Set { oid, mode });
        self.persist(&data)
    }

    /// Stage a deletion of `path`.
    pub fn remove(&self, path: RepoPath) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.changes.insert(path, StagedChange::Remove);
        self.persist(&data)
    }

    /// Record intent-to-add for `path`.
    pub fn intent_to_add(&self, path: RepoPath) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.changes
            .entry(path)
            .or_insert(StagedChange::IntentToAdd);
        self.persist(&data)
    }

    /// Unstage a path (drop its staged change, reverting it to match HEAD).
    pub fn unstage(&self, path: &RepoPath) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.changes.remove(path);
        self.persist(&data)
    }

    /// The staged change for `path`, if any.
    pub fn get(&self, path: &RepoPath) -> Option<StagedChange> {
        self.data.lock().unwrap().changes.get(path).cloned()
    }

    /// All staged changes, in path order.
    pub fn entries(&self) -> Vec<(RepoPath, StagedChange)> {
        self.data
            .lock()
            .unwrap()
            .changes
            .iter()
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect()
    }

    /// Reset the stage to empty (spec §24 step 10: after commit).
    pub fn clear(&self) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.changes.clear();
        self.persist(&data)
    }

    /// Whether nothing is staged.
    pub fn is_empty(&self) -> bool {
        self.data.lock().unwrap().changes.is_empty()
    }

    /// Number of staged paths.
    pub fn len(&self) -> usize {
        self.data.lock().unwrap().changes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glm_core::ObjectFormat;

    fn p(s: &str) -> RepoPath {
        RepoPath::from_bytes(s.as_bytes().to_vec()).unwrap()
    }
    fn oid(b: u8) -> ObjectId {
        ObjectId {
            format: ObjectFormat::Sha1,
            bytes: vec![b; 20],
        }
    }

    #[test]
    fn stage_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = Stage::open(dir.path()).unwrap();
            s.set(p("a.txt"), oid(1), GitMode::Regular).unwrap();
            s.remove(p("old.txt")).unwrap();
            assert_eq!(s.len(), 2);
        }
        let s = Stage::open(dir.path()).unwrap();
        assert_eq!(
            s.get(&p("a.txt")),
            Some(StagedChange::Set {
                oid: oid(1),
                mode: GitMode::Regular
            })
        );
        assert_eq!(s.get(&p("old.txt")), Some(StagedChange::Remove));
    }

    #[test]
    fn unstage_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stage::open(dir.path()).unwrap();
        s.set(p("a"), oid(1), GitMode::Regular).unwrap();
        s.set(p("b"), oid(2), GitMode::Executable).unwrap();
        s.unstage(&p("a")).unwrap();
        assert!(s.get(&p("a")).is_none());
        assert_eq!(s.len(), 1);
        s.clear().unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn intent_to_add_does_not_overwrite_set() {
        let dir = tempfile::tempdir().unwrap();
        let s = Stage::open(dir.path()).unwrap();
        s.set(p("a"), oid(9), GitMode::Regular).unwrap();
        s.intent_to_add(p("a")).unwrap();
        // The concrete staged content wins over a later intent-to-add.
        assert!(matches!(s.get(&p("a")), Some(StagedChange::Set { .. })));
    }
}
