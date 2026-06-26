//! Linux FUSE mount of the transparent **read-only** projection. Compiled only with the `fuse` feature (links libfuse3; Linux-only),
//! exercised by the Linux mount CI job and via Docker.
//!
//! ## Real file handles
//!
//! `open` allocates a durable handle backed by a [`ContentHandle`] (a cache-file
//! FD, or the synthetic `.git` bytes); `read` serves range reads from that
//! handle; `release` drops it. We never return `fh = 0` and never re-resolve the
//! path to service a read.
//!
//! ## Bounded executor, not thread-per-callback
//!
//! fuser's dispatch loop is serial: it will not read the next kernel request
//! until the current `Filesystem` method returns. A callback that shells out to
//! git can be slow, and a subprocess `exec` can issue a `FLUSH` back to us that
//! only a free dispatch thread can answer — a hard deadlock if the loop blocks.
//! So every callback hands its work to a **bounded worker pool** and returns
//! immediately, keeping the loop free. Unlike a thread-per-callback design, the
//! pool caps the number of OS threads. There are two pools: a small one for the
//! fast, non-faulting metadata callbacks (`readdir`/`statfs`) and a larger one
//! for object-IO callbacks that may block on git, so an `ls` stays responsive
//! while reads hydrate blobs.

#![forbid(unsafe_code)]

#[cfg(feature = "fuse")]
mod mount;

#[cfg(feature = "fuse")]
pub use mount::{mount, spawn_mount, BackgroundMount};
