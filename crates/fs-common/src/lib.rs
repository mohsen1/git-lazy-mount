//! `glm-fs-common` — backend-independent filesystem support types.
//!
//! * [`InodeTable`] — stable inode/file identity with generations, rename
//!   preservation, and open-unlink semantics.
//!
//! Platform-specific FFI and `unsafe` live in the FUSE backend crate, not here.

#![forbid(unsafe_code)]

mod inode;

pub use inode::{InodeTable, ROOT_INO};
