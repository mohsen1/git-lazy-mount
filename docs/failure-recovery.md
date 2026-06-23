# Failure recovery and FSCK

This document covers what happens after a crash and what `git lazy-mount fsck` /
`recover` do (spec §43). It also states plainly what is implemented and tested
versus what is designed but not yet built.

## The core invariant

The operation log (`glm-oplog`; see [operation-log.md](operation-log.md))
commits in a fixed order: **write view (fsync) → write op (fsync) → advance
`CURRENT` (last)**. Because `CURRENT` is the single source of truth and is
advanced last:

> A crash at **any** earlier boundary leaves the previous committed state fully
> intact. The only residue is **unreferenced orphan** view/op files — records
> that were written but that `CURRENT` never came to point at.

Nothing that `CURRENT` references is ever partially written, so reopening the
log after a crash always yields a consistent prior state. Orphans are harmless:
they are not reachable from `CURRENT`, so they are ignored on reopen and may be
quarantined later (see FSCK below).

## Persistence boundaries proven by crash injection

`glm-oplog::CrashPoint` enumerates the boundaries in `OpLog::commit`, and the
test suite injects a crash at each, reopens the log, and asserts that the
*previous* committed operation is still current and healthy
(`crates/oplog/src/lib.rs`, tests `crash_after_view_durable`,
`crash_after_op_durable`, `crash_before_current_swap`):

| `CrashPoint` | When | Proven outcome |
| --- | --- | --- |
| `AfterViewDurable` | view fsynced, op not yet written | prior state survives; orphan view only |
| `AfterOpDurable` | op fsynced, `CURRENT` not advanced | prior state survives; orphan view+op |
| `BeforeCurrentSwap` | immediately before the `CURRENT` swap | prior state survives; orphan view+op |

In each case the reopened log reports `recover().healthy == true`, `head()`
still points at the last good operation, and `current_view()` returns the
generation from *before* the doomed transaction. The crash-injection code is
always compiled, so release behavior matches the tested behavior.

## Overlay atomic publication

The writable overlay (`glm-overlay`, `crates/overlay/src/lib.rs`) protects dirty
content with the same discipline, ordered so that **content is durable before
anything points at it**:

1. Write the content to a temp file, **fsync**, and rename it into
   `content/<id>`.
2. *Only then* write the metadata record to a temp file, fsync, and rename it
   into `meta/<id>.json`.

`put_file` / `put_symlink` follow this order explicitly (content first, metadata
second). The result is the overlay's central crash invariant:

> A metadata record **never** points at absent or torn content, because the
> content is published (fsynced + renamed) before the metadata that references
> it.

On reopen, `Overlay::open` scans `meta/` and rebuilds the in-memory index. Two
properties make this crash-safe:

* **Torn temp files are ignored.** Only files with a `.json` extension are
  considered; in-progress temp files (created by `NamedTempFile`) are skipped.
* **Corrupt/partial records are ignored.** A metadata file that fails to
  deserialize is skipped rather than aborting the open.

`BaseRef` entries and tombstones store no content file at all, so they have
nothing to tear. Tests confirm that overlay entries — regular files, executables,
symlinks, base-refs, and non-UTF-8 paths — **survive reopen** with their content
and modes intact (spec §53.11/§53.12). Clean tracked content is never stored in
the overlay; it remains recoverable from Git objects.

## `git lazy-mount fsck` / `recover`

`OpLog::recover()` (`crates/oplog/src/lib.rs`) validates the log **without
mutating user data** and returns a structured `RecoveryReport` (spec §43 step
9):

```rust
pub struct RecoveryReport {
    pub current_op: Option<OperationId>, // the current op, if any
    pub stale: bool,                     // desired_generation != applied_generation
    pub issues: Vec<String>,             // human-readable problems found
    pub healthy: bool,                   // true iff issues is empty
}
```

`recover()` reads `CURRENT`, then checks that the current operation and its view
are readable, recording a human-readable issue for anything missing or corrupt.
A fresh log recovers clean (`healthy == true`, `current_op == None`).

Two design commitments hold here:

* **Quarantine, not delete.** Recovery's posture is to *report* problems and set
  aside suspect/orphan records, not to destroy data. `recover()` itself never
  mutates user data.
* **No network needed for locally modified data.** Recovering the workspace's
  own committed log and its dirty overlay content is entirely local; the remote
  is not contacted. (Re-fetching *clean* base blobs that were never materialized
  is a separate, on-demand concern handled by the object provider, not
  recovery.)

The CLI exposes these as `git lazy-mount fsck` and `git lazy-mount recover`.

## Implemented and tested vs. future

**Implemented and tested today:**

* Op-log crash injection at all three boundaries, with reopen proving the prior
  committed state survives (`glm-oplog` tests).
* `OpLog::recover()` producing a structured `RecoveryReport`
  (healthy / stale / issues), plus staleness detection via the desired/applied
  generation pair.
* Overlay atomic publication (content fsync+rename before metadata record),
  with torn temp files and corrupt records ignored on reopen.
* Dirty overlay state (files, symlinks, base-refs, non-UTF-8 paths) surviving
  unmount/remount.

**Designed but not yet implemented (future):**

* Full overlay/journal FSCK *repair* tooling — automated quarantine sweeps,
  orphan collection, and reconciliation beyond the read-only `RecoveryReport`.
* `recover --export` (extracting locally modified content out of a damaged
  workspace).

## Scope and honesty note

Crash recovery here concerns the **engine state**: the operation log, the
overlay, and the stage. The kernel filesystem backends (FUSE / FSKit / ProjFS)
are **not production-ready** — the callback logic exists and is tested, but the
FFI adapters that perform a real kernel mount are not built in this environment.
Nothing in this document should be read to imply that recovery has been
exercised through a live mounted filesystem; it is verified at the engine layer,
against real Git, through the test suite.
