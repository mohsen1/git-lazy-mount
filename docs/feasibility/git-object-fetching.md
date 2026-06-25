# Feasibility: missing-object fetching

**Question.** Can we serve filesystem reads from a partial clone while
guaranteeing that a read never triggers an *implicit* network fetch?

## Experiment

Make a `blob:none` partial clone of a local bare remote, then probe
`git cat-file --batch-command --buffer` with `GIT_NO_LAZY_FETCH=1` against three
kinds of object.

## Result (git 2.43 and 2.54)

| Query (`NO_LAZY_FETCH=1`)                | Output | Exit |
|------------------------------------------|--------|------|
| `info <present tree>`                    | `<oid> tree <size>` | 0 |
| `info <unknown oid>` (never referenced)  | `<oid> missing` | 0 |
| `info <promisor blob, missing locally>`  | `fatal: could not fetch … from promisor remote` | **128** |

A one-shot `git cat-file -e <oid>` with `NO_LAZY_FETCH=1` is graceful
(returns exit 1) for the same promisor-missing object. Only the long-lived
`--batch*` mode fatals and terminates the process.

## Decision (release gate)

* The **object provider is the residency authority**: it never queries the batch
  session for an object it has not confirmed present. Presence is checked with
  graceful `cat-file -e` and cached. Content is read via the batch session only
  for confirmed-present objects. A session death is treated as an error and the
  session respawns. (See ADR-0006 and `docs/design/object-fetching.md`.)
* `GIT_NO_LAZY_FETCH=1` is the default for read paths; only the fetch scheduler
  escalates to network. `FetchPolicy::{MustNotFetch,CacheOnly}` reads of a
  missing object return a structured `offline_missing_object` error.
* Coalescing was verified: 100 concurrent reads of one missing blob produce exactly
  one fetch (`object-provider` integration test), with the other callers
  waiting on a condvar. Distinct objects batch into one fetch invocation.
* Locks are released before any fetch/subprocess.

## Status

Implemented and tested (`crates/object-provider`, `crates/git-store/src/batch.rs`).
Not yet implemented: streaming very large blobs to a verified temp file before
publication, per-remote concurrency limits, negative caching, circuit breaking.
