# How it works — architecture overview

A tour of how `git-lazy-mount` turns a partial clone into a transparent working
tree that stock `git` (and your editor, and your build) drive directly. The
per-area deep-dives are linked from here; the full specification is
[`design.md`](design.md).

## The thesis: transparency

`git lazy-mount https://…/huge-repo ~/huge-repo` replaces the initial `git
clone`. Afterward, **the user's ordinary `git` and tools work with no wrapper,
alias, env, or `git lazy-mount` workflow verb**. `git lazy-mount`
retains only lifecycle/diagnostics (`unmount`, `list`, `doctor`, `stats`,
`trace`, `prefetch`, `dehydrate`, `recover`). The presence of `git lazy-mount
add|commit|switch|push|git --` means transparency has failed.

## Two sources of truth

| Owner | State |
|-------|-------|
| **Git** (the native admin gitdir) | `HEAD`, branches, refs, reflogs, remote-tracking refs, **the real `.git/index`** (the only stage), conflict stages 1/2/3, commit creation/amend, merge/rebase/cherry-pick/stash/bisect/sequencer state, tags, push/fetch config |
| **The daemon** (custom state) | only the **virtual working-tree bytes**: baseline + overlay + tombstones + synthetic entries; the FUSE projection; the change journal; fetch scheduling; filtered-content cache; inode/handle tables |

The daemon's parses of Git state are **disposable caches**, rebuilt from the
real gitdir. We never mirror Git state into a second authoritative model, never
import commits after Git exits, never keep a second stage or branch DB.

## Working-tree model — baseline + overlay

```
working tree(path) =
  1. synthetic entry (root .git gitfile)        — reserved, protected
  2. overlay file / dir / symlink               — local writes
  3. overlay tombstone                          — deletions
  4. overlay rename / subtree mapping
  5. baseline Git tree entry (HEAD tree)        — lazy, unmaterialized
  6. absent
```

Initial: `baseline = checked-out commit tree`, `overlay = empty`. The baseline
answers "what would this unmaterialized path contain", **not** what is staged /
HEAD / the branch — those come from Git. Baseline advances only after a command
is known to have updated the working tree; index-only ops (`reset
--mixed`, `restore --staged`, `rm --cached`) leave baseline+overlay untouched.

## On-disk layout

```
~/.local/share/git-lazy-mount/workspaces/<id>/
  git/              real native partial-clone admin dir  (NOT inside FUSE)
  state.sqlite      namespace DB (overlay metadata, inodes, tombstones, renames)
  overlay/          native files holding writable content
  filtered-cache/   validated working-tree-representation cache files
  journal/          FSMonitor durability log (SQLite WAL / append log)
  mount.json, logs/
```

Mounted worktree `~/huge-repo/` projects a synthetic read-only regular file
`.git` whose bytes are `gitdir: /abs/.../workspaces/<id>/git`. The admin gitdir
is configured with `core.worktree = <mountpoint>`, so stock Git resolves the
repo via the gitfile and operates on the mounted worktree. The synthetic `.git`
is protected from unlink/rename/replace/chmod/write/mkdir-beneath.

## Startup as an idempotent transaction

`preflight → create git (clone --filter=blob:none --no-checkout
--separate-git-dir) → init working-tree state (baseline, empty overlay, gen,
fsmonitor token) → init real index (O(tracked paths), 0 blobs) → configure git
(core.worktree, core.fsmonitor, …) → start+validate mount → mounted`. Lifecycle
states: `creating cloning initializing-git building-index starting-daemon
mounting validating mounted quiescing unmounting recovering failed`. Enter
`mounted` **only after** a kernel mount + Git health checks pass.

## FUSE projection

- Stable inode table (generation, namespace identity, link/handle/lookup counts,
  deleted-but-open). Root `.git` has a reserved inode. Open handles are real,
  not path lookups.
- `readdir` returns names + d_type only — **never** sizes or blob reads.
- Real handle table for open/create/read/write/flush/fsync/release/opendir; CoW
  on first writable open; `O_TRUNC` seeds an empty overlay file (no baseline
  fetch); streaming/FD-based content (no full-blob `Vec<u8>`); open-unlink,
  rename-while-open.
- Bounded executor (separate metadata / local-IO / decompress / filter /
  network pools), backpressure, cancellation — never one thread per callback.

## Deadlock invariants

Git runs *inside* the mount and triggers callbacks; callbacks need objects.
Therefore: FUSE callbacks **never** run Git porcelain or worktree-scanning
commands, never wait on the requesting process's index lock; object readers hit
the **native gitdir directly** via long-lived `cat-file --batch` with
`GIT_NO_LAZY_FETCH=1`; only the dedicated fetch scheduler causes network;
all session FDs are CLOEXEC and not inherited by children.

## FSMonitor v2 + hook chaining

A tiny hook is a thin IPC client to the daemon, which serves the durable
FSMonitor protocol (token = workspace + journal epoch + monotonic seq +
projection generation; inclusive responses; `/` full-invalidation on any
discontinuity). Bootstrap marks initial index entries FSMonitor-valid **without
hashing working-tree contents** so the first + every clean `status` fetch zero
blobs. Notification hooks (post-index-change, reference-transaction,
post-checkout/merge/commit/rewrite) are *multiplexed* with the user's existing
hooks, never replacing them.

## Crate structure — built behind the transparent design

```
crates/
  cli/ daemon/ ipc/ git-repo/ git-hooks/ worktree/ namespace/ overlay/
  object-provider/ filtered-cache/ fsmonitor/ fuse/ platform/ testkit/
```

The existing repo's `fs-fuse` fuser adapter, `object-provider` streaming, and
the CLOEXEC fd-hardening are reusable substrate; the custom `stage`,
`workspace` (custom branch/commit), and the `git lazy-mount git --` bridge are
**superseded** and removed as the transparent path replaces them.

## Milestone plan — Linux only, real-mount tested in CI

- **M0** this: architecture + requirements checklist + a first vertical
  slice (transparent read-only mount, stock `git rev-parse`, zero-hydration
  readdir, lazy read), in real `/dev/fuse` CI.
- **M1** read-only vertical slice complete (one-command clone+daemon+mount).
- **M2** writable semantics (real handles, CoW, rename, open-unlink, durable
  overlay, recovery).
- **M3** stock status/staging/commit via the real index + durable FSMonitor v2.
- **M4** branch-changing workflows (switch/reset/merge/rebase/…); measured eagerness.
- **M5** remote + maintenance (fetch/pull/push/gc/…); offline; credential recovery.
- **M6** large-repo index strategy chosen from measurements (Profiles A–D).
- **M7** optional shared object cache. **M8** other platforms (out of scope here).

Priority order: stock-Git correctness → user-data durability → filesystem
correctness → transparent UX → measured laziness → large-repo perf → sharing.
