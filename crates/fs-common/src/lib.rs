//! `glm-fs-common` — backend-independent filesystem support types.
//!
//! Shared by every platform backend (FUSE/FSKit/ProjFS):
//! * [`InodeTable`] — stable inode/file identity with generations, rename
//!   preservation, and open-unlink semantics (spec §19).
//! * [`FileAttr`] — neutral, synthetic-by-default attributes (spec §28).
//!
//! Platform-specific FFI and `unsafe` live in the per-backend crates, not here.

#![forbid(unsafe_code)]

mod attr;
mod inode;

pub use attr::FileAttr;
pub use inode::{InodeTable, ROOT_INO};
