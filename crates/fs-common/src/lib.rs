//! `glm-fs-common` — backend-independent filesystem support types.
//!
//! * [`InodeTable`] — stable inode/file identity with generations, rename
//!   preservation, and open-unlink semantics.
//! * [`Pool`] — a small bounded worker pool shared by the FUSE mount and the
//!   worktree projection's off-callback prefetch.
//!
//! Platform-specific FFI and `unsafe` live in the FUSE backend crate, not here.

#![forbid(unsafe_code)]

mod inode;
mod pool;

pub use inode::{InodeTable, ROOT_INO};
pub use pool::Pool;
