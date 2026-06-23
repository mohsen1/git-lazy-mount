//! Platform-appropriate data roots and the on-disk layout (spec §8).

use std::path::{Path, PathBuf};

use glm_core::{RepoId, WorkspaceId};

const APP: &str = "git-lazy-mount";

/// The set of base directories git-lazy-mount uses, resolved per platform.
#[derive(Clone, Debug)]
pub struct DataRoots {
    /// Reproducible/evictable caches (Git objects, filtered content).
    pub cache: PathBuf,
    /// Durable state (workspaces, operation logs, overlays).
    pub state: PathBuf,
    /// User configuration.
    pub config: PathBuf,
    /// Durable application data (shared stores).
    pub data: PathBuf,
}

impl DataRoots {
    /// Resolve roots from the environment for the current platform.
    ///
    /// * Linux: XDG base directories (`XDG_CACHE_HOME`, `XDG_STATE_HOME`,
    ///   `XDG_CONFIG_HOME`, `XDG_DATA_HOME`).
    /// * macOS: `~/Library/Caches` and `~/Library/Application Support`.
    /// * Windows: `%LOCALAPPDATA%`.
    pub fn for_user() -> DataRoots {
        #[cfg(target_os = "linux")]
        {
            let home = home_dir();
            DataRoots {
                cache: xdg("XDG_CACHE_HOME", &home, ".cache"),
                state: xdg("XDG_STATE_HOME", &home, ".local/state"),
                config: xdg("XDG_CONFIG_HOME", &home, ".config"),
                data: xdg("XDG_DATA_HOME", &home, ".local/share"),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let home = home_dir();
            let app_support = home.join("Library/Application Support").join(APP);
            DataRoots {
                cache: home.join("Library/Caches").join(APP),
                state: app_support.join("state"),
                config: app_support.join("config"),
                data: app_support.clone(),
            }
        }
        #[cfg(target_os = "windows")]
        {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
                .join(APP);
            DataRoots {
                cache: base.join("cache"),
                state: base.join("state"),
                config: base.join("config"),
                data: base.join("data"),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let home = home_dir();
            let base = home.join(format!(".{APP}"));
            DataRoots {
                cache: base.join("cache"),
                state: base.join("state"),
                config: base.join("config"),
                data: base.join("data"),
            }
        }
    }

    /// All-under-one-directory roots for tests and ephemeral runs.
    pub fn ephemeral(base: impl AsRef<Path>) -> DataRoots {
        let base = base.as_ref();
        DataRoots {
            cache: base.join("cache"),
            state: base.join("state"),
            config: base.join("config"),
            data: base.join("data"),
        }
    }

    /// The layout helper for resolving concrete paths.
    pub fn layout(&self) -> Layout<'_> {
        Layout { roots: self }
    }
}

#[cfg(target_os = "linux")]
fn xdg(var: &str, home: &Path, default: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v).join(APP),
        _ => home.join(default).join(APP),
    }
}

#[allow(dead_code)]
fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    if let Some(h) = std::env::var_os("USERPROFILE") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    PathBuf::from(".")
}

/// Resolves concrete on-disk paths from [`DataRoots`] (spec §8 layout).
pub struct Layout<'a> {
    roots: &'a DataRoots,
}

impl Layout<'_> {
    /// `data/repos/<repo-id>/` — a shared repository store directory.
    pub fn repo_dir(&self, repo: &RepoId) -> PathBuf {
        self.roots.data.join("repos").join(&repo.0)
    }

    /// `.../repos/<repo-id>/git` — the bare Git store.
    pub fn git_store_dir(&self, repo: &RepoId) -> PathBuf {
        self.repo_dir(repo).join("git")
    }

    /// `.../repos/<repo-id>/metadata-cache`.
    pub fn metadata_cache_dir(&self, repo: &RepoId) -> PathBuf {
        self.repo_dir(repo).join("metadata-cache")
    }

    /// `.../repos/<repo-id>/filtered-content-cache`.
    pub fn filtered_content_cache_dir(&self, repo: &RepoId) -> PathBuf {
        self.repo_dir(repo).join("filtered-content-cache")
    }

    /// `.../repos/<repo-id>/locks`.
    pub fn repo_locks_dir(&self, repo: &RepoId) -> PathBuf {
        self.repo_dir(repo).join("locks")
    }

    /// `state/workspaces/<workspace-id>/`.
    pub fn workspace_dir(&self, ws: &WorkspaceId) -> PathBuf {
        self.roots.state.join("workspaces").join(&ws.0)
    }

    /// `state/workspaces/`.
    pub fn workspaces_dir(&self) -> PathBuf {
        self.roots.state.join("workspaces")
    }

    /// `state/daemon/` — endpoint, pid, state.
    pub fn daemon_dir(&self) -> PathBuf {
        self.roots.state.join("daemon")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_layout_paths() {
        let dir = tempfile::tempdir().unwrap();
        let roots = DataRoots::ephemeral(dir.path());
        let layout = roots.layout();
        let repo = RepoId("example-repo-abc123".into());
        assert!(layout
            .git_store_dir(&repo)
            .ends_with("repos/example-repo-abc123/git"));
        let ws = WorkspaceId("ws1".into());
        assert!(layout.workspace_dir(&ws).ends_with("workspaces/ws1"));
        assert!(layout.daemon_dir().ends_with("daemon"));
    }
}
