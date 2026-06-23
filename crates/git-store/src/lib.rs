//! `glm-git-store` — the authoritative adapter over the `git` binary.
//!
//! Git remains the source of truth for the network protocol, credentials, the
//! configured object format, ref transactions, working-tree filters, push, and
//! fetch (spec §9). This crate wraps those operations behind a typed API:
//!
//! * [`GitStore`] — bare store init/open, fetch (with partial-clone filters),
//!   ref compare-and-swap, push, tree/blob reads, filter plumbing, commit and
//!   tree construction.
//! * [`BatchSession`] — a long-lived `cat-file --batch-command` process that
//!   serves local objects and reports missing ones *without* fetching.
//!
//! Every subprocess runs non-interactively (`GIT_TERMINAL_PROMPT=0`) and uses
//! NUL/unambiguous delimiters where paths are involved (spec §3.18).

#![forbid(unsafe_code)]

mod batch;
mod interop;
mod proc;
mod store;
pub mod tree_parse;

pub use batch::{BatchSession, ObjectInfo};
pub use interop::InteropOutcome;
pub use store::{
    CommitParams, FetchOptions, GitStore, Identity, MergeConflict, MergeStage, MergeTreeOutput,
};
