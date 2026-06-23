//! `glm-fs-projfs` — Windows ProjFS backend scaffold (spec §42).
//!
//! Status: **scaffold.** Windows support is not production-ready and is not
//! claimed to be (spec §54). ProjFS is a distinct architecture from FUSE — not
//! "FUSE with Windows callbacks" — so this backend is intentionally separate.
//! What remains Windows-specific:
//!
//! * placeholder creation and directory enumeration sessions, with a ContentID
//!   that identifies the logical content **and filter context**, not just a
//!   path (spec §42);
//! * required file-size metadata at placeholder time, async hydration, and
//!   callback cancellation;
//! * post-operation notifications (which may arrive out of transactional order)
//!   reconciled via a journal plus a startup FSCK;
//! * offline-modification reconciliation when the provider was not running;
//! * reserved names, invalid characters, trailing dots/spaces, long paths,
//!   reparse points, symlink policy, alternate data streams;
//! * antivirus/indexer interaction.
//!
//! If ProjFS cannot provide required semantics on a supported Windows version,
//! a WinFsp backend would be added as an explicit, separate backend (never
//! hiding semantic differences). Real ProjFS behavior must be validated on
//! Windows before it is labeled supported (spec §54).

#![forbid(unsafe_code)]

use std::path::Path;

use glm_core::{Error, ErrorCode, Result};

/// Whether a usable ProjFS backend is available at runtime.
pub fn backend_available() -> bool {
    false
}

/// Attempt to mount at `mountpoint` (currently unavailable; see module docs).
pub fn mount(mountpoint: &Path) -> Result<()> {
    Err(Error::new(
        ErrorCode::FilesystemBackendUnavailable,
        format!(
            "the Windows ProjFS backend is not implemented yet ({})",
            mountpoint.display()
        ),
    )
    .with_action("use the headless CLI; track Windows support in docs/platform-windows.md"))
}
