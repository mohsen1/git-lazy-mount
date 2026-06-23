# 0003 — Append-only operation log with an atomic `CURRENT` pointer

**Status:** Accepted

## Context

The transactional workspace advances through immutable views, and uncommitted
work must survive crashes at any persistence boundary. An embedded database
(SQLite) was an option, but it adds a dependency and a second durability model to
reason about alongside Git's object store, and its crash behavior would still need
the same careful ordering at our own boundaries.

## Decision

Persist state as an **append-only journal** of immutable view and operation
records under the workspace directory, with a single atomic **`CURRENT`** pointer
([oplog/src/lib.rs](../../crates/oplog/src/lib.rs), spec §13). A transaction:

1. writes the new immutable **view** record and fsyncs it;
2. writes the new immutable **operation** record and fsyncs it;
3. **only then** atomically swaps `CURRENT` (write temp, fsync, rename).

`CURRENT` is the single source of truth and is advanced **last**, so a crash at
any earlier step leaves the previous committed state fully intact; records written
before the crash are simply unreferenced orphans. A `desired`/`applied` generation
pair in `CURRENT` makes a lagging filesystem projection (a *stale* workspace)
detectable rather than silently wrong.

## Consequences

* No database dependency; durability is plain files + fsync + atomic rename, the
  same primitives the overlay uses.
* Crash safety is **tested by deterministic crash injection** at every boundary
  (`CrashPoint::{AfterViewDurable, AfterOpDurable, BeforeCurrentSwap}`): after a
  simulated crash, recovery confirms `CURRENT` still points at the last good
  operation and the doomed operation never took effect.
* History is a walkable parent chain; orphaned records are harmless and can be
  garbage-collected later.
