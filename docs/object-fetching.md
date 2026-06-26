# Object fetching: materialization, single-flight, streaming, size

How the mount turns a baseline object ID into bounded, streamable working-tree
bytes and a correct file size — without ever cloning the whole repo. This is
the home for: blob materialization, per-oid single-flight coalescing,
`ContentHandle` streaming, the `smudge_blob` filter primitive, and exact-size
metadata. It owns none of Git's repository state; see
[git-state-model.md](git-state-model.md).

This doc is an *explanation* of the fetch/materialize substrate, all in
`crates/worktree` and `crates/git-store`:

- Per-oid single-flight materialization into a content-addressed cache
  (`worktree::Projection::materialize_path`).
- Bounded streaming reads via `worktree::ContentHandle` (`pread`, never a
  whole-file `Vec<u8>`).
- The git filter primitive `git-store::GitStore::smudge_blob`
  (`cat-file --filters --path --attr-source`).
- A long-lived metadata/contents session `git-store::BatchSession` returning
  `ObjectInfo { kind, size }`, and the one-shot `GitStore::object_size`.
- CLOEXEC-hardened subprocesses with lazy-fetch and lock policy
  (`git-store::proc::harden_fds`, `GIT_NO_LAZY_FETCH`, `GIT_OPTIONAL_LOCKS`,
  `GIT_TERMINAL_PROMPT=0`).
- A hydration counter that backs the laziness budgets
  (`Projection.hydrations`).

---

## 1. Position in the stack

```
FUSE callback (getattr / read / open)        crates/fuse  (TransparentFs)
  └─ Projection (baseline + overlay)          crates/worktree
       ├─ materialize_path  (single-flight → content-addressed cache file)
       ├─ ContentHandle     (pread, bounded RSS)
       └─ GitStore                            crates/git-store
            ├─ blob_to_file   (cat-file blob → cache file, streamed)
            ├─ object_size    (cat-file -s, exact raw size)
            ├─ smudge_blob    (cat-file --filters --path --attr-source)
            └─ BatchSession   (cat-file --batch-command, info/contents)
```

**Network policy.** Fetching today is git's own lazy fetch, triggered when a
materialization or size lookup is allowed to fault a missing object in. The
projection passes `allow_fetch` explicitly (`metadata_fetch` for size,
`true` for content materialization); read-only/offline paths pass `false`.
`crates/core/src/fetch.rs` defines the `FetchPolicy` vocabulary
(`MustNotFetch`/`CacheOnly`/`AllowNetwork`/`Prefetch`, `may_fetch()`); the
boolean is the wire.

Every helper git subprocess inherits no FUSE session fd
(`git-store::proc::harden_fds`, CLOEXEC) and runs with `GIT_OPTIONAL_LOCKS=0`
so inspection never takes `index.lock`. `BatchSession` additionally runs with
`GIT_NO_LAZY_FETCH=1`: it must only be asked for objects already known present
(the caller is the residency authority), or git terminates the session.

---

## 2. Materialization and single-flight

`Projection::open` fixes `baseline_tree` from the HEAD tree once, for the life
of the projection (`crates/worktree/src/lib.rs:208`). A read of a baseline file
resolves to a baseline `ObjectId`, then `open_content` materializes it
(`lib.rs:559`).

`materialize_path` (`lib.rs:596`) is the core:

- **Fast path:** if `cache_dir/<oid hex>` already exists, return it — no fetch,
  no lock.
- **Single-flight:** otherwise take the per-oid lock from
  `inflight: Mutex<HashMap<ObjectId, Arc<Mutex<()>>>>` (`lib.rs:141`). The first
  caller streams the blob; concurrent callers for the same oid block on that
  oid's lock and reuse the published file. There is **one** retrieval per
  missing oid, and no global lock is held across the subprocess.
- **Stream + atomic publish:** `GitStore::blob_to_file`
  (`crates/git-store/src/store.rs:322`) runs `cat-file blob` with stdout wired
  straight to a temp file — git writes the content; this process never buffers
  it. A successful temp file is `rename`d into place. A partial file is never
  observed under the final name.
- **Hydration accounting:** each real materialization bumps `Projection.hydrations`
  (`lib.rs:133`, `lib.rs:615`). This counter is the signal behind the budget
  assertions ("`ls` = 0 hydrations, `cat` ≥ 1").

The mount serves the **raw baseline blob** here (`blob_to_file` reads the
unfiltered object). Smudge-side `.gitattributes` conversions
(`eol=crlf`/`ident`/`working-tree-encoding`/custom `filter=`/LFS) therefore
diverge on read, while commits stay byte-correct because git's clean filter is
the inverse. See [compatibility.md](compatibility.md) and
[limitations.md](limitations.md) for that contract; the filter primitive that
would close the read-side gap is `smudge_blob` (§4).

**Invariants (shipped tests):**

- `open_content_serves_blob_bytes_from_a_cache_fd`
  (`crates/worktree/src/lib.rs:1214`) — content is served from the cache fd.
- `partial_clone_fetches_trees_but_not_blobs`
  (`crates/git-store/tests/store_integration.rs:37`) — under `tree:0`, trees and
  blobs are absent until faulted.

---

## 3. Bounded streaming: `ContentHandle`

`ContentHandle` (`crates/worktree/src/lib.rs:155`) is the unit of all content
I/O on the read path. It is either the tiny synthetic `.git` bytes in memory or
an open `File` over the cache. `read_at(offset, len)` is a `pread`
(`lib.rs:180`): it allocates a buffer of **`len`** (the FUSE request size),
never the file size. Reading a 64 MiB blob in request-sized ranges grows RSS by
roughly one request-sized buffer, not the blob size — large-file reads are O(1)
in memory.

**Invariant.** Peak RSS for a read is bounded by the request length,
independent of blob size. The `fuse` read path services strictly by file
handle via `pread`/`pwrite` (no whole-file buffering); see
[fuse-semantics.md](fuse-semantics.md).

---

## 4. Filters, attributes, and exact size

### 4.1 The filter primitive

`GitStore::smudge_blob` (`crates/git-store/src/store.rs:356`) is the shipped
filtering primitive: `cat-file --filters --path=<p> --attr-source=<src>`, which
returns exactly the bytes a normal checkout would write. `--attr-source` lets
`.gitattributes` resolve from a tree-ish (e.g. the workspace base commit) even
in a bare shared store whose `HEAD` need not match — essential for correct
attribute resolution without a worktree. Byte-level filtering stays git's job;
the mount does not reimplement clean/smudge drivers, EOL, encoding, or `ident`.

`smudge_blob` requires a UTF-8 path (it is passed to `cat-file --path`); a
non-UTF-8 path returns `InvalidRepositoryPath` and the caller must fall back to
a raw read. It is plumbing available to callers; the projection read path
currently materializes the raw blob (§2), so smudge conversions are not applied
on read today.

### 4.2 Size and metadata

A tree entry carries **no size**. The two metadata paths are deliberately split:

- **`readdir` never resolves a size.** `Projection::readdir`
  (`crates/worktree/src/lib.rs:484`) merges baseline-tree children with overlay
  children and returns name + kind + inode only — O(direct children), **zero**
  blob fetches. The FUSE side (`crates/fuse/src/mount.rs:533`) emits names +
  d_type + inode. So `ls` of a huge directory fetches nothing.
- **`getattr` returns an exact size and may fault once.** `attr_of`
  (`lib.rs:405`) resolves the size: an overlay file is `fstat`ed (no fetch); a
  baseline file calls `GitStore::object_size` (`store.rs:305`,
  `cat-file -s`) — the exact raw object size, read from the object header only,
  no content. Under `tree:0` the blob may be absent, so the first `ls -l` /
  `stat` of an unmaterialized file faults its blob in once (gated by
  `metadata_fetch`, `lib.rs:137`). This is fundamental to lazy-blob fetching,
  not a closeable gap — an exact projected size needs the object's bytes.

`object_size` never fakes a value: a cache-only miss returns a structured
offline/missing error (mapped to `EIO`/`ENOENT` via `core::ErrorCode`), never a
zero or a guess.

First writable `open` / `O_TRUNC` seeds an empty overlay file and does **not**
fetch the baseline blob, so size becomes a local `fstat`.

**Invariant (shipped test):** `large_directory_readdir_fetches_zero_blobs`
(`crates/worktree/src/lib.rs:1550`) — a large directory listing performs no blob
fetches.

### 4.3 First `git status` faults zero blobs

The mount pre-seeds the index FSMonitor extension after `read-tree HEAD` so the
**first** clean `git status` faults zero blobs (separate from the `ls -l` size
fault above). That seed, its conversion-attribute carve-out, and the
full-invalidation rules are owned by [fsmonitor.md](fsmonitor.md); the real
index build is [index-strategy.md](index-strategy.md). This doc does not
re-derive them.

### 4.4 Batch metadata session

`BatchSession` (`crates/git-store/src/batch.rs`) is a long-lived
`cat-file --batch-command` process exposing `info(oid) -> Option<ObjectInfo>`
and `contents(oid)`. `ObjectInfo { kind, size }` (`batch.rs:16`) reports the
**raw** object size and type with no content materialization on `info`. Because
it runs `GIT_NO_LAZY_FETCH=1`, it must only be queried for present objects;
`Ok(None)` means locally missing. Test:
`batch_session_serves_local_and_reports_missing`
(`crates/git-store/tests/store_integration.rs:299`).

---

## See also

- [worktree-model.md](worktree-model.md) — baseline/overlay, BaseRef, rename.
- [fuse-semantics.md](fuse-semantics.md) — FUSE ops, handles, the two pools.
- [fsmonitor.md](fsmonitor.md) — the zero-blob first-status seed (canonical).
- [index-strategy.md](index-strategy.md) — `read-tree HEAD`, interop bridge.
- [compatibility.md](compatibility.md) / [limitations.md](limitations.md) —
  smudge divergence and the lazy-size fault as documented behavior.
- [durability-security.md](durability-security.md) — auth/offline gating,
  `GIT_TERMINAL_PROMPT=0`.
- [design.md](design.md) — the lean spec this area expands.
</content>
