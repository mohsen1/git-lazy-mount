# Git state model: what Git owns, what the daemon caches

This is part of the broader [specification](design.md). It rests on two
principles: Git is authoritative for repository state, and stock Git index
behavior is preserved. That means a baseline+overlay working tree, no second
stage/branch/commit model, and observation of the gitdir without replacing
Git. Overview: [`architecture.md`](./architecture.md). This doc is the
two-source-of-truth analysis and the real-index integration plan.

This is a *design*, not a refactor. The mechanisms this doc replaces are
named explicitly in [What this supersedes](#9-what-this-supersedes-in-the-existing-tree). Do
not preserve them.

---

## 1. The one invariant

> **INV-OWNERSHIP.** Git owns every piece of repository state. The daemon
> owns only the *virtual working-tree bytes* (baseline + overlay). The
> daemon's view of any Git-owned state is a **disposable cache** keyed by a
> checksum/generation and **rebuilt from the real gitdir**. The daemon
> **never** writes a second authoritative copy of Git state and **never**
> infers Git state into a parallel model.

Three corollaries, each an anti-claim and a regression test:

- **No second stage.** There is exactly one stage: the real
  `$GIT_DIR/index`. The daemon never writes a stage; it only *reads*
  the index after Git replaces it.
- **No second branch/ref model.** HEAD, refs, reflogs, remote-tracking refs,
  and pseudorefs live only in the gitdir. The daemon never holds a
  private `workspace_head_ref` or "attached-branch lease".
- **No commit import / adoption.** A commit object exists the instant `git
  commit` writes it and advances its branch. There is no post-Git "adopt the
  commit" step.

The gitdir is a normal native directory outside FUSE, so Git uses its
ordinary `index.lock`, `packed-refs`, ref locks, reflogs, sequencer and
rebase state, and atomic renames. The daemon never synthesizes `.git`
contents through FUSE; only the root `.git` *gitfile* is synthetic.

---

## 2. State ownership matrix

Every row is Git-owned and authoritative. The "daemon cache" column is
always disposable and rebuilt from disk; an empty cache is always valid.

| Git-owned state | On-disk location (native gitdir) | Daemon cache (disposable) | Rebuild trigger |
|---|---|---|---|
| `HEAD` (attachment + tip) | `HEAD`, `refs/heads/*` | `RefSnapshot.head` | `reference-transaction` hook; mtime watch on `HEAD`/`packed-refs` |
| Local branches | `refs/heads/*`, `packed-refs` | `RefSnapshot.branches` | ref-transaction hook; watch `refs/`, `packed-refs` |
| Remote-tracking refs | `refs/remotes/*` | `RefSnapshot.remotes` | post-fetch; ref-transaction hook |
| Tags | `refs/tags/*` | `RefSnapshot.tags` | ref-transaction hook |
| Reflogs | `logs/**` | *not cached* (read on demand via `git reflog`) | n/a |
| Pseudorefs | `ORIG_HEAD`, `FETCH_HEAD`, `MERGE_HEAD`, `REBASE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `AUTO_MERGE`, `BISECT_*` | `OpState` (enum) | watch gitdir top-level + post-* hooks |
| **The index** (stage 0 + unmerged 1/2/3, modes, oids, flags) | `$GIT_DIR/index` (+ `sharedindex.*`) | `IndexCache` | atomic-replace watch on `index`; `post-index-change` hook |
| Commit creation / amend | objects + branch ref update | *none* | n/a (Git writes objects + advances refs itself) |
| History rewrite | refs + reflogs + objects | invalidate `RefSnapshot` | `post-rewrite` hook |
| Merge / rebase / cherry-pick / revert / stash / bisect | `MERGE_HEAD`, `MERGE_MSG`, `rebase-merge/`, `rebase-apply/`, `sequencer/`, `BISECT_*`, `refs/stash` | `OpState` | gitdir watch + post-* hooks |
| Push / fetch config, partial-clone filter, promisor | `config`, `remote.*` | read on demand | n/a |
| Object database | `objects/**` (+ promisor) | object/tree/filtered caches (separate docs) | content-addressed; immutable |

**Reflogs and reflog-derived answers are never cached.** They change on
every ref move and the daemon has no reason to mirror them. When a
diagnostic needs them it shells `git reflog` against the gitdir.

What the daemon owns and Git does **not** (out of scope here, see
`worktree-model.md`): the projected baseline tree id, the overlay
namespace/content, tombstones, rename mappings, synthetic entries, the
inode/handle tables, the FSMonitor journal, fetch scheduling, and the
filtered-content cache. None of these answer "what is staged / what is HEAD
/ what branch". Those answers come only from Git.

---

## 3. Reading Git state safely

All Git-state reads obey the deadlock invariants: a read issued from a
FUSE callback must never run porcelain, never scan the worktree, and never
wait on the requesting process's `index.lock`. The daemon reads Git state on
two paths:

1. **Plumbing reads against the native gitdir** using the existing
   [`GitStore`](../../crates/git-store/src/store.rs) adapter (`--git-dir`,
   `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_LAZY_FETCH=1` for
   inspection). Used for refs (`for_each_ref`), `rev-parse`, and op-state
   probes. These are cheap, lock-light, and never touch the worktree.
2. **Direct on-disk index parse.** The index is a single file replaced
   atomically by rename; the daemon mmaps/reads the new file directly. This
   avoids invoking `git ls-files` (which can take a worktree lock and is
   O(entries) per shell-out) on the hot path.

`GIT_OPTIONAL_LOCKS=0` ensures a daemon-side inspection never *takes* the
index lock and never races a user's in-flight Git command. The daemon reads
the index only when it is not locked (no sibling `index.lock`). If a lock is
present the previous cache stays valid until the replacement lands.

---

## 4. The index cache: disposable parse of `$GIT_DIR/index`

The real index is authoritative. The daemon parses it after each atomic
replacement and keeps a read-only cache. **The daemon never writes the
index to mirror its own state** (INV-OWNERSHIP). `git add`, `add -p`,
`reset`, `restore --staged`, `rm`, `rm --cached`, `mv`, and all
merge/rebase conflict-stage manipulation are performed by stock Git on that
file. The daemon observes the result.

There is no index parser in the tree today, so this section defines the
one to build (new module, e.g. `crates/git-repo/src/index.rs`). It grounds
in existing core types: [`ObjectId`](../../crates/core/src/object_id.rs),
[`GitMode`](../../crates/core/src/mode.rs),
[`RepoPath`](../../crates/core/src/path.rs), and
[`ObjectFormat`](../../crates/core/src/object_id.rs).

### 4.1 Wire format parsed (index v2/v3/v4)

The parser handles the on-disk `DIRC` format. Required coverage:

```
Header:   "DIRC" | version u32 (2|3|4) | entry_count u32
Entries:  ctime,mtime (s+ns) | dev | ino | mode | uid | gid | size(u32)
          | oid (raw, ObjectFormat.raw_len()) | flags u16 | (v3+) xflags u16
          | path bytes (NUL-terminated; v4 = prefix-compressed against prev)
Extensions (signature + size + body), at least:
  TREE  cache-tree            (subtree oids; informational)
  REUC  resolve-undo
  link  SPLIT INDEX           (shared index ref + replace/delete bitmaps)
  UNTR  untracked cache
  FSMN  fsmonitor             (last token + valid bitmap)
  sdir  SPARSE directory marker / index.sparse
Trailer:  index checksum (oid-format hash over all preceding bytes)
```

Per-entry flags the daemon extracts (the load-bearing ones):

- `stage` (2 bits of `flags`): **0** = ordinary; **1/2/3** = unmerged
  conflict stages (base/ours/theirs).
- `assume-valid` / `skip-worktree` / `intent-to-add` (`flags` + v3
  `xflags`).
- `FSMONITOR_VALID` per-entry bit (paired with the `FSMN` extension's token
  + bitmap). This is how a clean status skips the redundant full-tree stat
  scan. Git's `refresh_cache_ent` early-returns on `CE_FSMONITOR_VALID`
  *before* any `lstat`, so an entry carrying the valid bit needs neither a
  stat nor a blob fault. A freshly `read-tree`'d index, though, carries no
  `FSMN` extension, so git's "mark all entries valid" pass never runs on the
  first status and it would stat (and so fault) every entry. The daemon
  closes that gap by pre-seeding the `FSMN` extension at mount, right after
  `read-tree`, marking every entry `CE_FSMONITOR_VALID` with the journal's
  seq-0 token. With the seed in place the *first* clean status after a
  `read-tree` faults zero blobs, exactly like every later clean status; a
  zero-blob first status *is* achievable over a `blob:none` clone. Paths
  under a checkout conversion (filter / `ident` / `working-tree-encoding` /
  CRLF `eol`) are excluded from the seed so git checks them normally.
- `extended` bit (selects v3 16-bit `xflags`).

**v4 path compression** (`index.version=4`) is decoded by carrying
the previous entry's path and applying the varint strip-length + suffix.
Paths are emitted as raw bytes into `RepoPath::from_bytes`, never lossy
UTF-8. A path that fails `RepoPath` validation is surfaced as a structured
parse error, not silently dropped.

**Split index** (`link` extension): when present, the parser also
reads `$GIT_DIR/sharedindex.<oid>`, applies the replace/delete bitmaps over
the shared base, and exposes the merged entry list. The cache records both
the shared-index oid and the top index checksum.

**Sparse directory entries** (`index.sparse`): an entry with
`GitMode::Tree` and a trailing-`/` path is a collapsed subtree (a sparse
*directory* in a sparse index). The cache keeps it verbatim as an
`IndexEntryKind::SparseDir { tree_oid }` and does not expand it into
children. The product does not assume sparse-checkout rules apply, but the
parser must represent what Git wrote.

### 4.2 Cached types

```rust
/// One parsed index entry (stage 0 or an unmerged stage). Read-only.
pub struct IndexEntry {
    pub path: RepoPath,        // raw bytes; never lossy UTF-8
    pub stage: u8,             // 0 = merged; 1/2/3 = base/ours/theirs
    pub mode: GitMode,         // from the entry mode word
    pub oid: ObjectId,         // staged object (format-tagged)
    pub flags: IndexFlags,     // skip-worktree, assume-valid, intent-to-add, fsmonitor-valid
    pub stat: CachedStat,      // ctime/mtime/dev/ino/size, for racy-clean reasoning only
    pub kind: IndexEntryKind,  // Regular | SparseDir { tree_oid }
}

pub struct IndexFlags {
    pub skip_worktree: bool,
    pub assume_valid: bool,
    pub intent_to_add: bool,
    pub fsmonitor_valid: bool,
    pub extended: bool,
}

/// The whole disposable cache. `from_disk` is the only constructor.
pub struct IndexCache {
    pub checksum: ObjectId,            // index trailer hash == identity of this parse
    pub version: u8,                   // 2 | 3 | 4
    pub format: ObjectFormat,
    pub entries: Vec<IndexEntry>,      // sorted by (path, stage), Git order
    pub by_path: BTreeMap<RepoPath, SmallVec<[usize; 1]>>, // path to stage indices
    pub unmerged: BTreeMap<RepoPath, [Option<usize>; 3]>,  // conflicts: stages 1/2/3
    pub split: Option<SplitIndexRef>,  // shared index oid + applied bitmaps
    pub fsmonitor_token: Option<Vec<u8>>, // FSMN extension token, opaque bytes
    pub has_untracked_cache: bool,
    pub is_sparse: bool,
    pub generation: u64,               // daemon-local monotonic parse counter
}

pub struct SplitIndexRef { pub shared_oid: ObjectId, pub replaced: usize, pub deleted: usize }
```

Signatures the rest of the daemon depends on:

```rust
impl IndexCache {
    /// Parse the index file at `git_dir/index` from disk. The sole constructor.
    /// Returns Err(IndexParseError) on malformed bytes, never a partial cache.
    pub fn from_disk(git_dir: &Path, format: &ObjectFormat) -> Result<IndexCache>;

    /// Cheap freshness check: re-parse only if the trailer checksum changed.
    pub fn checksum_of(git_dir: &Path, format: &ObjectFormat) -> Result<Option<ObjectId>>;

    pub fn stage0(&self, p: &RepoPath) -> Option<&IndexEntry>;
    pub fn conflict(&self, p: &RepoPath) -> Option<ConflictStages<'_>>; // stages 1/2/3
    pub fn is_conflicted(&self) -> bool;                                // any unmerged entry
    pub fn iter_stage0(&self) -> impl Iterator<Item = &IndexEntry>;
}
```

### 4.3 Identity and freshness

The **index checksum** (trailer hash) is the cache's identity. Freshness
protocol on any read:

1. If no `index.lock` exists and the `index` file mtime/size is unchanged
   since the last parse, the cache is fresh. Return it (no I/O of the body).
2. Else read the trailer via `checksum_of`; if it equals
   `cache.checksum`, the cache is fresh.
3. Else `from_disk`, bump `generation`, publish the new `IndexCache`
   atomically (single `arc-swap`/`RwLock` store). The old cache is dropped.

`generation` is daemon-local (it counts parses, not Git operations) and is
folded into the FSMonitor projection generation so a re-parse can
trigger a continuity decision.

### 4.4 What the index cache is used for (read-only)

- **status fast path**: combine stage-0 entries + `skip-worktree` /
  `fsmonitor-valid` bits with the FSMonitor journal to answer "what
  changed" without re-statting every file. With the `FSMN` extension
  pre-seeded at mount (see the `FSMONITOR_VALID` note above), the first
  clean status after a `read-tree` faults zero blobs, the same as every
  clean status after it.
- **conflict projection**: the `unmerged` map tells the projection
  which paths are conflicted so the overlay's conflict-marker files line up
  with the real stages 1/2/3. The index is the *source of truth*. Any
  structured conflict metadata the daemon keeps is a reconstructable
  diagnostic cache, never the authority.
- **baseline-advance gate**: an index change *alone* never advances
  the baseline. The cache participates only as one input to the
  worktree-update detection that runs after a known checkout-like command.

### 4.5 The index-only-update rule (INV-INDEX-ONLY)

> **INV-INDEX-ONLY.** When Git changes the index without updating the
> worktree, the daemon's baseline and overlay are untouched. Only the
> `IndexCache` is re-parsed.

Canonical commands and their effect on each store:

| Command | `.git/index` | `HEAD`/refs | baseline (daemon) | overlay (daemon) |
|---|---|---|---|---|
| `git reset --mixed <c>` | reset to `<c>` tree | HEAD moves | **unchanged** | **unchanged** |
| `git restore --staged <p>` | entry to HEAD blob | none | **unchanged** | **unchanged** |
| `git rm --cached <p>` | entry removed | none | **unchanged** | **unchanged** |
| `git update-index --cacheinfo` | entry rewritten | none | **unchanged** | **unchanged** |
| `git add <p>` | entry to worktree blob | none | **unchanged** | **unchanged** (worktree bytes already in overlay) |
| `git reset --soft <c>` | **unchanged** | HEAD moves | **unchanged** | **unchanged** |
| `git commit` | stage→HEAD (no path change) | HEAD moves | **unchanged** | **unchanged** |

The rationale: if the projection sourced content from the *index*,
`reset --mixed` / `restore --staged` / `rm --cached` would corrupt or delete
working-tree files that the user never changed. The baseline answers "what
would this unmaterialized path contain in the working tree", and that is
unaffected by index-only edits. Worktree bytes change **only** when Git
writes/unlinks/renames through FUSE, which the overlay records as ordinary
filesystem operations. The daemon never infers a worktree update from a
changed index.

The complement: the baseline advances **only** after a command known to
have updated the worktree (a successful checkout/switch/reset --hard/merge
that wrote files), detected from the post-checkout/post-merge hooks plus the
overlay write stream. Never from the index alone.

---

## 5. Conflict stages live in the real index

During merge / rebase / cherry-pick / revert:

- **Stages 1/2/3 remain in `$GIT_DIR/index`.** The daemon reads them via
  `IndexCache.unmerged`. The existing
  [`MergeStage`/`MergeConflict`](../../crates/git-store/src/store.rs) types
  (already produced by `GitStore::merge_tree`) are the natural shape for
  exposing a conflicted path's three stages, but their *source of truth* is
  now the parsed index, not a `merge-tree` invocation.
- **Conflict-marker files exist in the overlay**, written by stock Git
  through FUSE during the merge. The daemon does not synthesize them.
- **`MERGE_HEAD`, `MERGE_MSG`, sequencer/rebase state remain in the
  gitdir** (`OpState`).

> **INV-CONFLICT.** The unmerged index + overlay marker files +
> gitdir op-state are the only authority for an in-progress conflict. Any
> structured conflict record the daemon caches must be fully reconstructable
> from those three. There is no custom conflict database as a source of
> truth.

`merge --abort` / `rebase --abort` / `cherry-pick --abort` are stock Git:
they rewrite the index back to stage 0 and clear the op-state. The daemon
re-parses and drops its conflict view. The daemon never aborts on Git's
behalf.

---

## 6. In-progress operation state (`OpState`)

A small enum mirroring (caching) which sequenced operation is live, derived
from gitdir top-level files. It is advisory, used for diagnostics
(`git lazy-mount doctor`/`stats`) and for deciding when baseline advancement
is plausible, and is rebuilt by scanning the gitdir.

```rust
pub enum OpState {
    Clean,
    Merge       { merge_head: Vec<ObjectId> },      // MERGE_HEAD
    Rebase      { kind: RebaseKind, onto: ObjectId },// rebase-merge/ | rebase-apply/
    CherryPick  { head: ObjectId },                  // CHERRY_PICK_HEAD
    Revert      { head: ObjectId },                  // REVERT_HEAD
    Bisect,                                           // BISECT_* present
    Sequencer   { remaining: usize },                // sequencer/todo
}
pub enum RebaseKind { Merge, Apply, Interactive }

impl OpState {
    /// Rebuild by scanning the gitdir top level. Never authoritative; cheap.
    pub fn from_gitdir(git_dir: &Path) -> Result<OpState>;
}
```

`OpState` is refreshed by the gitdir watcher on changes to
`MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `REBASE_HEAD`,
`rebase-merge/`, `rebase-apply/`, `sequencer/`, `BISECT_*`, plus the
`post-merge`/`post-rewrite`/`reference-transaction` hooks. On daemon
restart it is reconciled from disk.

---

## 7. Refs are read, never mirrored

The daemon keeps a `RefSnapshot` purely to answer diagnostics and to detect
HEAD/branch movement for baseline-advance reasoning. It is rebuilt from
`for_each_ref` + `rev-parse HEAD` against the gitdir.

```rust
pub struct RefSnapshot {
    pub head: HeadState,                              // attached(branch) | detached(oid)
    pub branches: BTreeMap<String, ObjectId>,         // refs/heads/*
    pub remotes: BTreeMap<String, ObjectId>,          // refs/remotes/*
    pub tags: BTreeMap<String, ObjectId>,             // refs/tags/*
    pub generation: u64,
}
pub enum HeadState { Attached { branch: String, tip: Option<ObjectId> }, Detached(ObjectId), Unborn }
```

The daemon never writes refs, never holds a private head ref, never
performs ref CAS to "publish" a workspace commit, and never adopts a commit
created elsewhere. Plain `git commit` / `rebase` / `push` update refs
directly; the daemon learns via the `reference-transaction` hook and
re-snapshots. Refresh is also triggered by mtime watches on
`HEAD`/`packed-refs`/`refs/`.

> **INV-REFS-READONLY.** No daemon code path issues `update-ref`,
> `symbolic-ref`, `commit-tree`+ref-publish, or `push` to represent
> workspace state. (The fetch scheduler's `git fetch` is the one network
> entry point, and it is Git updating its own remote-tracking refs.)

---

## 8. Synchronization with Git

Caches are kept warm by two cooperating mechanisms. Neither is required
for correctness: on any gap the daemon reconciles from disk.

1. **Notification hooks** (multiplexed with user hooks):
   `post-index-change` re-parses `IndexCache`; `reference-transaction`,
   `post-checkout`, `post-merge`, `post-commit`, `post-rewrite`, and
   `post-applypatch` refresh `RefSnapshot`/`OpState` and feed
   baseline-advance detection. Hooks send a *bounded* notification to the
   daemon over IPC and then run the user's previous hook unchanged. They
   never hold daemon locks while the user hook runs and never alter the
   user hook's exit status.
2. **Gitdir watcher**: mtime/inotify on `index`, `index.lock`, `HEAD`,
   `packed-refs`, `refs/`, `logs/`, and the op-state files. Catches
   changes from Git invoked outside the mount or when a hook is absent.

On daemon restart or any missed event, the daemon re-derives `IndexCache`,
`RefSnapshot`, and `OpState` from disk and, if continuity cannot be proven,
returns the FSMonitor full-invalidation path.

---

## 9. What this supersedes in the existing tree

The design removes these mechanisms. They are listed so they are not
reused:

- **Custom stage.** [`crates/stage/src/lib.rs`](../../crates/stage/src/lib.rs),
  a JSON `index.json` of `StagedChange::{Set,Remove,IntentToAdd}`. This is
  the "second staging database" the design forbids. **Replaced by**
  read-only `IndexCache` over the real `$GIT_DIR/index`.
- **The `git lazy-mount git --` bridge.**
  [`crates/git-store/src/interop.rs`](../../crates/git-store/src/interop.rs)
  stands up a throwaway operational gitdir, routes object I/O via
  `GIT_OBJECT_DIRECTORY`, synthesizes an index from the staged tree with
  every entry marked skip-worktree, and reads back `bridge_head` to *adopt*
  the commit. This is exactly the per-command disposable gitdir, the
  commit-adoption step, and the skip-worktree-as-universal-trick the design
  forbids. **Replaced by** stock Git operating directly on the real worktree
  via the synthetic `.git` gitfile; the daemon only *observes* the resulting
  index/refs.
- **Custom branch/merge state in the workspace.**
  [`crates/workspace/src/lib.rs`](../../crates/workspace/src/lib.rs):
  `WorkspaceConfig.workspace_head_ref`, `attached_branch`, the
  `base`/`attached_expected`/`merge_head` mutexes, and the `commit_tree`+CAS
  publish path. This is the second authoritative branch model and the
  in-process merge state. **Replaced by** the read-only `RefSnapshot` +
  `OpState`; Git owns HEAD, branches, and `MERGE_HEAD`.

Reusable as-is: [`GitStore`](../../crates/git-store/src/store.rs) and its
`BatchSession` (long-lived `cat-file --batch`), the core types
(`ObjectId`/`GitMode`/`RepoPath`/`ObjectFormat`/`TreeEntry`), and the
`MergeStage`/`MergeConflict` shapes (now sourced from the parsed index, not
`merge-tree`). The original in-memory `Mutex<Vec<_>>` `glm-fsmonitor`
([`crates/fsmonitor/src/lib.rs`](../../crates/fsmonitor/src/lib.rs)) has been
replaced by a durable change journal that the daemon writes synchronously and
the `git-lazy-mount-fsmonitor` hook reads. It is wired to `core.fsmonitor`
and covered in `fsmonitor.md`. It delivers correct change detection (no false
negatives) and lets a subsequent clean status skip the full-tree stat scan.

---

## 10. Testable invariants (regression tests)

Each maps to an anti-claim and/or a release criterion. Run via
differential tests vs a conventional checkout through a real mount.

- **T-OWN-1 (INV-OWNERSHIP).** Delete the daemon's entire cache
  (`IndexCache`/`RefSnapshot`/`OpState`), restart, and reach byte-identical
  projected state + identical `status --porcelain=v2`. The cache is provably
  disposable.
- **T-IDX-1.** For a hand-built index containing v2, v3, and v4 entries,
  split-index, conflict stages 1/2/3, `skip-worktree`, `assume-valid`,
  `intent-to-add`, `fsmonitor-valid`, and a sparse-dir entry, `IndexCache`
  round-trips every field and matches `git ls-files --stage` /
  `ls-files -v` / `ls-files -t`.
- **T-IDX-2 (split index).** With `git update-index --split-index`, the
  cache resolves the shared base + bitmaps and produces the same entry set
  as a non-split index.
- **T-IDX-3 (checksum identity).** Re-parse is skipped when the trailer
  checksum is unchanged and forced when it changes; `generation` advances
  exactly once per real replacement.
- **T-IDX-4 (non-UTF-8).** An index entry whose path is invalid UTF-8
  (e.g. `0xFF` byte, embedded newline) parses to the exact `RepoPath` bytes
  with no lossy conversion.
- **T-ONLY-1 (INV-INDEX-ONLY, release crit. 20).** `git reset --mixed
  HEAD~1` changes the index but leaves every projected working-tree byte and
  every overlay entry identical; `cat`/sha of each path is unchanged.
- **T-ONLY-2 (release crit. 19).** `git rm --cached <p>` removes the index
  entry but the working-tree file still reads its prior bytes.
- **T-ONLY-3.** `git restore --staged <p>` leaves baseline+overlay
  untouched.
- **T-CONFLICT-1 (INV-CONFLICT, release crit. 16).** After a conflicting
  `git merge`, `IndexCache.unmerged` lists stages 1/2/3 matching
  `ls-files -u`, overlay marker files match a conventional checkout, and
  `MERGE_HEAD` is in the gitdir. Dropping the daemon's conflict cache and
  rebuilding reproduces the same view.
- **T-CONFLICT-2 (release crit. 17).** `merge --abort` / `rebase --abort`
  returns the index to stage 0 and the daemon's conflict view clears with no
  daemon-initiated ref/index write.
- **T-REFS-1 (INV-REFS-READONLY).** A grep/audit asserts no daemon code path
  outside the fetch scheduler issues `update-ref`/`symbolic-ref`/`push`; a
  runtime test confirms a normal `git commit` advances `refs/heads/<b>`
  with no daemon ref write and no commit-adoption step. (release
  crit. 11, 29, 30)
- **T-OP-1.** `OpState::from_gitdir` reports the correct phase for an
  in-progress merge, rebase (apply + merge + interactive), cherry-pick,
  revert, and bisect, matching the gitdir files.
- **T-SYNC-1.** With hooks removed, the gitdir watcher alone re-derives a
  correct `IndexCache`/`RefSnapshot` after an out-of-mount Git command;
  hooks are an optimization, not a correctness dependency.
