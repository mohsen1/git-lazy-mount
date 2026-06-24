# Known limitations & tracked refinements

Honest status of the transparent rebuild (redesign.md §44: do not claim what is
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
| R5 | **Directory/subtree rename** — `Projection::rename` handles files/symlinks; a directory rename returns `EOPNOTSUPP`. | deferred | §29 | — |
| R6 | **`getattr` size hydration** — exact size of an unmaterialized `blob:none` file requires faulting the object (no size manifest yet); so `ls -l` / a bare `stat` hydrates (sanctioned by §21/§38.3, but eager). | by-design (eager) | §21, §38.3 | — |

## Performance / laziness (built but not yet wired)

| # | Area | Status | Spec |
|---|------|--------|------|
| P1 | **FSMonitor v2 wiring** — the durable token + change journal (`glm-worktree::journal`) and the FSMonitor-valid index bootstrap are not yet wired to `core.fsmonitor` + a hook, so clean `git status` is *correct but eager* (reads each file) rather than 0-blob. | journal done; hook/serve/feed/bootstrap pending | §12, §38.4 |
| P2 | **Bounded executor split** — one bounded pool today; §18 wants separate fast-metadata vs object-IO pools. | deferred | §18 |
| P3 | **Switch/rebase eagerness measurement** — branch transitions work but write every changed file into the overlay; the eagerness is not yet *measured/reported*. | deferred | §27 |

## Product surface not yet built

| # | Area | Status | Spec |
|---|------|--------|------|
| S1 | **One-command CLI** — `git lazy-mount <url> <path>` (daemonized, returns after the mount is ready) + lifecycle/diagnostic verbs. The mount + git-workflows are proven in-process via tests; the user-facing binary + daemonization (re-exec serve) are pending. The old `crates/cli`/`crates/daemon` (forbidden subcommands) are superseded and await quarantine. | pending | §1, §9, §10, §43.1 |
| S2 | **Conflict-stage / rebase / fetch-pull / add -p tests** — being added (coverage teammate). | in progress | §26 |
| S3 | **Crash-injection durability** (criterion 27), **multi-GiB** large-file (criterion 25 / Experiment I full scale), **100k-file** scale (Experiment B), **macOS/Windows** (M8). | deferred | §40.5, §39, §42 |
| S4 | **Shared object cache** (M7), **LFS / filters** beyond identity (§23/§24), **submodules/worktrees** (§26.8). | deferred | §34, §23, §24 |

Nothing above is claimed as working in the checklist. Each refinement has a spec
reference and (where applicable) a test that will flip from `#[ignore]` to green
when implemented.
