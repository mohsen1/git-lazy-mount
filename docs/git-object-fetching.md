# Git object fetching

All missing-object access goes through an explicit **object provider**
(`glm-object-provider`). The provider is the system's **residency authority**:
it decides what is locally present, it is the only component allowed to initiate
network retrieval, and it enforces the [`FetchPolicy`](#fetch-policy) so a
filesystem callback can never silently fetch or prompt for credentials.

## The shared store and partial clone

A repository is cloned once into a **bare** store with a partial-clone filter:

```bash
git init --bare <store>
git -C <store> remote add origin <url>
git -C <store> fetch --filter=blob:none origin
```

`blob:none` brings down commits and trees but not file blobs; trees are present
locally, blobs are fetched on demand. `glm-git-store` configures the promisor
settings (`remote.origin.promisor`, `remote.origin.partialclonefilter`) when a
filter is used. History/tree behavior is exposed explicitly
(`--history=full|tip`, `--tree-fetch=eager|lazy`); a full-object clone
(`--allow-full-object-clone`) is offered only when the remote rejects the filter
and **still never implies a full checkout**.

## Critical finding: a NO_LAZY_FETCH batch session dies on promisor misses

The hot read path uses a long-lived `git cat-file --batch-command` process for
cheap repeated object access. To guarantee a read never triggers an implicit
network fetch, that process runs with `GIT_NO_LAZY_FETCH=1`.

**Measured behavior (git 2.43 and 2.54):**

| Query (NO_LAZY_FETCH=1)                         | Result |
|-------------------------------------------------|--------|
| `info <present tree>`                           | `<oid> tree <size>` |
| `info <unknown oid Git has never seen>`         | `<oid> missing` (clean, exit 0) |
| `info <promisor blob missing locally>`          | **`fatal: could not fetch … from promisor remote`, exit 128** |

So a batch session, when asked for a *promisor* object that is missing locally,
**fatally terminates** instead of reporting "missing". (A one-shot
`git cat-file -e <oid>` with `NO_LAZY_FETCH=1` is graceful and returns exit 1 —
only the long-lived `--batch*` mode fatals.)

**Consequence — the design rule:** the provider must **never** query the batch
session for an object it has not confirmed is local. It therefore:

1. tracks a present-set (objects from the partial clone's trees are present;
   blobs become present only after an explicit fetch);
2. checks residency cheaply with `cat-file -e` (graceful) before a first read,
   caching the result;
3. reads content via the batch session only for confirmed-present objects;
4. treats a session death as an error and respawns (this should never happen in
   correct operation).

This is the *safe* failure mode for filesystem callbacks (spec §3.13): a stray
request fails loudly rather than silently escalating to a network fetch.

## Fetch policy

`FetchPolicy` (in `glm-core`) gates network access:

| Policy          | May fetch | Used by |
|-----------------|-----------|---------|
| `MustNotFetch`  | no        | strict fs-callback paths (asserts no I/O) |
| `CacheOnly`     | no        | reads that must stay offline (`GIT_NO_LAZY_FETCH=1`) |
| `AllowNetwork`  | yes       | interactive on-demand reads / CLI |
| `Prefetch`      | yes       | background / `git lazy-mount prefetch` |

A `CacheOnly`/`MustNotFetch` read of a missing object returns a structured
`offline_missing_object` error — it never fetches.

## Coalescing and batching

`ensure_objects` coalesces concurrent requests for the same object and batches
distinct objects into one fetch:

* 100 concurrent reads of one missing blob trigger **exactly one** underlying
  fetch — proven by `object-provider`'s integration test (the other 99 callers
  wait on a condvar and are counted as `coalesced_waits`).
* distinct missing objects requested together are fetched in a single
  invocation.

Locks are **never held across the fetch / subprocess** (spec §3.19): the
provider claims in-flight slots under a mutex, releases the lock, performs the
network I/O, then re-acquires the lock to publish results and notify waiters.

## Metrics

Every read/fetch updates counters (`tree_reads`, `blob_reads`, `filtered_reads`,
`bytes_read`, `presence_checks`, `fetch_invocations`, `objects_fetched`,
`coalesced_waits`) surfaced via `git lazy-mount stats --json`. These back the
hydration-budget assertions in the test suite (spec §50): a test that passes
functionally but unexpectedly fetches the whole repo is a failure.

## Not yet implemented

Streaming very large blobs to a verified temp file before publishing (the
current provider returns `Vec<u8>`), per-remote concurrency limits, negative
caching, network circuit breaking, and prefetch-for-offline pinning are designed
(see the trait and §16) but not all implemented; see
[limitations.md](limitations.md).
