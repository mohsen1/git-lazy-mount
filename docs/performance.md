# Performance and complexity invariants

*(spec §49)*

git-lazy-mount's value proposition is that work scales with **what changed or
what you touched**, not with repository size. This document states the intended
complexity invariants, marks which are demonstrated by tests today, and lists
the explicit exemptions where a bound is deliberately *not* sub-linear.

## Intended complexity invariants

| Operation | Intended cost |
| --- | --- |
| `mount` | `O(ref resolution + root metadata)` |
| `readdir` | `O(entries in that directory)` |
| `status` | `O(changed/staged/conflicted paths)` |
| `diff` | `O(selected changed paths)` |
| clean branch switch | `O(changed tree regions)` |
| read a clean file | `O(file bytes + fetch)` |
| rename a clean file | `O(metadata)` — **no blob fetch** |
| rename a clean subtree | `≈ O(changed dir mappings)` |

The unifying principle: enumeration and metadata operations never read whole-
tree state or fault content, and content cost is paid only for bytes you
actually read.

## Test-verified today

These invariants are asserted by the integration suite against real Git. The
budget assertions read provider **metrics**
([crates/object-provider/src/metrics.rs](../crates/object-provider/src/metrics.rs)):
`objects_fetched`, `blob_reads`, `filtered_reads`, `fetch_invocations`,
`presence_checks`, `bytes_read`, `coalesced_waits`.

* **`status` does not fetch or scan content.**
  `status_reports_overlay_changes_without_fetching_blobs` asserts
  `objects_fetched` and `blob_reads` are unchanged across a `status` that
  reports an added file — status is driven by the staged delta and overlay, not
  a working-tree walk, and writes no objects.
* **Rename of a clean file fetches nothing.**
  `rename_clean_file_does_not_fetch_blob` asserts `objects_fetched` and
  `blob_reads` are unchanged: a clean rename writes a **base-reference**, not a
  copy (spec §53.10).
* **Truncate-to-zero does not fetch old content.**
  `truncate_to_zero_does_not_fetch_old_content` asserts `objects_fetched` is
  unchanged and the file reads back empty.
* **Executable-bit change fetches nothing.**
  `executable_bit_change_without_fetch` asserts `objects_fetched` is unchanged
  for a mode-only edit.
* **Coalescing 100 → 1.** `coalesces_100_concurrent_reads_into_one_fetch`:
  100 concurrent reads of one missing blob produce `fetch_invocations == 1`,
  `objects_fetched == 1`, and `blob_reads == 100` (spec §2.2, §53.5).
* **Distinct objects batched.** `ensure_objects_batches_distinct_objects`:
  three distinct missing objects fault in with a single `fetch_invocations`.
* **Commit reuses unchanged subtrees.**
  `commit_reuses_subtrees_and_passes_fsck`: a root-level change reuses the
  existing `src` subtree entry, and `git fsck` accepts the resulting commit and
  tree (so the reuse is genuine, not a rewrite).
* **Cache-only never touches the network.** `cache_only_read_never_fetches`:
  a `CacheOnly` read of an absent blob errors `offline_missing_object` with the
  fetcher invoked **zero** times.

## Asserted by design, not yet benchmarked

The following are intended invariants without a dedicated performance test or
benchmark yet, and are labeled as such:

* `mount` = `O(ref resolution + root metadata)` — wired structurally (a mount
  resolves refs and reads the root; it does not enumerate the tree), but not
  benchmarked against repository size.
* `readdir` = `O(entries in that directory)` — the directory-listing path reads
  a single tree level, but there is no large-fan-out benchmark asserting the
  bound.
* `diff` = `O(selected changed paths)` — follows the same changed-path model as
  `status`; not separately metric-asserted.
* clean branch switch = `O(changed tree regions)` — the tree-builder reuses
  unchanged subtrees (the mechanism `commit` is tested on), but a switch-cost
  benchmark over a large tree is not yet present.
* `read` = `O(file bytes + fetch)` and rename-subtree `≈ O(changed dir
  mappings)` — consistent with the read/rename paths above; no size-scaling
  benchmark yet.

We do not present these as measured results.

## Explicit exemptions

Some operations are intentionally **not** sub-linear; pretending otherwise would
be dishonest:

* **Full path preflight.** Validating an entire pathspec up front is `O(paths
  supplied)` by design.
* **Full-tree searches.** Operations that are semantically "look at everything"
  (e.g. a whole-tree grep/scan) are inherently `O(tree)`; lazy-mount does not
  claim to make them cheaper.
* **Exact `stat` when the size is unknown.** When an exact working-tree size is
  required and not already known (notably under `autocrlf`/`eol`, where the
  filtered size differs from the raw blob and is platform-dependent), computing
  it costs `O(file bytes)` — it may require materializing/filtering the
  content. See [filters-and-lfs.md](filters-and-lfs.md) and
  [metadata-limitations.md](metadata-limitations.md).
* **Full-history clone.** If a remote rejects the partial-clone filter and a
  full-object clone is permitted (`--allow-full-object-clone`), that clone is
  `O(history)` — the lazy guarantees apply *after* a successful partial clone,
  not to a forced full one.

## How the budgets are measured

Every hydration-budget assertion checks the provider's metrics snapshot
([crates/object-provider/src/lib.rs](../crates/object-provider/src/lib.rs))
before and after an operation — typically that `objects_fetched` and/or
`blob_reads` did **not** increase. Metrics are the contract: a regression that
fetched a blob it should not have would change a counter and fail the test.

Note the scope of the counters: the CLI is **per-process**, so metrics start at
zero for each invocation and reflect only that command. A long-lived daemon
would **accumulate** metrics across operations instead. Interpret a metrics
snapshot accordingly (per-command under the CLI; cumulative under a daemon).
