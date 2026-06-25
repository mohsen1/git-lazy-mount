# Known limitations & tracked refinements

What's by-design, what's fundamental, and what's genuinely deferred. We don't
claim anything we haven't proven. What **does** work is in the
[requirements checklist](requirements-checklist.md). This page lists the
refinements that shipped, the costs that are fundamental to `blob:none`, and the
few items still deferred, each with the test that will be un-`#[ignore]`d when it
lands.

## Correctness / filesystem refinements

| # | Area | Status | Un-ignores |
|---|------|--------|-----------|
| R1 | **Open-unlink retention**: `getattr` on a deleted-but-open inode now falls back to the live fd's size (the mount tracks each handle's inode), so `seek(End)`/`fstat` work after `unlink`. Reads/writes go through the held fd (Linux fd survival). | ✅ fixed | `m2_semantics::open_then_unlink_handle_survives_and_name_is_gone` (un-ignored) |
| R2 | **`O_TRUNC` no-fetch**: enabled `FUSE_ATOMIC_O_TRUNC` so a truncating open is one `open(O_TRUNC)` (handled with an empty overlay, no CoW) instead of `open`(no-trunc)→CoW + `setattr(0)`. | ✅ fixed | `m2_semantics::otrunc_open_fetches_no_old_blob` (un-ignored) |
| R3 | **Fetch coalescing (single-flight)**: a per-oid lock in `materialize_path` so N concurrent faults of one missing blob cause exactly one retrieval. | ✅ fixed | `m2_semantics::hundred_concurrent_reads_coalesce_to_one_retrieval` (un-ignored) |
| R4 | **Content-file retention**: overlay content is deleted eagerly on tombstone/clear, but an open handle keeps working via **Linux fd survival** (POSIX: an unlinked inode persists until the last fd closes), which this Linux-only tool relies on by design. Correct as is. An explicit retain-until-last-release refcount would be tidier but adds no correctness on Linux. | by-design (Linux fd survival) | `m2_semantics::open_then_unlink_handle_survives_and_name_is_gone` |
| R5 | **Directory/subtree rename**: `Projection::rename` now moves a whole directory. Overlay descendants re-key, baseline descendants become base-refs at the destination, and the source subtree is tombstoned. Metadata-only, no blob fetch. | ✅ fixed | `worktree::directory_rename_moves_subtree_without_fetch`, `survey_worktree_ops` (un-ignored) |
| R6 | **`getattr` size hydration (fundamental to `blob:none`)**: the exact size of an unmaterialized blob requires faulting the object. Trees carry no sizes, and `cat-file --batch-check` fetches the whole promisor object to report one. So `ls -l`/`stat` of an unmaterialized file faults its blob once for the size. (`git status`/`git diff` do **not** fault: the seeded FSMonitor extension lets git skip the stat entirely, see P1.) Not closeable without a server-side size manifest. | by-design (fundamental) | n/a |
| R7 | **Smudge-side `.gitattributes`**: the projection serves the raw baseline blob, so a file governed by a *smudge* filter (`eol=crlf`, `ident`, `working-tree-encoding`, a custom `filter=`/LFS driver) reads through the mount as its stored bytes (LF, unexpanded `$Id$`), not the bytes a real checkout would write. Git's *content* comparison stays clean (the clean filter is the inverse) and **commits remain byte-correct**. Only working-tree reads of smudge-attributed files diverge. Applying smudge at materialize would make `getattr` size depend on the filter output, breaking the lazy-stat and clean-rename-without-fetch guarantees (the directory-rename-without-fetch path of R5). A correct fix needs filter-aware lazy sizing. The first-status FSMonitor seed (P1) carves these paths out, so git still checks them normally. | by-design (documented) | n/a |

## Performance / laziness (wired and shipped)

| # | Area | Status |
|---|------|--------|
| P1 | **FSMonitor v2**: **wired** to `core.fsmonitor` (`git-lazy-mount-fsmonitor` reads the durable change journal; every worktree mutation is recorded synchronously), giving correct change detection with no false negatives (`fsmonitor` test). **The first clean `git status`/`git diff` faults zero blobs.** A freshly `read-tree`'d index has no FSMonitor extension, so git would stat (and so fault) every entry before writing one; pre-seeding the extension at mount (every entry `CE_FSMONITOR_VALID` plus the journal's seq-0 token, `seed_fsmonitor_valid`) makes git's `refresh_cache_ent` early-return on the valid bit *before* any `lstat`, while the hook answers "nothing changed". Paths under a checkout conversion (`filter`/`ident`/`working-tree-encoding`/CRLF `eol`, R7) are carved out so git still checks them. Verified zero-fault on an 81k-file mount (`first_status_faults_zero_blobs_and_surfaces_edits`). | wired; first status zero-blob |
| P2 | **Bounded executor split** (done): `readdir` (which reads only present tree objects plus the overlay, never a blob) runs on a separate small metadata pool. Object-IO callbacks (read/open/write/lookup/getattr/…) keep the main pool. So an `ls` stays responsive even when every IO thread is hydrating a blob. `lookup`/`getattr` still fault for *size*, but that's R6, a separate fundamental cost, not this split's concern. | ✅ done |
| P3 | **Switch/rebase eagerness** (measured, `switch_eagerness`): a branch switch over an M-of-N delta touches O(M) blobs, not O(N). Bounded by the delta, not the repo. | ✅ measured |

## Product surface: proven core, genuinely deferred extras

| # | Area | Status |
|---|------|--------|
| S1 | **Large-file bounded memory** (proven, `large_file`): reading a 64 MiB baseline blob grows daemon RSS ~2 MiB, not 64 MiB, via streamed `cat-file`→cache + `pread`. The multi-GiB / 100k-file extreme scale is heavier CI stress. The streaming and O(direct children) properties are structural and proven at representative sizes (64 MiB file, 1000-file readdir). | ✅ proven (representative) |
| S2 | **Shared object cache** across workspaces, **LFS / custom filters** (an external `filter=` driver, git-lfs not installed in CI; the *clean* filter and native `text/eol/ident` attributes are exercised, R7), and **submodules / nested worktrees** (classified `partial`). | deferred |
| S3 | **Windows (ProjFS) / macOS (FSKit)** backends: out of scope for this Linux-only tool. The engine is platform-neutral and the design notes are kept under [`future-platforms/`](future-platforms/). | out of scope |

The shipped refinements (R1–R5, P1–P3, S1) have green tests, named above. The
by-design and fundamental costs (R4, R6, R7) are stated plainly rather than
tracked as bugs. The genuinely deferred items (S2's shared cache, LFS,
submodules) still carry a test that will flip from `#[ignore]` to green when
implemented.
