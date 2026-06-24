# Object provider, fetch scheduler, filters + LFS, metadata/size

Authoritative spec: [`redesign.md`](../../redesign.md) §20 (object provider),
§21/§22 (metadata & size), §23 (filters/attributes), §24 (LFS). Supporting:
§18/§19 (bounded I/O, deadlock), §34 (alternates), §35 (auth/offline), §38
(hydration budgets). This doc specifies the **daemon-internal** object/content
layer. It owns none of Git's repository state (§7) — it only turns object IDs +
filter context into bounded, streamable working-tree bytes and correct sizes.

This is a *redesign*. The streaming-provider shape and the
residency-authority + coalescing core (`crates/object-provider`,
`crates/git-store/src/batch.rs`) are **reusable substrate**. What changes:
`raw_blob`/`filtered_blob` returning `Vec<u8>` (§4.6, §20) become `ReadSeek` /
`ContentHandle`; coalescing-by-condvar grows into a real **scheduler** with
priorities/limits/retries/circuit-breaker; the five caches (§20.2) are
separated and given atomic validated publication; filter context grows to the
full §23 key. Superseded crates (`stage`, custom `workspace` branch/commit, the
`git lazy-mount git --` bridge) do not touch this layer.

---

## 1. Position in the stack

```
FUSE/FSKit callback (getattr/read/open)         §16,§40   — passes FetchPolicy::MustNotFetch
  └─ Worktree projection (baseline+overlay)     §8        — resolves path → (oid, FilterContext)
       └─ ObjectProvider  ── this doc, §20 ──────────────┐
            ├─ FetchScheduler   (network, the ONLY fetcher)   §20.1
            ├─ CacheSet         (odb/tree/filtered/meta/lfs)  §20.2
            ├─ FilterEngine     (git plumbing + trust)        §23
            └─ LfsEngine        (smudge/pointer/error)        §24
                 └─ GitStore / BatchSession  (cat-file)       crates/git-store
```

**Invariant (§19).** A FUSE callback enters this layer with
`FetchPolicy::MustNotFetch`. Only the `FetchScheduler` may cause network I/O,
on its own threads, with **no provider/inode/namespace/index lock held** (§18).
Every git subprocess is `GIT_NO_LAZY_FETCH=1` + CLOEXEC-hardened
(`git-store/src/proc.rs::harden_fds`, `batch.rs`) except the scheduler's own
fetch invocation.

---

## 2. The streaming `ObjectProvider` trait (§20)

No method returns `Vec<u8>` for blob/working-tree content (§4.6, §20, §44 "read
allocates the complete blob"). Identity is `ObjectId` (format-agnostic,
`core/src/object_id.rs`) and `RepoPath` (raw bytes, `core/src/path.rs`) — never
lossy UTF-8 (§20, §31).

```rust
pub trait ObjectProvider: Send + Sync {
    /// Parsed tree. Trees are present under blob:none, so the common path
    /// fetches nothing; a genuine miss may fetch when policy allows.
    fn tree(&self, oid: &ObjectId, policy: FetchPolicy) -> Result<Arc<TreeObject>>;

    /// Type + RAW object size, no content materialization. For a clean blob
    /// this is the cat-file size; it is NOT the projected working-tree size
    /// (see §6). Cheap: one `info` on the batch session for a present object.
    fn object_info(&self, oid: &ObjectId, policy: FetchPolicy) -> Result<ObjectInfo>;

    /// Seekable reader over the RAW (unfiltered) blob bytes. Backed by an
    /// on-disk handle; never an in-memory Vec. Used for raw mode, pointer
    /// inspection, and as the filter pipeline's source.
    fn open_raw_blob(&self, oid: &ObjectId, policy: FetchPolicy)
        -> Result<Box<dyn ReadSeek + Send>>;

    /// Seekable reader over the PROJECTED working-tree bytes (filters + LFS
    /// applied per `context`) a normal checkout would write (§23). Served from
    /// the filtered-content cache file; range reads hit the fd (§17.1).
    fn open_worktree_file(
        &self,
        oid: &ObjectId,
        path: &RepoPath,
        context: &FilterContext,
        policy: FetchPolicy,
    ) -> Result<ContentHandle>;

    /// Ensure objects are present locally, coalescing/batching/prioritizing.
    /// The sole fetch entry point used by prefetch and by metadata/content
    /// paths that escalate from MustNotFetch.
    fn ensure_objects(&self, oids: &[ObjectId], priority: FetchPriority)
        -> Result<EnsureResult>;

    fn is_present(&self, oid: &ObjectId) -> bool;
    fn metrics(&self) -> MetricsSnapshot;
}
```

### Supporting types

```rust
/// Seek + buffered read; the unit of all content I/O.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A resolved working-tree representation, opened against a published,
/// validated cache file (or an overlay/native file — see fast paths §6.3).
pub struct ContentHandle {
    pub reader: Box<dyn ReadSeek + Send>,
    pub size: u64,                 // EXACT projected size — never synthetic (§21)
    pub source: ContentSource,     // for metrics + getattr fast-path classification
    pub size_source: SizeSource,   // Local | RawObject | FilteredHydration | Manifest
}

pub enum ContentSource { OverlayFile, FilteredCache, RawPresentBlob, Lfs, Symlink }

/// `object_info` reply (already in git-store/src/batch.rs).
pub struct ObjectInfo { pub kind: ObjectKind, pub size: u64 }   // size = RAW

pub struct EnsureResult {            // already in object-provider/src/lib.rs
    pub fetched: usize,
    pub already_present: usize,
    pub coalesced: usize,
}
```

`FetchPolicy` / `FetchPriority` keep the existing shapes
(`core/src/fetch.rs`): `MustNotFetch ⊂ CacheOnly` for fs callbacks,
`AllowNetwork`/`Prefetch` for the scheduler; `Interactive > Prefetch >
Background`. `MustNotFetch.may_fetch() == false` is load-bearing — a passive
read that misses returns `offline_missing_object` rather than escalating.

### Trait invariants (regression tests)

- **T1 — no full-blob allocation.** `open_raw_blob`/`open_worktree_file` peak
  RSS is bounded by a fixed buffer, independent of blob size (§4.6, §38.8). A
  multi-GiB blob read in 64 KiB ranges allocates O(1).
- **T2 — `object_info` never materializes.** Calling it on a present blob runs
  one `info` (no `contents`), 0 filter runs, 0 fetches.
- **T3 — identity is bytes.** A `RepoPath` with invalid UTF-8 round-trips
  through `open_worktree_file` and reaches git plumbing without lossy
  conversion (§20, §31, §23 "no lossy UTF-8").
- **T4 — `tree` is fetch-free under blob:none** for trees already present;
  parsed trees come from the parsed-tree cache after first parse.

---

## 3. Fetch scheduler (§20.1)

Today's coalescing lives inline in `GitObjectProvider::ensure_objects`
(`object-provider/src/lib.rs`): an in-flight `HashSet` + `Condvar`, fetch with
no lock held. The redesign extracts a dedicated `FetchScheduler` owning the
network budget. ADR-0002 (synchronous, thread-based) and ADR-0006 (residency
authority) still hold; this adds the missing pieces the feasibility doc lists as
TODO (`docs/feasibility/git-object-fetching.md`: per-remote limits, retries,
circuit breaker, streaming-to-temp).

```rust
pub struct FetchScheduler {
    origins: HashMap<OriginId, OriginState>,   // per-remote concurrency + breaker + auth
    inflight: Mutex<HashMap<ObjectId, Arc<FetchSlot>>>,  // coalescing map
    waiters: Condvar,                          // OR a per-slot completion gate
    rate: TokenBucket,                         // global bandwidth limit
    cancel: CancellationRegistry,              // by request id / requesting pid
    cfg: SchedulerConfig,
    metrics: Metrics,
}

struct FetchSlot {
    oid: ObjectId,
    state: Mutex<SlotState>,                    // Queued | InFlight | Done | Failed(Error)
    done: Condvar,
    waiters: AtomicUsize,
    priority: FetchPriority,                    // max-priority of joined requests
}
```

### 3.1 Coalescing + batching

- **Coalescing.** A request for an `oid` already in `inflight` joins its
  `FetchSlot` (increment `waiters`, bump `priority` to the max) and blocks on
  `slot.done`. *Exactly one* network retrieval per oid.
  - **Invariant S1 (§20.1, §38.6):** 100 concurrent `ensure_objects([X])` for
    one missing `X` ⇒ **1** `fetch_invocation`, 99 `coalesced_waits`. Already
    covered by `object-provider/tests/provider_integration.rs`; keep it.
- **Batching window.** Distinct missing oids arriving within
  `cfg.batch_window` (default 5 ms, capped at `cfg.max_batch` oids) drain into
  one `git fetch <oids…>` invocation per origin.
  - **Invariant S2 (§20.1):** N distinct missing oids requested together ⇒ 1
    invocation (existing test: distinct objects batch).
- A waiter is released the instant *its* oid resolves, even if the batch carries
  others (per-slot `done`, not a single global condvar) — so an interactive read
  never waits on an unrelated slow object in the same batch.

### 3.2 Per-origin concurrency, bandwidth, priority

- `OriginState.semaphore` caps concurrent fetch invocations per remote
  (`cfg.per_origin_concurrency`, default 4). Distinct from the local/decompress
  pools (§18): network has its own budget.
- `TokenBucket` enforces `cfg.max_bytes_per_sec` globally; the fetch worker
  acquires tokens before reading the wire.
- A ready queue is ordered by `FetchPriority` then arrival (FIFO within a
  priority). `Background` prefetch yields to `Interactive` and may be dropped
  under pressure (§18, §4.8). Priority only orders *queued* work; an in-flight
  fetch is never preempted (its waiters would lose their result).

### 3.3 Cancellation

- Every request carries a `RequestId` and optional requesting `pid`. The kernel
  cancelling a FUSE op, or the requesting process exiting, fires
  `cancel.cancel(request_id)` (§18 "support cancellation").
- Cancellation removes a *queued* slot; an *in-flight* fetch keeps running (so
  another waiter / the cache still benefits) but the cancelled caller returns
  promptly with a cancelled error. The last waiter leaving an in-flight
  background-only slot may abort it.

### 3.4 Retries, auth, offline, circuit breaker — state machine

Per-origin state. Transitions on a fetch invocation's classified result
(`git-store/src/proc.rs::classify` already maps stderr →
`Authentication` / `OfflineMissingObject` / `RemoteMissingObject` / …).

| State        | Event                                   | Action                                                              | Next         |
|--------------|-----------------------------------------|---------------------------------------------------------------------|--------------|
| `Closed`     | fetch ok                                | mark present; wake waiters with success                             | `Closed`     |
| `Closed`     | transient (offline/timeout/5xx)         | retry w/ exp. backoff+jitter, ≤ `cfg.max_retries` (default 3)        | `Closed`     |
| `Closed`     | retries exhausted                       | record `last_failure`; **fail all joined waiters with it**          | `HalfOpen?`  |
| `Closed`     | ≥ `cfg.breaker_threshold` consec. fails | open breaker                                                        | `Open`       |
| `Closed`     | auth failure                            | enter `AuthBlocked`; fail waiters with the auth error               | `AuthBlocked`|
| `Closed`     | `remote_missing_object` (hard)          | **do not retry**; fail waiters; object truly absent                 | `Closed`     |
| `Open`       | any fetch request                       | fail fast with `last_failure` (no network) until `cooldown` elapsed | `Open`       |
| `Open`       | cooldown elapsed                        | allow one trial                                                     | `HalfOpen`   |
| `HalfOpen`   | trial ok                                | close breaker                                                       | `Closed`     |
| `HalfOpen`   | trial fails                             | re-open, extend cooldown                                            | `Open`       |
| `AuthBlocked`| `git lazy-mount doctor` / successful refresh | clear; allow retries                                           | `Closed`     |
| `AuthBlocked`| passive fetch request                   | fail fast with the recorded auth error (no prompt — §35, §10.1)     | `AuthBlocked`|

- **Offline (`--offline`, §35):** the scheduler is constructed in an offline
  mode where every origin starts `Open` with infinite cooldown; only cache reads
  succeed; a miss returns `offline_missing_object`. `prefetch --for-offline`
  temporarily lifts this for an explicit user op.
- **Never prompt from a callback (§35, §10.1).** Auth interaction happens only
  during the initial mount or an explicit `git fetch`/`doctor`; the scheduler
  records `AuthBlocked` and surfaces a daemon diagnostic.

### 3.5 The original-failure invariant (§20.1)

> Waiting callers must receive the **original** fetch failure, not a later
> generic "missing object" error.

The `FetchSlot` stores the *classified `Error`* from the failing fetch.
Every joined waiter returns a **clone of that error** (preserving code,
`recommended_action`, redacted context — `core/src/error.rs`). The current code
violates this: after a failed fetch it re-checks presence and returns a fresh
`RemoteMissingObject` (`object-provider/src/lib.rs::ensure_present_locally`).

- **Invariant S3 (§20.1):** inject an auth-failing fetch with 50 concurrent
  waiters ⇒ all 50 receive `ErrorCode::Authentication` with the original
  action string, **not** `remote_missing_object`. (New regression test;
  `core::Error` needs a `clone()` for the propagated error, or store
  `Arc<Error>` in the slot.)

### 3.6 Scheduler invariants

- **S4 — no lock across network (§18, §19).** A debug assertion / lock-order
  lint: the fetch invocation runs with `inflight`/`origins`/provider locks
  released (already the rule in `ensure_objects`).
- **S5 — breaker fails fast.** With an `Open` origin, an `AllowNetwork` request
  returns in < 1 ms without spawning git.
- **S6 — cancellation is prompt.** A cancelled queued request returns within the
  batch window; it does not wait for the origin semaphore.
- **S7 — bounded retries.** A permanently-offline origin makes ≤ `max_retries`
  invocations per request then fails; no unbounded loop.

---

## 4. Cache separation + atomic validated publication (§20.2)

Five caches, distinct keyspaces and lifetimes (§20.2). Per-workspace under
`workspaces/<id>/` (architecture.md on-disk layout): `git/` (odb),
`filtered-cache/`, plus tree/meta/lfs subdirs. **Never** store filtered bytes as
a git blob unless git's *clean* filter produced it (§20.2, §44).

| Cache              | Key                                                              | Backing                        | Producer / source                       |
|--------------------|-----------------------------------------------------------------|--------------------------------|-----------------------------------------|
| **odb**            | `ObjectId`                                                       | git's own object store         | clone/fetch via `GitStore`              |
| **parsed-tree**    | `(object-format, tree oid, PARSER_VERSION)`                     | `metadata::TreeCache` (+disk)  | `tree()` first parse                    |
| **filtered**       | `FilterContext::cache_key()` (§5, sha256)                       | validated file in `filtered-cache/` | `open_worktree_file` (filters/LFS) |
| **metadata**       | `ObjectId` → `{raw_size, kind}`; opt. manifest (§21)            | small kv / sqlite              | `object_info`; optional size manifest   |
| **LFS**            | LFS pointer `oid`+size                                          | file in `lfs/` (or git-lfs's)  | `LfsEngine` smudge                      |

The existing `TreeCache` (`metadata/src/lib.rs`) already keys by
`(format, oid, PARSER_VERSION)` with negative caching and atomic
tempfile→`persist` writes; reuse as-is for parsed-tree.

### 4.1 Atomic validated publication (§20.2)

Every cache *file* (filtered, lfs, tree-on-disk, future size manifest) is
published with this exact protocol (§20.2: temp path → validate → fsync →
atomic publish → immune to partial reuse):

```rust
fn publish(dir: &Path, key: &str, write: impl FnOnce(&mut File) -> Result<Validation>)
    -> Result<PathBuf>
{
    let mut tmp = NamedTempFile::new_in(dir)?;        // 1. unique temp in SAME dir
    let v = write(tmp.as_file_mut())?;                // 2. stream content (bounded)
    v.verify()?;                                      // 3. validate (size + digest)
    tmp.as_file().sync_all()?;                        // 4. fsync content
    let final_path = dir.join(key);
    tmp.persist(&final_path)?;                        // 5. atomic rename (publish)
    sync_dir(dir)?;                                   // 6. fsync dir (durable name)
    Ok(final_path)
}
```

- **Validation** (`Validation`): the bytes written hash to the expected content
  digest **and** length matches the recorded `size`. For the filtered cache the
  digest is over the *produced* bytes and is stored in a sidecar / xattr so a
  reader can re-verify; a file whose digest mismatches is treated as absent and
  rebuilt (security §36 "cache poisoning", "partially written reuse").
- A reader **only** opens the final published path; a crash mid-write leaves a
  temp file that recovery (§32.2) reconciles/sweeps — never the final name.
- `metadata::TreeCache::put` already follows tempfile→fsync→persist; extend it
  with the dir-fsync + digest step for the format unifier.

### 4.2 Cache invariants (regression tests)

- **C1 — no torn reads.** Kill the process between steps 2 and 5; on restart the
  key is absent (not half-written) and rebuilds cleanly (§40.5 crash injection).
- **C2 — filtered bytes are never a git blob.** A filtered-cache entry is a
  plain file under `filtered-cache/`, addressed by `cache_key()`, with no
  corresponding `hash-object -w` (§20.2, §44).
- **C3 — digest gate.** Corrupting a published filtered file flips it to
  "absent" on next open and triggers rebuild, never serving poison (§36).
- **C4 — key isolation.** Two paths with different `.gitattributes` but the same
  raw blob get **different** filtered keys (§5, §23).

---

## 5. Git filters & attributes (§23)

Byte-level filtering stays git's job (`GitStore::smudge_blob` →
`cat-file --filters --path=<p> --attr-source=<src>`, ADR-0007); this layer
decides **whether** to run external code, composes the **cache key**, and avoids
**index-lock recursion**. Supported conversions (§23): `text`, `eol`,
`working-tree-encoding`, `ident`, `filter` (clean/smudge drivers), `binary`,
Git LFS.

### 5.1 Filtered-cache key composition (§23)

`filters::FilterContext::cache_key()` already exists and hashes blob+path+
attr_source+config_digest+filter_identity+eol_mode+format_version. The §23 key
must include **at least** these; map 1:1 and close the gaps:

| §23 required input                  | `FilterContext` field        | Notes / gap to close                                      |
|-------------------------------------|------------------------------|-----------------------------------------------------------|
| raw blob object ID                  | `raw_blob` (+format)         | present                                                   |
| repository path **bytes**           | `path.as_bytes()`            | present; raw bytes, NUL-separated in hash (§31)           |
| baseline/attribute-source identity  | `attr_source`                | the base-commit/tree id (ADR-0007); present               |
| relevant `.gitattributes` state     | *(via `attr_source`)*        | **gap:** must also fold a digest of the *effective* attr  |
|                                     |                              | stack for `path` so an overlay-modified `.gitattributes`  |
|                                     |                              | (not yet committed) invalidates — add `attr_digest`       |
| relevant Git config digest          | `config_digest`              | present; covers autocrlf/eol/encoding/filter.* config     |
| filter implementation identity      | `filter_identity`            | present; e.g. `lfs`, or `clean=<cmd>` version             |
| platform EOL mode                   | `eol_mode`                   | present (`native`/`crlf`/`lf`) — explains the Win/Linux   |
|                                     |                              | size delta in `docs/feasibility/file-metadata.md`         |
| cache format version                | `format_version`            | present                                                   |

- **Rename across attribute boundaries (§23, §29).** A clean rename changes
  `path`, hence `cache_key()`, so the new path's filtered result is recomputed
  and the old key's entry is no longer referenced — old result effectively
  invalidated. The overlay rename mapping does *not* fetch descendant blobs
  (§29); only a *read* of the renamed path resolves a (possibly different)
  filtered key.
- **`.gitattributes` change invalidates descendants (§23).** Two mechanisms:
  (a) committing/checkout advances `attr_source`, changing every descendant key;
  (b) an overlay-local edit to `.gitattributes` is folded into `attr_digest` for
  paths under that directory. Either way descendant keys move.

### 5.2 External-filter trust policy (§23.2)

`filters::{FilterMode, decide, TrustStore}` implement this; wire it to §23.2's
four-mode vocabulary:

| §23.2 policy                 | `FilterMode`        | Behavior                                                            |
|------------------------------|---------------------|--------------------------------------------------------------------|
| `trusted`                    | `Faithful` + trust  | run external clean/smudge drivers; matches a real checkout         |
| `builtins-only`              | `DenyExternal`      | run git built-ins (eol/encoding/ident); **refuse** external driver |
| `error-on-external`          | `DenyExternal`      | same, but the refusal is surfaced as an actionable error (default) |
| `raw` (non-checkout-compat.) | `Raw`               | serve raw blob; explicitly **does not** match a checkout (§23.2)   |

- **Passive hydration never runs untrusted code (§23.2, §36).** At mount,
  `detect_external_filter_required()` scans `.gitattributes` for `filter=`
  drivers; if present and the repo is untrusted, projected reads of those paths
  return `FilterFailure` with the grant-trust action (`filters::refusal_error`)
  rather than executing the command. Trust is per-repo and persisted
  (`TrustStore`, keyed by `RepoId`).
- **Resource limits (§23.2, §36).** External filters run under the dedicated
  filter pool (§18) with a wall-clock timeout, output-size cap (anti
  decompression/expansion bomb), and memory cap; a breach is `FilterFailure`
  (and, for LFS, `LfsFailure`). The filter never inherits the mount fd
  (`harden_fds`).

### 5.3 Index-lock recursion avoidance (§23.1)

A passive read can occur while the user's git holds `index.lock`. Attribute
resolution + smudge in that read **must not** lock or rewrite the index (§23.1,
§19). Rules:

- Resolve attributes from the **bare gitdir** via `--attr-source=<commit>`
  (ADR-0007), reading `<commit>:<dir>/.gitattributes` tree objects — a read-only
  object path that never touches the index or worktree.
- `GitStore` runs with `GIT_OPTIONAL_LOCKS=0` (already set in `git()`), so
  inspection subprocesses never *take* the index lock.
- The smudge invocation is `cat-file --filters` (object-level), **not** `git
  checkout`/`checkout-index`/`add` (which lock the index). It reads a blob and
  applies filters; no index mutation.
- `ensure_attributes_present` (`object-provider/src/lib.rs`) pre-faults the
  `.gitattributes` blobs along the path with the *scheduler* (coalescing,
  `GIT_NO_LAZY_FETCH`) so the smudge itself runs cache-only and never spawns a
  recursive lazy-fetch (the deadlock §19/§3.13 warns about).

### 5.4 Filter invariants (regression tests)

- **F1 — checkout parity.** Projected bytes for a path equal `git checkout`'s
  bytes under the same config, for CRLF, `working-tree-encoding`, `ident`, and a
  clean/smudge driver (differential test, §40.1, §40.8).
- **F2 — untrusted external refused, not executed.** A repo with `filter=evil`
  attribute, untrusted ⇒ read returns `FilterFailure`; the command never runs
  (§23.2, §36) — assert via a sentinel side-effect file the filter would create.
- **F3 — no index lock taken.** Hold `index.lock` externally, then read a
  filtered file ⇒ succeeds; the read takes no index lock (§23.1).
- **F4 — non-UTF-8 path filters.** A non-UTF-8 path with a `.gitattributes`
  rule still resolves attributes (no "stop at first non-UTF-8 component", §31);
  if `cat-file --path` cannot accept the bytes, fall back to raw with a recorded
  reason rather than silently wrong bytes.
- **F5 — `.gitattributes` edit invalidates.** Editing an overlay
  `.gitattributes` changes the filtered result of affected descendants on next
  read (§23).

---

## 6. Metadata & size (§21, §22)

A tree entry has **no size**; the size a program sees is the *filtered
working-tree* size, which differs under CRLF / encoding / ident / smudge / LFS /
path-attrs (§21; measured in `docs/feasibility/file-metadata.md` — same blob
projects 6 bytes on Linux, 7 on Windows). Therefore:

### 6.1 The three rules (§21)

- **`readdir` never requires exact size (§4.5, §21, §38.2).** It returns names +
  inode + d_type only (`fs-fuse/src/adapter.rs::readdir` already does;
  `TreeObject` cost is O(direct entries)). **0 child blobs, 0 smudge filters.**
- **`getattr` must return the correct size (§21).** It may cause
  metadata-triggered hydration when the size is otherwise unknowable.
- **Never fake a size (§21, §44).** No zero, no raw-size-as-projected
  approximation. `metadata::MetadataMode::Exact` is the default and
  `workspace.file_size()` enforces "no fake size" today; keep that contract.

### 6.2 `getattr` size resolution — decision table

Resolve in this order; **stop at the first that yields an exact size** (records
`SizeSource` for the hydration ledger §37):

| # | Condition                                                | Size source                | Fetch? | `SizeSource`         |
|---|----------------------------------------------------------|----------------------------|--------|----------------------|
| 1 | overlay native file (locally written/materialized)       | `fstat` the overlay file   | no     | `Local`              |
| 2 | published filtered-cache entry exists for this key       | `fstat` the cache file     | no     | `FilteredCache`→`Local` |
| 3 | symlink                                                   | length of target blob bytes| maybe* | `RawObject`/hydrate  |
| 4 | clean blob, **no transform** applies (binary/no-filter)  | `object_info` raw size     | maybe† | `RawObject`          |
| 5 | size manifest present + validated (optional, §21)        | manifest entry             | no     | `Manifest`           |
| 6 | any transform applies (crlf/encoding/ident/smudge/LFS)   | materialize → filtered len | yes    | `FilteredHydration`  |

\* A symlink's projected size is its target-byte length = blob content length;
needs the (tiny) blob. † Raw size needs the object locally; under blob:none a
never-read blob may be absent — escalating here is the *getattr-may-hydrate*
allowance (§21). When `getattr` arrives with `MustNotFetch` and the size is
unknown, return the structured `offline_missing_object`/EIO rather than a fake
size (§35) — the fast paths 1/2/5 cover the common warm cases.

"No transform applies" (row 4) is decided by checking the effective attributes
for `path` (text/eol/encoding/ident/filter unset and not LFS); this is an
attribute lookup (object-level, no index — §5.3), not a content read.

### 6.3 Fast paths (§21) and the `open` path

- `open_worktree_file` returns a `ContentHandle` whose `size` is exact; `getattr`
  after open is `fstat` (row 1/2). First writable open / `O_TRUNC` seeds an empty
  overlay file and **does not fetch the baseline blob** (§17.2, §38.7) — size
  becomes `Local`.
- `ls` vs `ls -l` hydration differs and that is documented (§21): `ls` → readdir
  (rows none, 0 fetch); `ls -l` → getattr per entry (may hit row 6). Both report
  hydrations distinctly in metrics (§37, §38.3).

### 6.4 Metadata invariants (regression tests)

- **M1 — readdir is fetch-free (§38.2).** `ls` of a 100k-file dir ⇒ 0 blob
  fetches, 0 filtered reads, O(direct children) tree work (Experiment B).
- **M2 — getattr exact + classified.** `ls -l` of a CRLF text file reports the
  *filtered* size and records a `FilteredHydration`; a binary no-filter file
  reports raw size with **no** filter run (rows 4 vs 6).
- **M3 — never fake.** No code path returns size 0 / raw-as-filtered for a file
  needing a transform; `file_size()` cannot be satisfied by a guess.
- **M4 — overlay/cache fast paths fetch nothing.** getattr on a materialized or
  cached file performs only `fstat` (rows 1/2), 0 network.
- **M5 — synthetic metadata stability (§22).** Repeated getattr within a
  projection generation returns identical inode/mode/mtime; a synthetic-time
  mismatch never marks the file dirty (racy-clean care, §22).

---

## 7. Git LFS (§24)

Three explicit modes (§24); LFS content is cached **separately** (§20.2, §24)
and reported as a distinct hydration class (§24, §37).

| Mode      | Behavior                                                                                       |
|-----------|------------------------------------------------------------------------------------------------|
| `smudge`  | use installed `git-lfs`; fetch real content on first access; cache in `lfs/`; no callback auth prompt |
| `pointer` | expose the raw pointer blob (the ≈130-byte `version … oid sha256:… size …` text) — `open_raw_blob` |
| `error`   | return an actionable `LfsFailure` (`git lazy-mount …` action)                                  |

- **Detection.** A path is LFS when its `filter` attribute is `lfs` and the raw
  blob is a valid LFS pointer. Pointer parse is cheap (read the small pointer
  blob via `open_raw_blob`).
- **smudge fetch (§24, §35).** LFS object download goes through the
  `FetchScheduler`/LFS engine, **noninteractive** in a callback (no credential
  prompt; auth handled at mount or explicit op). A missing LFS object offline ⇒
  `offline_missing_object`/`LfsFailure`, never a hang.
- **Cache key.** Filtered/LFS key includes `filter_identity = "lfs"` and the
  pointer oid (§5.1) so a pointer change invalidates the materialized content.
- **Plain git LFS untouched (§24).** `git add`/`commit`/`push` continue to use
  the user's normal git-lfs via stock git; this layer only serves *reads*. LFS
  **locking** is not claimed unless tested (§24).

### LFS invariants

- **L1 — pointer mode is fetch-free.** `pointer` mode read returns the pointer
  bytes with 0 LFS network.
- **L2 — smudge caches + classifies.** First `smudge` read fetches once, caches
  in `lfs/`, records an LFS hydration; second read is cache-only.
- **L3 — error mode is actionable.** `error` mode read returns `LfsFailure` with
  a recommended action and never hangs.

---

## 8. Summary of testable invariants (→ regression suite)

Trait: **T1–T4**. Scheduler: **S1–S7** (S1 coalescing, S2 batching, S3
original-failure, S4 no-lock-across-net, S5 breaker, S6 cancellation, S7 bounded
retries). Cache: **C1–C4** (crash-atomic, never-a-blob, digest gate, key
isolation). Filters: **F1–F5** (checkout parity, untrusted-refused, no
index-lock, non-UTF-8, attr-invalidate). Metadata: **M1–M5** (fetch-free
readdir, exact+classified getattr, never-fake, fast paths, synthetic stability).
LFS: **L1–L3**. These back the §38 hydration budgets and the
`requirements-checklist.md` items 5, 6, 24, 25 + budget rows.

## 9. Reuse / change ledger

| Component                          | Status   | Action                                                              |
|------------------------------------|----------|--------------------------------------------------------------------|
| `object-provider` coalescing core  | reuse    | extract `FetchScheduler`; add priorities/limits/breaker/retries    |
| `ensure_objects` original-failure  | **fix**  | store `Arc<Error>` in the slot; propagate to all waiters (S3)      |
| `raw_blob`/`filtered_blob` → Vec    | **change**| replace with `open_raw_blob`/`open_worktree_file` → ReadSeek/Handle |
| `git-store::{batch,store}`         | reuse    | `BatchSession`, `smudge_blob`, `--attr-source`, `harden_fds`        |
| `metadata::TreeCache`              | reuse    | parsed-tree cache; add dir-fsync+digest to the publish helper       |
| `metadata::{MetadataMode,SizeSource}`| reuse  | drive the §6.2 size table                                          |
| `filters::{FilterContext,decide,TrustStore}`| reuse | add `attr_digest` to the key; map to §23.2 4-mode policy       |
| LFS engine                          | **new**  | `smudge`/`pointer`/`error` over the scheduler + `lfs/` cache        |
| Superseded: `stage`, custom `workspace`, `git lazy-mount git --` | drop | not part of this layer (§4, §7)              |
