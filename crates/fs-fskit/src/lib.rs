//! `glm-fs-fskit` — macOS FSKit backend scaffold (spec §41).
//!
//! Status: **scaffold.** macOS support is not production-ready and is not
//! claimed to be (spec §54). The backend logic shared with other platforms
//! lives in `glm-fs-common` and `glm-workspace`; what remains macOS-specific:
//!
//! * an FSKit `FSUnaryFileSystem`/`FSVolume` extension (or an isolated macFUSE
//!   backend for older systems) bridging the same callbacks `FuseOps` exposes;
//! * runtime capability detection and clear installation diagnostics;
//! * APFS case-sensitivity and Unicode-normalization collision handling;
//! * resource forks / Finder metadata / xattrs / file flags policy (never
//!   silently committing them as Git content);
//! * file coordination, case-only rename, and system-extension lifecycle +
//!   signing/entitlements;
//! * mount recovery after a daemon or extension restart.
//!
//! Real FSKit behavior must be validated on-device before macOS is labeled
//! supported (spec §54).

#![forbid(unsafe_code)]

use std::path::Path;

use glm_core::{Error, ErrorCode, Result};

/// Whether a usable FSKit/macFUSE backend is available at runtime.
pub fn backend_available() -> bool {
    false
}

/// Attempt to mount at `mountpoint` (currently unavailable; see module docs).
pub fn mount(mountpoint: &Path) -> Result<()> {
    Err(Error::new(
        ErrorCode::FilesystemBackendUnavailable,
        format!(
            "the macOS FSKit backend is not implemented yet ({})",
            mountpoint.display()
        ),
    )
    .with_action("use the headless CLI; track macOS support in docs/platform-macos.md"))
}
