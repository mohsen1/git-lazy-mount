# Known limitations & tracked refinements

Honest status of the transparent rebuild (design.md §44: do not claim what is
not proven). What **works today** is in [`requirements-checklist.md`](requirements-checklist.md);
this lists what is deliberately deferred, with the spec reference and the test
that will be un-`#[ignore]`d when it lands.

## Correctness / filesystem refinements

| # | Area | Status | Spec | Un-ignores |
|---|------|--------|------|-----------|
| R1 | **Open-unlink retention** — `getattr` on a deleted-but-open inode now falls back to the live fd's size (the mount tracks each handle's inode), so `seek(End)`/`fstat` work after `unlink`; reads/writes go through the held fd (Linux fd survival). | ✅ fixed | §17.4, §14 | `m2_semantics::open_then_unlink_handle_survives_and_name_is_gone` (un-ignored) |
| R2 | **`O_TRUNC` no-fetch** — enabled `FUSE_ATOMIC_O_TRUNC` so a truncating open is one `open(O_TRUNC)` (handled with an empty overlay, no CoW) instead of `open`(no-trunc)→CoW + `setattr(0)`. | ✅ fixed | §38.7, §17.2 | `m2_semantics::otrunc_open_fetches_no_old_blob` (un-ignored) |
| R3 | **Fetch coalescing (single-flight)** — a per-oid lock in `materialize_path` so N concurrent faults of one missing blob cause exactly one retrieval. | ✅ fixed | §38.6, §20.1 | `m2_semantics::hundred_concurrent_reads_coalesce_to_one_retrieval` (un-ignored) |
| R4 | **Content-file retention** — overlay content is deleted eagerly on tombstone/clear; relies on Linux fd survival for open handles. Explicit retain-until-last-release is cleaner. | partial | §17.4 | — |
| R5 | **Directory/subtree rename** — `Projection::rename` now moves a whole directory: overlay descendants re-key, baseline descendants become base-refs at the destination, the source subtree is tombstoned. Metadata-only, no blob fetch. | ✅ fixed | §29 | `worktree::directory_rename_moves_subtree_without_fetch`, `survey_worktree_ops` (un-ignored) |
| R6 | **`getattr` size hydration** — exact size of an unmaterialized `blob:none` file requires faulting the object (no size manifest yet); so `ls -l` / a bare `stat` hydrates (sanctioned by §21/§38.3, but eager). | by-design (eager) | §21, §38.3 | — |
| R7 | **Smudge-side `.gitattributes`** — the projection serves the **raw baseline blob**, so a file governed by a *smudge* filter (`eol=crlf`, `ident`, `working-tree-encoding`, a custom `filter=`/LFS driver) reads through the mount as its stored bytes (LF, unexpanded `$Id$`), not the bytes a real checkout would write. Git's *content* comparison stays clean (the clean filter is the inverse) and **commits remain byte-correct**; only working-tree reads of smudge-attributed files diverge. Applying smudge at materialize would make `getattr` size depend on the filter output, breaking the lazy-stat / clean-rename-without-fetch guarantees (R3/§29) — a correct fix needs filter-aware lazy sizing. | by-design (documented) | §23, §24 | — |

## Performance / laziness (built but not yet wired)

| # | Area | Status | Spec |
|---|------|--------|------|
| P1 | **FSMonitor v2 wiring** — the durable token + change journal (`glm-worktree::journal`) and the FSMonitor-valid index bootstrap are not yet wired to `core.fsmonitor`. **Measured:** a *repeat* clean `git status` already faults 0 blobs (git's index refresh), so FSMonitor's remaining value is making the *first* status lazy (§12.2 bootstrap) and avoiding the full stat scan on huge repos. | journal done; wiring = optimization | §12, §38.4 |
| P2 | **Bounded executor split** — one bounded pool today; §18 wants separate fast-metadata vs object-IO pools. | deferred | §18 |
| P3 | **Switch/rebase eagerness measurement** — branch transitions work but write every changed file into the overlay; the eagerness is not yet *measured/reported*. | deferred | §27 |

## Product surface not yet built

| # | Area | Status | Spec |
|---|------|--------|------|
| S1 | **multi-GiB** large-file bounded-memory (criterion 25 / Experiment I full scale; 4 MiB proven) and **full 100k-file** scale (Experiment B proven at 1000). | deferred (scale) | §39 |
| S2 | **Shared object cache** across workspaces, **LFS / filters** beyond identity (§23/§24), **submodules / nested worktrees** (§26.8). | deferred | §34, §23, §24 |
| S3 | **Windows (ProjFS) / macOS (FSKit)** backends — out of scope for this Linux-only tool; the engine is platform-neutral and the design notes are kept under [`future-platforms/`](future-platforms/). | out of scope | §42 |

Nothing above is claimed as working in the checklist. Each refinement has a spec
reference and (where applicable) a test that will flip from `#[ignore]` to green
when implemented.
