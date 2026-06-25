//! Built-in [`crate::provider::SearchProvider`] implementations.
//!
//! - [`sourcegraph`] — native client for the Sourcegraph streaming search API.
//! - [`exec`] — a runtime "plugin": shells out to any command, so a new backend
//!   can be added with a script and no recompile.

pub mod exec;
pub mod sourcegraph;
