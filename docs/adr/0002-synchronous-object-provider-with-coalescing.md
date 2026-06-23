# 0002 — Synchronous object provider with thread-based coalescing

**Status:** Accepted

## Context

The object provider (`glm-object-provider`, spec §16) faults missing Git objects
into the local store and must collapse concurrent demand: many filesystem reads of
the same missing blob should cause **one** fetch. The spec sketched an `async`
provider trait. In practice the provider's work is almost entirely shelling out to
`git` (fetch, `cat-file`), which is blocking subprocess I/O, not the kind of
high-fan-out socket work that benefits from an async runtime.

## Decision

Make [`ObjectProvider`](../../crates/object-provider/src/lib.rs) a **synchronous**
trait. Concurrency comes from calling it on multiple threads. Coalescing and
batching are done with a `Mutex` over an in-flight/present set plus a `Condvar`:
the first caller for an object marks it in-flight, drops the lock, fetches, then
notifies waiters; concurrent callers for the same object find it in-flight and
**wait on the condvar** instead of issuing their own fetch. Distinct missing
objects in one `ensure_objects` call are fetched in a single `git` invocation.
Network I/O and subprocess execution happen with **no lock held** (spec §3.19).

**Recorded deviation from the spec:** the spec presented an `async` provider
trait; this is the deliberate, recorded divergence. Rationale: the CLI mostly
shells to `git`, so an async runtime would add a `tokio` dependency and coloring
without removing the blocking subprocess underneath.

## Consequences

* No async runtime dependency in the provider or its callers; the call sites
  (filesystem callbacks, CLI) stay synchronous.
* The coalescing guarantee is verified: the "100 concurrent reads of one missing
  blob ⇒ 1 fetch" budget test passes (`fetch_invocation` metric).
* Throughput scales with threads, not with a reactor; for predominantly
  subprocess-bound work this is adequate and far simpler.
* If a future workload becomes I/O-fan-out-bound, revisiting async would be a
  contained change behind this trait.
