# Known limitations & tracked refinements

What's by-design, what's fundamental, and what's genuinely deferred — honestly
(we don't claim anything we haven't proven). What **does** work is in the
[requirements checklist](requirements-checklist.md);
this lists the refinements that shipped, the costs that are fundamental to
`blob:none`, and the few items still deferred, each with the test
that will be un-`#[ignore]`d when it lands.

## Correctness / filesystem refinements

| # | Area | Status | Un-ignores |
|---|------|--------|-----------|
| R1 | **Open-unlink retention** — `getattr` on a deleted-but-open inode now falls back to the live fd's size (the mount tracks each handle's inode), so `seek(End)`/`fstat` work after `unlink`; reads/writes go through the held fd (Linux fd survival). | ✅ fixed | `m2_semantics::open_then_unlink_handle_survives_and_name_is_gone` (un-ignored) |
| R2 | **`O_TRUNC` no-fetch** — enabled `FUSE_ATOMIC_O_TRUNC` so a truncating open is one `open(O_TRUNC)` (handled with an empty overlay, no CoW) instead of `open`(no-trunc)→CoW + `setattr(0)`. | ✅ fixed | `m2_semantics::otrunc_open_fetches_no_old_blob` (un-ignored) |
| R3 | **Fetch coalescing (single-flight)** — a per-oid lock in `materialize_path` so N concurrent faults of one missing blob cause exactly one retrieval. | ✅ fixed | `m2_semantics::hundred_concurrent_reads_coalesce_to_one_retrieval` (un-ignored) |
| R4 | **Content-file retention** — overlay content is deleted eagerly on tombstone/clear; an open handle keeps working via **Linux fd survival** (POSIX: an unlinked inode persists until the last fd closes), which this Linux-only tool relies on by design. Correct; an explicit retain-until-last-release refcount would be tidier but adds **no correctness on Linux**. | by-design (Linux fd survival) | `m2_semantics::open_then_unlink_handle_survives_and_name_is_gone` |
| R5 | **Directory/subtree rename** — `Projection::rename` now moves a whole directory: overlay descendants re-key, baseline descendants become base-refs at the destination, the source subtree is tombstoned. Metadata-only, no blob fetch. | ✅ fixed | `worktree::directory_rename_moves_subtree_without_fetch`, `survey_worktree_ops` (un-ignored) |
| R6 | **`getattr` size hydration (fundamental to `blob:none`)** — the exact size of an unmaterialized blob requires faulting the object: trees carry no sizes, and `cat-file --batch-check` fetches the whole promisor object to report one. So `ls -l`/`stat` and the **first** `git status` (which must populate the index stat to verify cleanliness) fault each blob once. This is **not closeable** without a server-side size manifest — it is the root cause of P1's first-status cost. | by-design (fundamental) | — |
| R7 | **Smudge-side `.gitattributes`** — the projection serves the **raw baseline blob**, so a file governed by a *smudge* filter (`eol=crlf`, `ident`, `working-tree-encoding`, a custom `filter=`/LFS driver) reads through the mount as its stored bytes (LF, unexpanded `$Id$`), not the bytes a real checkout would write. Git's *content* comparison stays clean (the clean filter is the inverse) and **commits remain byte-correct**; only working-tree reads of smudge-attributed files diverge. Applying smudge at materialize would make `getattr` size depend on the filter output, breaking the lazy-stat / clean-rename-without-fetch guarantees (the directory-rename-without-fetch path of R5) — a correct fix needs filter-aware lazy sizing. | by-design (documented) | — |

## Performance / laziness (wired and shipped)

| # | Area | Status |
|---|------|--------|
| P1 | **FSMonitor v2** — **wired** to `core.fsmonitor` (`git-lazy-mount-fsmonitor` reads the durable change journal; every worktree mutation is recorded synchronously). Gives correct change detection (no false negatives, `fsmonitor` test) and skips the redundant stat scan on *subsequent* statuses (the real win on huge repos). The *first-status zero-blob bootstrap is **fundamentally unachievable*** with stock git + `blob:none` (verified via `GIT_TRACE_FSMONITOR`): git marks each read-tree'd entry clean from the hook's empty reply, then `mark_fsmonitor_invalid` because the entry has **no stat data** — git must populate the stat (incl. size) to skip the content check, and under `blob:none` the size requires fetching the blob. The fsmonitor-valid bit does not override an empty-stat entry. So the *first* clean status faults each blob once (root cause is R6). | wired; first-status eager is fundamental |
| P2 | **Bounded executor split** — **done**: `readdir` (which reads only present tree objects + the overlay, never a blob) runs on a separate small **metadata pool**; object-IO callbacks (read/open/write/lookup/getattr/…) keep the main pool. So an `ls` stays responsive even when every IO thread is hydrating a blob. (`lookup`/`getattr` still fault for *size* — that's R6, a separate fundamental cost, not this split's concern.) | ✅ done |
| P3 | **Switch/rebase eagerness** — **measured** (`switch_eagerness`): a branch switch over an M-of-N delta touches O(M) blobs, not O(N) — bounded by the delta, not the repo. | ✅ measured |

## Product surface: proven core, genuinely deferred extras

| # | Area | Status |
|---|------|--------|
| S1 | **Large-file bounded memory** — **proven** (`large_file`: reading a 64 MiB baseline blob grows daemon RSS ~2 MiB, not 64 MiB — streamed `cat-file`→cache + `pread`). The multi-GiB / 100k-file *extreme* scale is heavier CI stress; the streaming + O(direct children) properties are structural and proven at representative sizes (64 MiB file, 1000-file readdir). | ✅ proven (representative) |
| S2 | **Shared object cache** across workspaces, **LFS / custom filters** (an external `filter=` driver — git-lfs not installed in CI; the *clean* filter and native `text/eol/ident` attributes are exercised, R7), **submodules / nested worktrees** (classified `partial`). | deferred |
| S3 | **Windows (ProjFS) / macOS (FSKit)** backends — out of scope for this Linux-only tool; the engine is platform-neutral and the design notes are kept under [`future-platforms/`](future-platforms/). | out of scope |

The shipped refinements (R1–R5, P1–P3, S1) have green tests, named above; the
by-design / fundamental costs (R4, R6, R7) are stated plainly rather than
tracked as bugs; the genuinely deferred items (S2's shared cache, LFS,
submodules) still carry a test that will flip from `#[ignore]` to green when
implemented.
