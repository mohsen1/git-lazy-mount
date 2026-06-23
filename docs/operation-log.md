# Operation log and transactions

Every semantic mutation of a workspace is sealed as an entry in an **append-only
operation log** (`glm-oplog`, `crates/oplog/src/lib.rs`; spec §13). The log is
what makes uncommitted work survive crashes and makes a stale projection
*detectable* rather than silently wrong. It is built from two kinds of immutable
records — **views** and **operations** — and a single `CURRENT` pointer.

## Records

### WorkspaceView — immutable workspace-identifying state

`view::WorkspaceView` (`crates/oplog/src/view.rs`; spec §2.4, §11) is written
once and never mutated. Each field identifies *what state the workspace is in*:

* `id` — the view's own id (content hash + nonce).
* `base_commit` — the base commit the working tree derives from.
* `workspace_head` — the workspace-private head commit (protected from GC by a
  keep-ref).
* `attached_branch` — the public branch this workspace is attached to, if any.
* `attached_branch_expected` — the *expected* current value of that branch, used
  for compare-and-swap when advancing it (spec §14).
* `mount_generation` — a monotonic generation counter (spec §2.5, §19).
* `parent_ops` — the operation(s) that produced this view (usually one; more
  after reconciling divergent operation heads).
* `path_mapping_version` — version of the path-mapping configuration.
* `filter_context_version` — version of the filter context (bumping it
  invalidates filtered caches).
* `stage_digest` — digest of the staged delta at this view (detects stage
  changes).
* `overlay_digest` — digest of the overlay entry set at this view (detects
  overlay changes).

### Operation — an append-only log entry

`record::Operation` (`crates/oplog/src/record.rs`):

* `id` — the operation's id.
* `parents` — parent operation ids (the operation DAG).
* `view` — the id of the `WorkspaceView` this operation produced.
* `timestamp_unix`, `user`, `hostname`, `pid` — best-effort provenance.
* `cause` — `Cause::Command(argv)`, `Cause::Filesystem(desc)`, or
  `Cause::Internal(desc)`.
* `description` — human-readable summary.
* `durability` — the `Durability` level the sealed user data reached (see the
  [state model](state-model.md)).
* `external_effects` — a list of `ExternalSideEffect` records (see below).

## On-disk layout

Rooted at the workspace's `journal/` directory:

```
journal/
  operations/   one <op-id>.json per operation
  views/        one <view-id>.json per view
  CURRENT       the single source of truth: which op is current
```

`CURRENT` is a small JSON object holding `current_op`, `desired_generation`, and
`applied_generation`. `OpLog::head()` reads `CURRENT.current_op`;
`current_view()` resolves head → its op → its view.

## The durability ordering (spec §13)

`OpLog::commit(view, meta)` performs exactly three steps, **in this order**:

1. **Write the view and fsync it.** Serialize the view, write it via
   `atomic_write` (temp file → `fsync` → rename → best-effort directory fsync).
2. **Write the operation and fsync it.** Build the `Operation` referencing the
   view id, write it the same way.
3. **Atomically advance `CURRENT` — last.** Only now is `CURRENT` rewritten
   (atomically) to point at the new op and record the new `desired_generation`.

Because `CURRENT` is advanced **last** and is the single source of truth, a
crash at any earlier step leaves the previously committed state fully intact; any
view/op files written before the crash are simply unreferenced orphans. See
[failure-recovery.md](failure-recovery.md) for the recovery side.

The `durability` field carried in `NewOperation` ties back to the ordered
`Durability` axis: a sealed operation records that its user data reached at least
`MetadataCommitted`, and the operation itself is `OperationSealed`.

## Desired vs applied generation (spec §2.5)

`CURRENT` carries a **pair** of generation counters:

* `desired_generation` — the generation the *committed log* wants the projection
  to be at (set from the new view's `mount_generation` on every `commit`).
* `applied_generation` — the generation the **filesystem projection** has
  actually caught up to (advanced by `OpLog::mark_applied(generation)`).

A workspace is **stale** exactly when these differ:

```rust
OpLog::is_stale()  ->  desired_generation != applied_generation
```

This is how the engine detects that the projected filesystem has not yet caught
up to the latest committed state — for example after a branch switch that bumped
the generation but before the mount re-synchronized. It turns "desired vs
applied skew" into an explicit, queryable condition instead of silent
corruption. After a successful commit, the workspace bumps the generation and
calls `mark_applied(new_gen)` so the freshly committed state is not reported as
stale.

## External side effects are saga steps, not transaction members

A local transaction (write view → write op → advance `CURRENT`) is atomic and
local. **Pushes are not part of it.** `ExternalSideEffect`
(`crates/oplog/src/record.rs`) records an external effect — a push, a
remote-branch creation — with a `kind`, a redacted `target` (never a
credentialed URL), and a saga `state` (`preflight` → `prepared` → `remote-done`
→ `acknowledged`).

These are **retryable saga steps**, deliberately outside the local atomic
transaction (spec §13). The consequence is a hard honesty boundary:

> **Op-log undo cannot undo an accepted remote push.** Once a remote has
> accepted a push, that effect lives on the remote, not in the local log;
> reverting the local operation does not unpush it.

`Workspace::push` (`crates/workspace/src/lib.rs`) implements the push step with a
`--force-with-lease` compare-and-swap against the last-known remote-tracking
value, so a *concurrent* remote update is detected rather than clobbered — but
that is concurrency safety, not undoability.

## Inspecting the log

`OpLog::log(limit)` walks the operation DAG from the head, newest first (each op
follows its first parent), up to `limit` entries. The CLI surfaces this as:

```bash
git lazy-mount op log
```

`OpLog::generations()` returns the `(desired, applied)` pair, and `is_stale()`
reports staleness, for diagnostics.

## Implementation choice: an append-only journal, not SQLite

The log is a directory of JSON files plus an atomically-swapped `CURRENT`
pointer — **not** an embedded SQL database. This is a deliberate design choice,
not an interim shortcut:

* **Simpler durability.** The crash-safety argument is a one-liner —
  *`CURRENT` advances last* — and is directly testable; there is no opaque
  WAL/B-tree to reason about.
* **No C dependency.** Avoiding SQLite keeps the crate `#![forbid(unsafe_code)]`
  and the build free of a native library, consistent with the rest of the
  workspace engine.

The records are content-addressed (`gen_id` hashes a prefix, the payload, a
nanosecond timestamp, a process-local counter, and the pid), and writes go
through the same atomic temp-file-then-rename helper used elsewhere.

## Deterministic crash injection

`glm-oplog` ships `CrashPoint`, an enum of the persistence boundaries in
`commit`, so tests can inject a deterministic crash and prove the durability
contract (spec §50):

* `AfterViewDurable` — after the view is durable, before the op is written.
* `AfterOpDurable` — after the op is durable, before advancing `CURRENT`.
* `BeforeCurrentSwap` — immediately before the atomic `CURRENT` swap.

`set_crash_point(...)` arms the next transaction; `check_crash(...)` is invoked
at each boundary and returns an error simulating a crash. Crucially this code is
**always compiled**, so the durability behavior in release is identical to what
the tests exercise. The recovery proofs built on these points are described in
[failure-recovery.md](failure-recovery.md).
