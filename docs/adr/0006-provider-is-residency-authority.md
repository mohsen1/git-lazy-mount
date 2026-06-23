# 0006 — The provider is the object-residency authority

**Status:** Accepted

## Context

Hot reads are served from a long-lived `git cat-file --batch-command` session
([batch.rs](../../crates/git-store/src/batch.rs)) so Git is not re-spawned per
object. That session runs with `GIT_NO_LAZY_FETCH=1` so a read never silently
triggers a network fetch. But this session is **fragile**: if it is asked for a
**promisor object that is missing locally**, Git refuses the lazy fetch and the
process **fatally exits** — killing the shared session for every concurrent
reader, not just the one that asked.

(See the cross-cutting analysis in
[../feasibility/git-object-fetching.md](../feasibility/git-object-fetching.md).)

## Decision

Make [`GitObjectProvider`](../../crates/object-provider/src/lib.rs) the **single
residency authority**: it must confirm an object is locally present **before** the
batch session is ever queried for it. The provider tracks a `present` set and,
before reading via the session, calls `ensure_present_locally` — a cheap
`cat-file -e` (with `GIT_NO_LAZY_FETCH`) and, if the policy permits, a scheduled
fetch — and only then reads contents through the session. If the session ever does
die, the death is surfaced as an error and the session is respawned lazily, with a
one-shot read as fallback.

The batch session's own contract documents this: "only query objects known to be
locally present"; the provider is named as the party responsible.

## Consequences

* The fragile batch session is only ever asked for objects proven resident, so a
  routine read of a not-yet-fetched blob cannot crash the session for everyone.
* Residency checks are a deliberate, counted step (`presence_check` metric); the
  `present` set memoizes them to avoid repeated `cat-file -e` calls.
* Filesystem callbacks pass a non-fetching policy (`CacheOnly`/`MustNotFetch`), so
  a missing object becomes a clean offline error rather than an implicit fetch or
  a credential prompt (spec §3.13).
* `tree` reads use a one-shot `cat-file tree` (a process death there is just an
  error, not a shared-session kill), so they can probe directly and fetch only on
  a genuine miss.
