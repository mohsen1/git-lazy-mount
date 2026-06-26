# How it works: architecture overview

A tour of how `git-lazy-mount` turns a partial clone into a transparent working
tree that stock `git`, your editor, and your build drive directly. This is the
explanation-level overview; each area has a deep-dive doc, linked inline below.
The full specification is [`design.md`](design.md).

`git-lazy-mount` is Linux-only and built on FUSE.

## The thesis: transparency

`git lazy-mount https://…/huge-repo ~/huge-repo` replaces the initial `git
clone`. Afterward, **the user's ordinary `git` and tools work with no wrapper,
alias, env, or `git lazy-mount` workflow verb**. Files materialize (hydrate) on
read or edit. `git lazy-mount` itself exposes only lifecycle/diagnostics:
`unmount` and `doctor` (plus a hidden internal `__serve` the mount flow spawns
for itself). There is deliberately **no** `add`/`commit`/`switch`/`push`/`git
--` — their presence would mean transparency had failed.

## Two sources of truth

| Owner | State |
|-------|-------|
| **Git** (the native admin gitdir) | `HEAD`, branches, refs, reflogs, remote-tracking refs, **the real `.git/index`** (the only stage), conflict stages 1/2/3, commit creation/amend, merge/rebase/cherry-pick/stash/bisect/sequencer state, tags, push/fetch config |
| **git-lazy-mount** (the projection) | only the **virtual working-tree bytes**: baseline + overlay + tombstones + the synthetic `.git`; the FUSE projection; the change journal; the content cache; inode/handle tables |

The projection's parsed views of Git state are disposable caches, rebuilt from
the real gitdir. We never mirror Git state into a second authoritative model,
never import commits, and never keep a second stage or branch DB. The boundary
itself — separate-git-dir plus `core.worktree` plus the synthetic `.git` — is
specified in [`git-state-model.md`](git-state-model.md).

## Working-tree model: baseline + overlay

```
working tree(path) =
  1. synthetic entry (root .git gitfile)        (protected)
  2. overlay file / dir / symlink               (local writes)
  3. overlay tombstone                          (deletions)
  4. overlay BaseRef / subtree mapping          (renames)
  5. baseline Git tree entry (HEAD tree)        (lazy, unmaterialized)
  6. absent
```

The `baseline_tree` object id is fixed at projection open from the HEAD tree and
is immutable for that projection's life. The baseline answers "what would this
unmaterialized path contain", not what is staged / HEAD / the branch — those
come from Git. The overlay (an `OverlayEntry` of `File` / `Symlink` / `Dir` /
`Tombstone` / `BaseRef{oid,mode}`) holds every local divergence. Resolution,
raw-byte paths, rename semantics (RENAME_NOREPLACE honored, RENAME_EXCHANGE
rejected, subtree rename metadata-only), and the protected synthetic `.git` are
owned by [`worktree-model.md`](worktree-model.md).

## On-disk layout

```
~/.local/share/git-lazy-mount/workspaces/<16-hex id>/
  git/        real native partial-clone admin gitdir  (NOT inside FUSE)
  cache/      content-addressed materialized blob cache
  overlay/    local writes: per-entry JSON sidecars (meta/) + content files (content/)
  anchor/     clone anchor directory
```

There is no SQLite anywhere. Overlay metadata is one atomic JSON sidecar per
entry — `id_for(path) = sha256(path)+".json"` under `overlay/meta/`, written
temp + fsync + rename — with content bytes in native files under
`overlay/content/`. The in-memory overlay index is a disposable cache rebuilt
from the sidecars on open. Durability details live in
[`durability-security.md`](durability-security.md).

The change journal is **not** under `workspaces/`: it is a NUL-separated append
log at `<gitdir>/glm-fsmonitor/changes.log`, replayed into an in-memory `Vec` on
open (see [`fsmonitor.md`](fsmonitor.md)).

The mounted worktree `~/huge-repo/` projects a synthetic read-only regular file
`.git` whose bytes are `gitdir: /abs/.../workspaces/<id>/git`. The admin gitdir
is configured with `core.worktree = <mountpoint>`, so stock Git resolves the
repo via the gitfile and operates on the mounted worktree. Any op on `.git`
(unlink/rename/replace/write/mkdir-beneath) is rejected — not via a reserved
inode, but by `child_path()` returning `ErrorCode::Authentication`.

## Startup sequence

`cmd_mount` is a single forward sequence (no lifecycle state-machine enum;
mount generation is just a monotonic `MountGeneration` counter):

1. **clone** — `AdminRepo::clone()`: `git clone --no-checkout --separate-git-dir
   --filter=tree:0` (adds `--no-single-branch` when `--depth` is set;
   `GIT_TERMINAL_PROMPT=0`).
2. **build index** — `build_index()` runs `git read-tree HEAD`, which faults the
   HEAD trees and **zero** blobs.
3. **configure + seed** — `configure_fsmonitor` sets only
   `core.fsmonitor=<dir-of-exe>/git-lazy-mount-fsmonitor` and
   `core.fsmonitorHookVersion=2`; `seed_first_status` opens the empty journal,
   then `seed_fsmonitor_valid` seeds the index FSMonitor extension.
4. **spawn `__serve`** — a detached hidden child process (stdio nulled,
   reparented to init, **not** waited on) holds the kernel FUSE mount after the
   parent returns. `cmd_serve` opens the `AdminRepo` + `ChangeJournal` +
   `Projection::with_journal`, then `glm_fuse::mount` blocks until unmounted.
5. **poll readiness** — the parent polls `mountpoint/.git` (up to 1000 × 10 ms),
   then runs Git health checks (`git rev-parse --show-toplevel` /
   `--is-inside-work-tree`) before printing success.

Why `tree:0` and not `blob:none` or `--depth 1`: `tree:0` fetches every commit
(so log, merge-base, and branch switching work) but no trees or blobs. A
full-history `blob:none` clone would download every tree from all history (slow
and large on big repos), and `--depth 1` grafts the commits, breaking `git
merge`/`git rebase` and hiding history. `blob:none` is still a valid explicit
`--filter` override, just not the default. The startup ordering and its
deadlock-avoidance constraints are owned by
[`deadlock-startup-recovery.md`](deadlock-startup-recovery.md).

## FUSE projection

`TransparentFs` implements `fuser::Filesystem`. See
[`fuse-semantics.md`](fuse-semantics.md) for the full op set and
[`object-fetching.md`](object-fetching.md) for hydration.

- **Inodes/handles.** `ROOT_INO=1` is the only pre-allocated inode; each
  `InodeEntry` is just `{path, lookups, generation}`. Open handles are real
  (`Handle::Read` / `Handle::Write`, `fh` from an `AtomicU64` starting at 1),
  not path lookups; read/write service strictly by `fh` via `pread`/`pwrite`
  into an FD, with no whole-file `Vec<u8>` buffering.
- **`readdir`** merges baseline tree + overlay children, O(direct children),
  returning names + d_type only — never sizes or blob reads.
- **Writes.** CoW on first writable open; `O_TRUNC` (negotiated via
  `FUSE_ATOMIC_O_TRUNC`) seeds an empty overlay file with no baseline fetch;
  open-unlink and rename-while-open work via Linux fd survival.
- **Two bounded pools** (`pool.rs`): an object-IO pool (`POOL_THREADS=16`) for
  faulting callbacks and a meta pool (`META_THREADS=4`) for `opendir`/`readdir`,
  so `ls` stays responsive while reads hydrate blobs. There are no separate
  decompress/filter/network pools and no backpressure/cancellation machinery.

## Deadlock invariants

Git runs *inside* the mount and triggers callbacks; callbacks need objects.
Therefore: FUSE callbacks never run Git porcelain or worktree-scanning commands,
and never wait on the requesting process's index lock. Object readers hit the
native gitdir directly via a long-lived `cat-file --batch-command` session with
`GIT_NO_LAZY_FETCH=1`; lazy fetches are explicit. All session FDs are CLOEXEC
(`git-store/src/proc.rs` `harden_fds`) and not inherited by children.

## FSMonitor v2 + the change journal

There is **no daemon and no IPC**. The `git-lazy-mount-fsmonitor` hook binary
(`cli/src/bin/fsmonitor_hook.rs`) reads the durable change journal file
**directly**: it resolves the gitdir, opens the `ChangeJournal`, calls
`query(prev)`, and prints the new token + NUL + changed paths. The projection's
`record()` writes each change synchronously (`write_all` + `sync_data`) before
the FUSE reply, so change detection has no false negatives.

The first clean `git status` faults **zero** blobs because
`seed_fsmonitor_valid` pre-marks every index entry `CE_FSMONITOR_VALID` at the
seq-0 token: git's `refresh_cache_ent` early-returns before any `lstat`, and the
hook answers "nothing changed". The seed is **skipped wholesale** if any tracked
`.gitattributes` declares a conversion attr (`filter=` / `ident` /
`working-tree-encoding=` / CRLF `eol`), so converted paths are never hidden from
a diff. The token form, full-invalidation rules, and the seed mechanics are
owned by [`fsmonitor.md`](fsmonitor.md) — the canonical home for the seed.

## Crates

A single Cargo workspace of nine crates (package names in parens):

```
crates/
  core        (glm-core)       backend-agnostic vocabulary: ObjectId, GitMode,
                               RepoPath, per-path state axes, FetchPolicy, Error/ErrorCode
  git-store   (glm-git-store)  git-CLI object access: cat-file --batch-command, smudge_blob,
                               CLOEXEC fd hardening, tree parsing, interop index bridge
  git-repo    (glm-git-repo)   AdminRepo: clone (tree:0, no-checkout, separate-git-dir),
                               build_index (read-tree HEAD), seed_fsmonitor_valid
  worktree    (glm-worktree)   the Projection: baseline + overlay + ChangeJournal,
                               resolve/readdir/rename/materialize
  fs-common   (glm-fs-common)  InodeTable (ROOT_INO + dynamic inodes)
  fuse        (glm-fuse)       TransparentFs (fuser::Filesystem), two bounded pools
  cli         (glm-cli)        the git-lazy-mount binary + git-lazy-mount-fsmonitor hook
  sgrep       (sgrep)          standalone remote-grep CLI; overlays edits without fetching
  testkit     (glm-testkit)    shared test helpers
```

`git-store/src/interop.rs` synthesizes a throwaway operational index (every
entry skip-worktree) so stock git can run against the shared store; it is off
the mount hot path but still exercised by `store_integration.rs`. Index strategy
detail is in [`index-strategy.md`](index-strategy.md).

## Status

Linux-only, real-`/dev/fuse`-CI tested on `ubuntu-latest`. Not supported: a
shared object cache across workspaces, full submodule support, and end-to-end
LFS (bounded by the smudge-side raw-baseline behavior). Windows (ProjFS) and
macOS (FSKit) backend notes live under
[`future-platforms/`](future-platforms/).

Per-command compatibility and the laziness matrix live in
[`compatibility.md`](compatibility.md); the by-design / fundamental / deferred
register (including the metadata-only subtree rename, `getattr` size hydration,
and smudge-side raw-baseline reads) lives in [`limitations.md`](limitations.md).
