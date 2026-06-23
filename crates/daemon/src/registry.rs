//! Crash-safe, idempotent mount registry (spec §39).

use std::io::Write;
use std::path::{Path, PathBuf};

use glm_core::{Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};

/// Mount lifecycle states (spec §39).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountState {
    /// Being created (store init / fetch).
    Creating,
    /// Backend is attaching.
    Mounting,
    /// Live and serving.
    Mounted,
    /// Draining in-flight operations.
    Quiescing,
    /// Backend detaching.
    Unmounting,
    /// Detached.
    Unmounted,
    /// Recovering after a crash.
    Recovering,
    /// Failed; needs attention.
    Failed,
}

/// A registered mount: a user-facing mountpoint bound to a shared store and a
/// private workspace (spec §8, §39).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountSpec {
    /// Workspace id (stable per mount).
    pub id: String,
    /// The user-facing mountpoint path.
    pub mountpoint: PathBuf,
    /// Repository identity (credential-free).
    pub repo_id: String,
    /// The bare Git store directory (shared across mounts of the same repo).
    pub store_dir: PathBuf,
    /// The workspace directory (overlay/stage/journal).
    pub ws_dir: PathBuf,
    /// Remote name, if any.
    pub remote: Option<String>,
    /// Attached public branch ref, if any.
    pub attached_branch: Option<String>,
    /// Private workspace head ref.
    pub workspace_head_ref: String,
    /// Partial-clone filter used, if any.
    pub filter: Option<String>,
    /// Current lifecycle state.
    pub state: MountState,
}

#[derive(Default, Serialize, Deserialize)]
struct RegistryData {
    mounts: Vec<MountSpec>,
}

/// The persistent registry of mounts for this user.
pub struct Registry {
    file: PathBuf,
}

impl Registry {
    /// Open the registry stored at `file` (created lazily on first write).
    pub fn open(file: impl Into<PathBuf>) -> Result<Registry> {
        let file = file.into();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Registry { file })
    }

    fn load(&self) -> Result<RegistryData> {
        if !self.file.exists() {
            return Ok(RegistryData::default());
        }
        let bytes = std::fs::read(&self.file)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::new(ErrorCode::Configuration, format!("corrupt registry: {e}")))
    }

    fn store(&self, data: &RegistryData) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(data)
            .map_err(|e| Error::internal(format!("encode registry: {e}")))?;
        let dir = self.file.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.file)
            .map_err(|e| Error::internal(format!("persist registry: {e}")).with_source(e.error))?;
        Ok(())
    }

    /// Insert or replace a mount (idempotent by mountpoint).
    pub fn upsert(&self, spec: MountSpec) -> Result<()> {
        let mut data = self.load()?;
        data.mounts.retain(|m| m.mountpoint != spec.mountpoint);
        data.mounts.push(spec);
        self.store(&data)
    }

    /// Remove a mount by mountpoint (idempotent).
    pub fn remove(&self, mountpoint: &Path) -> Result<bool> {
        let mut data = self.load()?;
        let before = data.mounts.len();
        data.mounts.retain(|m| m.mountpoint != mountpoint);
        let removed = data.mounts.len() != before;
        self.store(&data)?;
        Ok(removed)
    }

    /// All registered mounts.
    pub fn list(&self) -> Result<Vec<MountSpec>> {
        Ok(self.load()?.mounts)
    }

    /// Find the mount whose mountpoint is `path` or the closest ancestor of it
    /// (longest match wins), so commands work from any subdirectory.
    pub fn find_for_path(&self, path: &Path) -> Result<Option<MountSpec>> {
        let data = self.load()?;
        let mut best: Option<MountSpec> = None;
        for m in data.mounts {
            if path == m.mountpoint || path.starts_with(&m.mountpoint) {
                let better = match &best {
                    Some(b) => m.mountpoint.as_os_str().len() > b.mountpoint.as_os_str().len(),
                    None => true,
                };
                if better {
                    best = Some(m);
                }
            }
        }
        Ok(best)
    }
}
