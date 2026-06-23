//! `glm-core` — foundational types for **git-lazy-mount**.
//!
//! This crate is backend- and platform-independent. It defines the vocabulary
//! every other crate speaks:
//!
//! * [`ObjectId`] / [`ObjectFormat`] — object identity, never assuming SHA-1.
//! * [`RepoPath`] — raw Git path bytes with reversible escaping.
//! * [`GitMode`] / [`TreeEntry`] / [`TreeObject`] — Git tree structure.
//! * [`WorkspaceViewId`], [`OperationId`], [`WorkspaceId`], [`RepoId`],
//!   [`MountGeneration`] — distinct identifier newtypes.
//! * [`FetchPolicy`] / [`FetchPriority`] — object-fetch authorization.
//! * [`Source`], [`SemanticStatus`], [`Residency`], [`Durability`] — the
//!   *orthogonal* per-path state model (spec §12).
//! * [`Error`] / [`ErrorCode`] — the typed error model (spec §47).
//!
//! See `docs/state-model.md` for how these compose.

#![forbid(unsafe_code)]

pub mod error;
pub mod fetch;
pub mod ids;
pub mod mode;
pub mod object_id;
pub mod path;
pub mod state;
pub mod tree;

pub use error::{Error, ErrorCode, ErrorJson, Result};
pub use fetch::{FetchPolicy, FetchPriority};
pub use ids::{MountGeneration, OperationId, RepoId, WorkspaceId, WorkspaceViewId};
pub use mode::GitMode;
pub use object_id::{ObjectFormat, ObjectId, ObjectIdParseError};
pub use path::{PathError, RepoPath};
pub use state::{Durability, Residency, SemanticStatus, Source};
pub use tree::{TreeEntry, TreeObject};
