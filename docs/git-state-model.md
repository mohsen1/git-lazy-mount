# Git state model: what Git owns, what the mount caches

This is part of the broader [specification](design.md). Overview:
[`architecture.md`](./architecture.md). This doc explains the **ownership
boundary** between stock Git and the mount, and why the transparent
design needs no daemon-side copy of Git state to honor it.

---

## 1. The one invariant

> **INV-OWNERSHIP.** Git owns every piece of repository state — the index,
> HEAD, refs, reflogs, the object database, and in-progress operation state
> (merge/rebase/cherry-pick/bisect). The mount owns only the *virtual
> working-tree bytes* (baseline + overlay). Any view the mount holds of
> Git-owned state is a **disposable cache** rebuilt from the real gitdir; the
> mount **never** writes a second authoritative copy and **never** infers Git
> state into a parallel model.

Three corollaries:

- **No second stage.** There is exactly one stage: the real
  `$GIT_DIR/index`. The mount never writes a stage.
- **No second branch/ref model.** HEAD, refs, reflogs, remote-tracking refs,
  and pseudorefs live only in the gitdir. The mount holds no private
  `workspace_head_ref` or "attached-branch lease".
- **No commit import / adoption.** A commit object exists the instant `git
  commit` writes it and advances its branch. There is no post-Git "adopt the
  commit" step.

---

## 2. How the transparent design satisfies the boundary

The mount realizes INV-OWNERSHIP not by mirroring Git state into caches, but
by *removing the need to*. Stock Git drives the real gitdir directly:

- The clone is `git clone --no-checkout --separate-git-dir=<gitdir>`
  ([`AdminRepo::clone`](../crates/git-repo/src/lib.rs)), so the gitdir is a
  normal **native** directory outside FUSE.
- `core.worktree` is set to the mountpoint, and the FUSE projection serves a
  single synthetic `.git` *gitfile* at the mount root pointing back at that
  gitdir.

The result: when a user runs `git add`, `git commit`, `git reset`, `git
merge`, or `git switch` inside the mount, **stock Git reads and writes its own
index, refs, reflogs, locks, and sequencer/rebase state in the native gitdir
directly.** Git uses its ordinary `index.lock`, `packed-refs`, ref locks, and
atomic renames. The mount never parses or caches the index, never snapshots
refs, and never models in-progress operations — there is nothing for the mount
to keep consistent because Git is the only writer and the only reader of that
state.

The mount synthesizes nothing under `.git`; only the root `.git` gitfile is
synthetic, and any operation that tries to traverse into it is rejected
(`child_path` returns `ErrorCode::Authentication`).

### What the mount owns (and Git does not)

The projected baseline tree id, the overlay (content, tombstones, BaseRefs,
rename mappings), the synthetic `.git` gitfile, the inode/handle tables, the
FSMonitor change journal, and the content cache. None of these answer "what is
staged / what is HEAD / what branch" — those answers come only from Git. The
overlay and baseline are themselves disposable: the overlay's in-memory index
is rebuilt from atomic sidecars on open, and the journal replays from a durable
append log. Their ownership semantics are detailed in
[`worktree-model.md`](./worktree-model.md).

The one place the mount observes Git's index is the **FSMonitor seed**: right
after `git read-tree HEAD` builds the index, the mount writes the index's
`FSMN` extension so the first clean `git status` faults zero blobs (the same as
every later clean status). This is a write *into Git's own index extension for
Git's benefit*, not a daemon-side cache of index contents. It is the canonical
subject of [`fsmonitor.md`](./fsmonitor.md); a zero-blob first status is
achievable over the default `tree:0` clone. Paths under a checkout conversion
(filter / `ident` / `working-tree-encoding` / CRLF `eol`) are excluded from the
seed so Git checks them normally.

---

## 3. Index-only commands do not touch the worktree

Because Git owns the index and the mount owns the worktree bytes separately, a
command that rewrites the index without rewriting the worktree leaves the
mount's baseline and overlay untouched:

| Command | `.git/index` | HEAD/refs | baseline | overlay |
|---|---|---|---|---|
| `git reset --mixed <c>` | reset to `<c>` tree | HEAD moves | unchanged | unchanged |
| `git restore --staged <p>` | entry to HEAD blob | none | unchanged | unchanged |
| `git rm --cached <p>` | entry removed | none | unchanged | unchanged |
| `git add <p>` | entry to worktree blob | none | unchanged | unchanged |
| `git reset --soft <c>` | unchanged | HEAD moves | unchanged | unchanged |
| `git commit` | stage→HEAD | HEAD moves | unchanged | unchanged |

Worktree bytes change **only** when Git writes, unlinks, or renames a path
*through FUSE*, which the overlay records as an ordinary filesystem operation.
The mount never infers a worktree update from a changed index. Branch-changing
commands (`switch`/`checkout`/`reset --hard`/`merge`/`rebase`) are correct but
potentially eager: stock Git writes each changed path through the FUSE write
path, bounded by the delta, not the repo size.

Conflict stages 1/2/3 likewise live in the real `$GIT_DIR/index`, and
conflict-marker files are written by stock Git through FUSE into the overlay.
The mount does not synthesize either; `merge --abort` is stock Git rewriting
its own index back to stage 0.

---

## 4. Stock Git owns all repository state

Stock Git owns all repository state through the real gitdir (§2): the index,
HEAD, refs, reflogs, the object database, and in-progress operation state. The
projection keeps no Git-state cache — no daemon-side index parser, no ref
snapshot, no in-progress-operation model, and no gitdir watcher. The only Git
hook configured is `core.fsmonitor` (with `core.fsmonitorHookVersion=2`),
pointing at the `git-lazy-mount-fsmonitor` binary
([`configure_fsmonitor`](../crates/cli/src/main.rs)).

The change journal that backs that hook is the durable
[`ChangeJournal`](../crates/worktree/src/journal.rs): the mount writes it
synchronously before each FUSE reply and the `git-lazy-mount-fsmonitor` hook
reads it; see [`fsmonitor.md`](./fsmonitor.md).

### The interop bridge

[`crates/git-store/src/interop.rs`](../crates/git-store/src/interop.rs) stands
up a throwaway operational gitdir, routes object I/O via
`GIT_OBJECT_DIRECTORY`, synthesizes an index from a tree with every entry
marked skip-worktree, and reads back the resulting head. It is **not** on the
mount hot path, but the module is `pub`-exported (`InteropOutcome`) and
exercised by the `interop_bridge_status_commit_and_lazy_fetch` integration test
in [`store_integration.rs`](../crates/git-store/tests/store_integration.rs).

### Shared object-store components

[`GitStore`](../crates/git-store/src/store.rs) and its `BatchSession`
(long-lived `cat-file --batch-command`), the core types
([`ObjectId`](../crates/core/src/object_id.rs),
[`GitMode`](../crates/core/src/mode.rs),
[`RepoPath`](../crates/core/src/path.rs)), and the `MergeStage`/`MergeConflict`
shapes in `store.rs`.

---

See [`index-strategy.md`](./index-strategy.md) for the real index build
(`read-tree HEAD`) and the interop bridge's synthesized index in detail, and
[`worktree-model.md`](./worktree-model.md) for baseline/overlay/tombstone/rename
ownership.
