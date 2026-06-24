# Index + scalability feasibility (Profiles A–D)

Authoritative spec: [`redesign.md`](../../redesign.md) §11 (the scalability gate),
§27 (checkout/switch/rebase eagerness), §38 (hydration budgets). Companion
overview: [`architecture.md`](./architecture.md). This doc is the Milestone-0
deliverable "index strategy comparison" (§42 M0, §45 item 9) and the design that
Milestone 6 (§42 M6) selects a winner from.

> **The choice MUST come from measurements, not preference (§11, §27, §42 M6).**
> Profile **A is the correctness baseline and must work WITHOUT skip-worktree**
> (§4.4, §11.1). B/C/D are optimizations gated behind A and behind dedicated
> feasibility tests. We do not advertise a profile until a real mounted test
> proves its compatibility *and* its measured laziness.

---

## 0. The central scale question

Stock Git keeps one authoritative stage: `$GIT_DIR/index` (§4.2, §7, §25). For a
repo of `N` tracked paths, that index is `O(N)` entries on disk and the cost of
parsing it bounds every `status`, `diff`, `add`, and ref-moving command. A
1M-path monorepo has a ~100–300 MB v4 index. The redesign removes the old
`crates/stage` JSON delta and the `crates/git-store/src/interop.rs` skip-worktree
bridge (both **superseded**, §4.2, §4.4) and operates the *real* index directly.
Three independent costs must each be bounded or measured:

| Cost | Driver | Where it bites |
|------|--------|----------------|
| **Index construction** | `O(N)` entries written at mount (§10.4) | one-time, at `building-index` lifecycle state (§4.1) |
| **Index parse** | `O(entries)` per Git invocation | every `status`/`add`/`commit` |
| **Worktree scan** | `O(N)` `lstat`s unless suppressed | every `status` without FSMonitor |
| **Checkout eagerness** | `O(changed paths)` blob fetch + FUSE write | every `switch`/`reset --hard`/`merge` (§27) |

Profiles A–D attack these costs differently. The non-negotiable invariant: a
`blob:none` mount fetches **zero working-file blobs** to project the tree (§38.1),
and clean `status` after bootstrap fetches **zero blobs and runs zero smudge
filters** (§38.4). The worktree scan and checkout-eagerness costs are what the
profiles trade against compatibility.

---

## 1. Shared substrate (used by all profiles)

These types/sessions already exist and are **reusable as-is**; the profiles
differ only in *how the index is built and which bits are set*.

- `glm_core::RepoPath` (`crates/core/src/path.rs`) — byte-exact path identity, no
  lossy UTF-8 (§31). All index entry paths flow through this.
- `glm_core::{ObjectId, GitMode, TreeEntry, TreeObject}` (`crates/core/src/`).
- `glm_git_store::GitStore` (`crates/git-store/src/store.rs`) — `read_tree`,
  `smudge_blob` (with `--attr-source`), `hash_blob_clean`, all hardened with
  `GIT_NO_LAZY_FETCH` / `GIT_OPTIONAL_LOCKS=0`.
- `glm_git_store::BatchSession` (`crates/git-store/src/batch.rs`) — long-lived
  `cat-file --batch-command`, CLOEXEC-hardened (§19). The residency authority for
  tree/blob presence checks during index build.
- `glm_object_provider::GitObjectProvider` (`crates/object-provider/src/lib.rs`) —
  coalescing fetch + presence cache; its metrics back the budget assertions.

**Index I/O is stock-Git plumbing, never a re-implementation** (§4.2, §6, §25).
The daemon writes the index by driving `git read-tree`, `git update-index`, and
`git -c core.fsmonitor… status`, then *parses* the result read-only into a
disposable cache (§7, §25). It never hand-encodes the index binary format except
where a profile requires a bit Git's porcelain cannot set (called out per
profile).

### 1.1 Index-cache parse (read-only, all profiles)

```rust
/// Disposable parsed view of $GIT_DIR/index (spec §25). Rebuilt from disk on
/// any index.lock→index replacement; never authoritative.
pub struct IndexCache {
    pub checksum: [u8; 32],          // trailing index checksum; cache key
    pub format_version: u8,          // 2 | 3 | 4
    pub entries: Vec<IndexEntryView>,// stage-0 + unmerged 1/2/3
    pub is_split: bool,              // link extension present
    pub is_sparse: bool,            // sparse-directory entries present (Profile C)
    pub fsmonitor_valid: BitVec,     // per-entry CE_FSMONITOR_VALID
}

pub struct IndexEntryView {
    pub path: RepoPath,
    pub oid: ObjectId,
    pub mode: GitMode,
    pub stage: u8,                   // 0 normal; 1/2/3 conflict (§25.3)
    pub skip_worktree: bool,         // CE_SKIP_WORKTREE (Profile B)
    pub fsmonitor_valid: bool,       // CE_FSMONITOR_VALID (§12.2)
    pub assume_unchanged: bool,      // CE_VALID — must be false (§4.4)
}

pub trait IndexReader {
    /// Parse the current index. Cheap to call after each post-index-change hook.
    fn read_index(&self) -> Result<IndexCache>;
    /// True iff the on-disk checksum matches `cache`; lets a hook skip re-parse.
    fn index_unchanged(&self, cache: &IndexCache) -> Result<bool>;
}
```

Parsing uses `git ls-files --stage -z --debug` is **not** sufficient (it omits
flag bits); the cache is built from `git ls-files -z --stage` for paths/oids/modes
plus `git ls-files -z -v` / a direct read of the `flags` word for skip-worktree
and FSMonitor-valid bits. Profile-specific builders below set those bits.

---

## 2. Profile A — full index + FSMonitor (the correctness baseline)

**Status: REQUIRED. Must work without skip-worktree (§4.4, §11.1). Ships first.**

### 2.1 Characteristics (§11.1)

- Normal index semantics; maximum stock-Git compatibility.
- `O(N)` index construction at mount; possibly `O(N)` index parse per command.
- **No working-tree scan after FSMonitor bootstrap** — this is what makes clean
  `status` cheap despite the full index.
- Branch transitions are labeled **"potentially eager"** (§3.2, §27): stock Git
  may fetch + write every changed blob. We measure but do not hide this (§27).

### 2.2 FSMonitor-valid bootstrap (the key trick, §10.4, §11.1, §12.2)

The naïve full-index mount would make Git `lstat` every projected path on first
`status`, hydrating metadata for `N` files. We avoid that by building the index
already FSMonitor-valid, so Git trusts the monitor instead of scanning.

```rust
/// Build $GIT_DIR/index from `tree` with every entry CE_FSMONITOR_VALID and
/// stat data populated from the projection's *synthetic* metadata — fetching
/// ZERO blobs (spec §10.4, §38.1) and hashing ZERO working-tree contents
/// (spec §12.2).
fn bootstrap_index_profile_a(
    store: &GitStore,
    tree: &ObjectId,          // initial checked-out commit tree (baseline, §8)
    token0: &FsmonitorToken,  // first journal token (§12.1)
) -> Result<IndexCache>;
```

Procedure (exact commands that must be proven by Experiment H, §39):

1. `git read-tree <tree>` — populates `N` stage-0 entries, **0 blob fetches**
   (trees are present under `blob:none`, see `feasibility/partial-clone.md`).
2. Configure `core.fsmonitor=<hook>`, `core.fsmonitorHookVersion=2`,
   `core.untrackedCache=true`, `index.version=4`, `feature.manyFiles=true`
   (§10.5). `core.fileMode`/`core.symlinks`/`core.ignoreCase` from probed mount
   behavior.
3. Run the **first** `git status` with the FSMonitor hook returning token0 and an
   empty changed-path set. This is the step that *sets* `CE_FSMONITOR_VALID` on
   every clean entry: Git, trusting the monitor's "nothing changed", marks the
   entries valid and writes the index back. **It still `lstat`s once here** unless
   we also seed stat data — measure whether this first scan is acceptable or must
   be eliminated by writing stat data directly (fallback below).
4. **Fallback if step 3 still scans `N` files:** write the index binary directly
   with `ctime=mtime=0`, `size` left 0, and `CE_FSMONITOR_VALID` pre-set, so the
   monitor short-circuits the scan on the *first* status. This is the one place
   Profile A may bypass porcelain; it is isolated behind
   `bootstrap_index_profile_a` and guarded by a differential test vs a normal
   checkout's `status`.

**Invariant (regression test `profile_a_clean_status_zero_blobs`):** first and
every subsequent clean `status --porcelain=v2` fetches **0 blobs**, runs **0
smudge filters**, and does not `lstat` every projected path (§12.2, §38.4).

### 2.3 Features to evaluate (§11.1) and their decision criteria

| Feature | Config | Keep iff | Measure |
|---------|--------|----------|---------|
| index v4 | `index.version=4` | prefix-compression shrinks index, parse OK | index size, parse ms |
| split index | `core.splitIndex` | incremental writes cut `add` cost without breaking FSMonitor-valid | write ms, share-index churn |
| untracked cache | `core.untrackedCache=true` | dir-mtime invalidation works through FUSE (§12.3) | untracked scan count |
| FSMonitor-valid | §2.2 | bootstrap proven | first/clean `status` `lstat` count |
| `feature.manyFiles` | sets v4+untracked+fsmonitor | net win on large N | aggregate |
| preload-index | `core.preloadIndex` | threads help or hurt under FUSE latency | `status` wall time |

Each row is a measured A/B in Experiment H/G, not a default-on assumption.

### 2.4 Invariants (regression tests)

- `profile_a_no_skip_worktree`: no index entry has `CE_SKIP_WORKTREE` or
  `CE_VALID` (assume-unchanged) set (§4.4). Baseline must be correct without them.
- `profile_a_mount_zero_blobs`: mount of a `blob:none` repo fetches 0 working
  blobs (§38.1); index build reads only trees.
- `profile_a_index_only_ops_preserve_worktree`: `reset --mixed`, `restore
  --staged`, `rm --cached` change the index but **not** baseline+overlay bytes
  (§8.1, §25.1) — projection unchanged, `cat path` still yields the old bytes.
- `profile_a_differential_status`: mounted `status/diff/ls-files --stage` byte-
  identical to a conventional checkout at the same commit (§40.1).

---

## 3. Profile B — dynamic skip-worktree (experimental)

**Status: investigate ONLY after A works (§11.2). Must prove every command.**

The old `interop.rs` marked *every* entry skip-worktree as a universal trick
(§4.4 forbids this; that code is **superseded**). Profile B is the disciplined
version: skip-worktree tracks *materialization*, not "the FS is virtual".

### 3.1 Model (§11.2)

```
entry is CE_SKIP_WORKTREE  ⟺  path is clean AND unmaterialized in the overlay
entry is NOT skip-worktree ⟺  path is materialized (open/edited) OR has a
                              conflict stage OR is locally modified
the virtual FS exposes ALL paths regardless of the bit
```

State machine for one path's skip-worktree bit:

| Current | Event | Next | Index action |
|---------|-------|------|--------------|
| skip (clean, virtual) | first writable `open`/`create`/overlay write | not-skip | `update-index --no-skip-worktree <p>` |
| skip | merge/rebase writes conflict stages for `<p>` | not-skip | Git clears it when writing stages 1/2/3 |
| not-skip (materialized) | overlay dematerialized to baseline (§8.2 compaction) | skip | `update-index --skip-worktree <p>` |
| not-skip | `add`/`commit` records & path matches new baseline | skip | re-set after baseline advance |

Transitions are driven by the FUSE write path and the post-index-change /
post-checkout hooks (§13), never by a poll.

### 3.2 The exact Git commands that must be proven (§11.2)

These are the experiments that gate Profile B. Each must be run through a **real
mount** and diffed against a conventional checkout. Profile B is rejected if *any*
fails:

```bash
# B1 status must not invent deletions for skipped (virtual) paths
git status --porcelain=v2                 # clean; no " D " for skipped paths

# B2 add of a skipped path must NOT require --sparse (§11.2 "do not require
#    users to pass git add --sparse")
printf x >> src/skipped.rs; git add src/skipped.rs   # stages; clears skip bit

# B3 add -p of a skipped-then-materialized path
git add -p src/skipped.rs                  # hunk selection works

# B4 rm / mv of skipped paths
git rm src/skipped.rs ; git mv a b         # index + overlay agree

# B5 checkout/switch must not WRITE skipped files, and must not silently
#    clear skip bits across the whole tree
git switch other-branch                    # measure: entries un-skipped?
git checkout -- src/skipped.rs             # materializes exactly that path

# B6 reset --hard must re-skip clean paths, not materialize all of them
git reset --hard HEAD~1

# B7 merge/rebase must write conflicted paths (clearing their skip bit) but
#    leave clean skipped paths virtual
git merge topic ; git rebase main          # only conflicts materialize

# B8 stash / clean over a mixed skip/non-skip tree
git stash ; git stash pop ; git clean -fdn

# B9 sparse-index interaction (if combined with C): no corruption
git sparse-checkout list                   # consistent; index not corrupted
```

### 3.3 Things Git must be proven NOT to do (§11.2)

Hard failure conditions (each becomes a `profile_b_reject_*` test):

- `profile_b_reject_mass_clear`: Git must not clear `CE_SKIP_WORKTREE` across the
  **entire** tree on an ordinary command (some Git versions do this on `switch`).
- `profile_b_reject_add_refusal`: ordinary `git add <skipped>` must not error.
- `profile_b_reject_phantom_delete`: skipped+virtual paths must not show as `D`.
- `profile_b_reject_conflict_skip`: Git must not write skipped files during a
  conflict, nor leave a conflicted path skipped.
- `profile_b_reject_misreport`: deleted/modified paths must not be mis-reported
  because of a stale skip bit.

### 3.4 Why B is risky and bounded

Skip-worktree semantics are an *implementation detail of sparse-checkout*, not a
contract; behavior varies across Git versions (§11.2 "do not assume normal
sparse-checkout rules fit this product"). Profile B therefore pins a minimum Git
version and re-runs §3.2/§3.3 in CI against each supported Git. The win it buys:
clean `status` and ref-moving commands can skip the worktree for unmaterialized
paths *and* let us avoid materializing on `switch`. The risk: a Git upgrade
silently changes a bit's meaning. Hence A must stand alone.

---

## 4. Profile C — sparse index

**Status: measure whether a sparse index can represent unmaterialized subtrees
while the FS still exposes them (§11.3). Do not assume sparse-checkout rules fit.**

### 4.1 Model

A sparse index collapses an entire unmaterialized directory into one
**sparse-directory entry** (`mode=040000`, `CE_SKIP_WORKTREE`, oid = the tree),
instead of `O(files-in-subtree)` blob entries. For a monorepo where most subtrees
are never touched, this turns an `O(N)` index into `O(touched paths + boundary
dirs)`.

```
index entry kinds:
  file entry      mode 100644/100755/120000, stage 0..3   (materialized region)
  sparse-dir entry mode 040000, oid=<tree>, CE_SKIP_WORKTREE (collapsed subtree)
```

The FUSE projection still exposes every path inside a collapsed subtree (the
baseline tree answers `lookup`/`readdir`, §8, §15) — the sparseness is *only* in
the index, not in the namespace. This is the load-bearing difference from normal
sparse-checkout, where collapsed subtrees are absent from the worktree.

### 4.2 What must be measured / proven

- `profile_c_index_size`: index for `N=1M` with `K` touched paths is `O(K + dirs)`
  not `O(N)` (the whole point).
- `profile_c_expand_on_touch`: touching a path inside a collapsed dir expands
  exactly that dir (`git` does this via `command_requires_full_index`), and the
  expansion fetches **0 blobs** (only the tree, already present).
- `profile_c_status_consistency`: `status`/`diff`/`add` over a mixed sparse/dense
  index byte-match a conventional checkout for the materialized region.
- `profile_c_no_phantom_in_collapsed`: paths inside a collapsed subtree that the
  user views/edits via FUSE are correctly promoted (dir expanded, file entry
  added) without the index reporting them deleted.
- `profile_c_full_index_commands`: commands Git force-expands (`command_requires
  _full_index` list, e.g. some `merge`/`stash` paths) still complete — measure the
  expansion cost; if it is `O(N)`, that command falls back to Profile A cost.

Profile C composes with B (sparse-dir entries *are* skip-worktree). C without B
is the "collapsed subtree" win alone; B without C is per-file skip control. The
measurement matrix runs A, B, C, and B+C.

---

## 5. Profile D — minimal upstreamable Git provider extension

**Status: only if A–C cannot meet large-repo perf (§11.4, §27). NOT a wrapper.**

If stock Git remains correct-but-eager — fetching and writing every changed blob
on `switch`/`reset --hard`/`merge` (§27) — and B/C cannot prevent it, the answer
is a **minimal, upstreamable, explicitly-advertised** Git extension, not a command
wrapper and not a private fork shipped silently (§11.4, §1 forbids `git
lazy-mount git --`).

### 5.1 The provider protocol (proposed extension surface)

A "virtual working-tree provider" the upstream `git` process can ask to:

```
declare(paths)        -> mark paths virtual + clean (no bytes on disk)
update_baseline(tree) -> move the projected baseline WITHOUT writing file bytes
materialize(paths)    -> realize only paths needing conflict res / local edit
changed(since_token)  -> report changed paths (this is FSMonitor, generalized)
invalidate(paths)     -> drop projected paths
```

Concretely: a `git switch` would, instead of `checkout_entry()` writing every
changed blob, call `update_baseline(new_tree)` + `materialize(conflicts ∪
locally_modified)`. The daemon advances its baseline (§8.2) and the index is
rewritten to point at the new tree with clean paths still skip-worktree/virtual —
**zero blob writes for clean paths**. This is the google3-style lazy branch
switch §27 says we must not *claim* until demonstrated.

### 5.2 Requirements for the extension (§11.4 — all mandatory)

- `plain git` remains the user command (no wrapper, §1, §2).
- The repository **advertises** the extension explicitly (an `extensions.*` key);
  unaware Git versions **refuse safely** if it is required (`extensions.*` an old
  Git doesn't know → it errors rather than corrupting).
- The patch is **isolated and documented**; upstreamable in shape.
- **The correctness profile (A) still works with unmodified upstream Git.** D is
  strictly additive; removing the extension degrades to A's "potentially eager"
  behavior, never to incorrectness.
- We never ship a private fork while claiming upstream compatibility (§11.4).

### 5.3 Why D is last

It is the only profile requiring a Git change, hence the highest cost and the
slowest to land upstream. A–C exhaust what *unmodified* Git allows; D is the
escape hatch when measurements (Experiment G, §39) prove unmodified Git cannot be
lazy on branch transitions. The decision to pursue D is itself a measured outcome
(§27, §42 M6), recorded in an ADR.

---

## 6. Checkout / switch / rebase eagerness measurement plan (§27, Experiment G)

This is the experiment that *selects* among A–D. Build a branch delta over
**100,000 files** (§39 Experiment G) and, for `switch`, `checkout`, `reset
--hard`, `merge`, `rebase`, record the §27 vector:

```rust
/// One measured branch-transition run (spec §27). Emitted as JSON; the
/// compatibility report (§3, §40.3) is generated from these.
pub struct EagernessSample {
    pub command: String,           // "switch" | "checkout" | "reset --hard" | …
    pub profile: Profile,          // A | B | C | BplusC | D
    pub changed_paths: u64,        // size of the tree delta
    pub tree_objects_read: u64,
    pub blob_objects_fetched: u64, // the headline number (§27)
    pub bytes_fetched: u64,
    pub fuse_writes: u64,          // paths Git actually wrote through FUSE
    pub paths_materialized: u64,
    pub index_entries_expanded: u64, // Profile C force-expansion (§4.2)
    pub wall_time_ms: u64,
    pub peak_rss_bytes: u64,
}
```

Instrumentation source: `glm_object_provider::MetricsSnapshot`
(`crates/object-provider/src/metrics.rs`) for fetched objects/bytes, FUSE-adapter
write counters (`crates/fs-fuse/src/adapter.rs`) for `fuse_writes`, and the index
cache (§1.1) for `index_entries_expanded`.

**Decision rule (recorded as ADR):**

```
if Profile A eagerness ≤ perf budget for target repos:
    ship A; label branch transitions "correct, potentially eager" (§3.2, §27)
elif B and/or C reduce blob_objects_fetched to O(materialized) AND pass §3.2/§3.3:
    ship the cheapest passing profile; label "measured lazy"
else:
    pursue Profile D (§5); until landed, A remains the shipped correctness profile
```

A release **may** be stock-Git compatible while labeling branch transitions
"potentially eager"; it **must not** claim google3-style lazy switching until
demonstrated (§27).

---

## 7. Cross-cutting invariants (regression tests for this area)

These hold for whichever profile ships and become the regression suite gating
Milestone 6 (§42 M6, §43, §44):

1. **Single stage.** The only stage is `$GIT_DIR/index`; no JSON delta, no second
   index (§4.2, §44 "custom stage differs from .git/index"). The deleted
   `crates/stage` and `interop.rs` skip-worktree bridge stay deleted.
2. **No assume-unchanged.** No profile uses `CE_VALID` as a skip substitute
   (§4.4). Test: index parse asserts `assume_unchanged == false` everywhere.
3. **Index-only ops are projection-invisible.** `reset --mixed`, `restore
   --staged`, `rm --cached` change the index, never baseline+overlay bytes
   (§8.1, §25.1, §43 items 19–20).
4. **Mount fetches zero working blobs** to project the tree (§38.1); index build
   reads trees only.
5. **Clean status post-bootstrap fetches zero blobs, runs zero smudge filters,
   and does not stat every projected file** (§38.4, §12.2).
6. **Differential equality.** Mounted `status`/`diff`/`ls-files --stage`/resulting
   trees match a conventional checkout at the same commit (§40.1, §40.3).
7. **Conflict stages live in the real index.** Stages 1/2/3 are read from
   `$GIT_DIR/index`, conflict-marker files in the overlay; no custom conflict DB
   is authoritative (§25.3).
8. **Bootstrap hashes no working-tree contents** (§12.2): FSMonitor-valid bits set
   without reading blobs.
9. **Eagerness is reported, not hidden.** Every branch-transition test emits an
   `EagernessSample`; the compatibility report carries the laziness dimension
   (§3.2, §27).
10. **Profile A stands alone.** All of 1–9 pass with Profile A and **no**
    skip-worktree/sparse/extension (§4.4, §11.1) — the correctness baseline.

---

## 8. What is reusable vs. superseded (grounding)

| Existing code | Disposition |
|---------------|-------------|
| `crates/core/src/path.rs` `RepoPath` | **reuse** — byte-exact index entry paths (§31) |
| `crates/git-store/src/store.rs` `GitStore` | **reuse** — drives `read-tree`/`update-index`/`status`; add `bootstrap_index_profile_a`, `IndexReader` |
| `crates/git-store/src/batch.rs` `BatchSession` | **reuse** — residency authority during index build (§19) |
| `crates/object-provider` metrics | **reuse** — backs `EagernessSample` + budget asserts |
| `crates/git-store/src/interop.rs` (skip-worktree bridge, commit adoption) | **superseded** (§4.2, §4.4) — delete; D replaces its intent properly |
| `crates/stage` (JSON staged delta) | **superseded** (§4.2) — delete; the real index is the only stage |
| `crates/fsmonitor` (`Mutex<Vec<>>` journal) | **rework** — must be durable (§4.10, §12.1); the FSMonitor-valid bootstrap (§2.2) depends on a real token |
| `crates/workspace/src/status.rs` (three-tree XY) | **rework** — status comes from stock Git porcelain, not a re-implementation (§25) |

Detailed FSMonitor durability/token design is out of scope here; see
`docs/redesign/fsmonitor.md` (§12). This doc owns only the index strategy and the
A–D selection gate.
