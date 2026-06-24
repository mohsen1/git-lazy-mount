//! Linux FUSE mount of the transparent **read-only** projection (redesign.md
//! §14–§19). Compiled only with the `fuse` feature (links libfuse3; Linux-only),
//! exercised by the Linux mount CI job and via Docker.
//!
//! ## Real file handles (§4.7, §17)
//!
//! `open` allocates a durable handle backed by a [`ContentHandle`] (a cache-file
//! FD, or the synthetic `.git` bytes); `read` serves range reads from that
//! handle; `release` drops it. We never return `fh = 0` and never re-resolve the
//! path to service a read.
//!
//! ## Bounded executor, not thread-per-callback (§4.8, §18, §44)
//!
//! fuser's dispatch loop is serial: it will not read the next kernel request
//! until the current `Filesystem` method returns. A callback that shells out to
//! git can be slow, and a subprocess `exec` can issue a `FLUSH` back to us that
//! only a free dispatch thread can answer — a hard deadlock if the loop blocks.
//! So every callback hands its work to a **bounded worker pool** and returns
//! immediately, keeping the loop free. Unlike a thread-per-callback design
//! (which §44 forbids), the pool caps OS threads. (A later refinement splits
//! fast-metadata vs. object-IO pools per §18.)

#![forbid(unsafe_code)]

#[cfg(feature = "fuse")]
mod mount;
#[cfg(feature = "fuse")]
mod pool;

#[cfg(feature = "fuse")]
pub use mount::{mount, spawn_mount, BackgroundMount};
