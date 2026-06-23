//! `glm-platform` — platform data roots and canonical repository identity.
//!
//! * [`DataRoots`] resolves per-platform cache/state/config/data directories
//!   (spec §8) and [`Layout`] maps them to the concrete on-disk layout.
//! * [`repo_id`] / [`canonical_identity`] derive a credential-free repository
//!   identity so a single shared store can back multiple mounts (spec §2.3).

//! * [`validate`] implements platform path representability (Windows reserved
//!   names/forbidden chars, macOS normalization/case collisions) and the four
//!   path-collision policies with a reversible escape (spec §30).

#![forbid(unsafe_code)]

mod repo_id;
mod roots;
pub mod validate;

pub use repo_id::{canonical_identity, repo_id};
pub use roots::{DataRoots, Layout};
pub use validate::{PathIssue, PathPolicy, TargetPlatform};
