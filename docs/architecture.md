# How it works: architecture overview

A tour of how `git-lazy-mount` turns a partial clone into a transparent working
tree that stock `git`, your editor, and your build drive directly. The
per-area deep-dives are linked from here. The full specification is
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

The daemon's parsed views of Git state are disposable caches, rebuilt from the
real gitdir. We never mirror Git state into a second authoritative model, never
import commits after Git exits, and never keep a second stage or branch DB.

## Working-tree model: baseline + overlay

```
working tree(path) =
  1. synthetic entry (root .git gitfile)        (reserved, protected)
  2. overlay file / dir / symlink               (local writes)
  3. overlay tombstone                          (deletions)
  4. overlay rename / subtree mapping
  5. baseline Git tree entry (HEAD tree)        (lazy, unmaterialized)
  6. absent
```

Initial: `baseline = checked-out commit tree`, `overlay = empty`. The baseline
answers "what would this unmaterialized path contain", not what is staged /
HEAD / the branch. Those come from Git. Baseline advances only after a command
is known to have updated the working tree. Index-only ops (`reset
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
`mounted` only after a kernel mount and the Git health checks pass.

## FUSE projection

- Stable inode table (generation, namespace identity, link/handle/lookup counts,
  deleted-but-open). Root `.git` has a reserved inode. Open handles are real,
  not path lookups.
- `readdir` returns names + d_type only, never sizes or blob reads.
- Real handle table for open/create/read/write/flush/fsync/release/opendir; CoW
  on first writable open; `O_TRUNC` seeds an empty overlay file (no baseline
  fetch); streaming/FD-based content (no full-blob `Vec<u8>`); open-unlink,
  rename-while-open.
- Bounded executor (separate metadata / local-IO / decompress / filter /
  network pools), backpressure, cancellation. Never one thread per callback.
  `readdir` and other non-faulting metadata callbacks run on a fast metadata
  pool while object-IO callbacks use the main pool, so `ls` stays responsive
  even while reads are hydrating blobs.

## Deadlock invariants

Git runs *inside* the mount and triggers callbacks; callbacks need objects.
Therefore: FUSE callbacks never run Git porcelain or worktree-scanning
commands, never wait on the requesting process's index lock; object readers hit
the native gitdir directly via long-lived `cat-file --batch` with
`GIT_NO_LAZY_FETCH=1`; only the dedicated fetch scheduler causes network;
all session FDs are CLOEXEC and not inherited by children.

## FSMonitor v2 + hook chaining

A tiny hook (`git-lazy-mount-fsmonitor`) is a thin IPC client to the daemon,
which serves the durable FSMonitor protocol (token = workspace + journal epoch +
monotonic seq + projection generation; inclusive responses; `/`
full-invalidation on any discontinuity). The hook reads a durable change journal
the daemon writes synchronously, so change detection is correct (no false
negatives) and git can skip the redundant full-tree stat scan on subsequent
clean statuses.

The first clean `git status` fetches zero blobs by pre-seeding the FSMonitor
index extension at bootstrap (right after read-tree, in
`AdminRepo::seed_fsmonitor_valid`): every entry is marked `CE_FSMONITOR_VALID`
carrying the journal's seq-0 token. A freshly read-tree'd index carries no
FSMonitor extension, so without the seed git's "mark all entries valid" pass
never runs on the first status and git stats (and so faults) every entry; the
seed fixes that bootstrap ordering. With the seed in place, git's
`refresh_cache_ent` early-returns on `CE_FSMONITOR_VALID` before any `lstat`, so
the first clean `status` faults zero blobs and the hook answers "nothing
changed" at the seq-0 token; subsequent clean statuses stay zero-blob. Two
carve-outs keep this correct: paths under a checkout conversion
(`filter`/`ident`/`working-tree-encoding`/CRLF `eol`) are excluded from the seed
so git checks them normally and never hides a diff, and the seeded token must
match the hook's identity (else git safely falls back to the eager scan).
Notification hooks (post-index-change,
reference-transaction, post-checkout/merge/commit/rewrite) are *multiplexed* with
the user's existing hooks, never replacing them.

## Crate structure: built behind the transparent design

```
crates/
  cli/ daemon/ ipc/ git-repo/ git-hooks/ worktree/ namespace/ overlay/
  object-provider/ filtered-cache/ fsmonitor/ fuse/ platform/ testkit/
```

The existing repo's `fs-fuse` fuser adapter, `object-provider` streaming, and
the CLOEXEC fd-hardening are reusable substrate. The custom `stage`,
`workspace` (custom branch/commit), and the `git lazy-mount git --` bridge are
superseded, and we remove them as the transparent path replaces them.

## Milestone plan: Linux only, real-mount tested in CI

Shipped (M0–M6), all real-`/dev/fuse`-CI tested:

- **M0** architecture + requirements checklist + a first vertical
  slice (transparent read-only mount, stock `git rev-parse`, zero-hydration
  readdir, lazy read).
- **M1** read-only vertical slice (one-command clone+daemon+mount).
- **M2** writable semantics (real handles, CoW, rename, open-unlink, durable
  overlay, recovery).
- **M3** stock status/staging/commit via the real index + durable FSMonitor v2.
- **M4** branch-changing workflows (switch/reset/merge/rebase/…); a branch
  switch over an M-of-N delta is measured to touch O(M) blobs, bounded by
  the delta, not O(N) the repo.
- **M5** remote + maintenance (fetch/pull/push/gc/…); offline; credential recovery.
- **M6** large-repo index strategy chosen from measurements (Profiles A–D).
  Large-file reads are bounded-memory: reading a 64 MiB baseline blob grows
  daemon RSS by ~2 MiB, not 64 MiB (streamed `cat-file` → cache → `pread`).

The transparent mount produces a FUSE working tree byte-identical to a normal
checkout that stock git, editors, and builds drive directly. The full git
command surface, including apply, am, notes, replace, cherry-pick (incl.
ranges), revert, rebase --continue, pull --rebase, grep, blame, bisect,
tag/describe/archive, clean, restore/--staged, fsck/gc/repack/maintenance, and
worktree add, is classified correct through real mounts.

Genuinely deferred (still future):

- **M7** shared object cache across workspaces.
- Submodules (partial; some tests `#[ignore]`'d) and LFS end-to-end (bounded by
  the smudge-side raw-baseline behavior below; needs a separate git-lfs/server
  integration).
- **M8** other platforms. Windows (ProjFS) and macOS (FSKit) are out of scope;
  their notes live under `future-platforms/`.

### By-design behaviors (not bugs, not TODOs)

- **Whole-directory / subtree rename is metadata-only**: an overlay re-key plus
  baseline base-refs, no blob fetch. (A clean *rename* fetching zero blobs is
  correct; the first clean *status* is likewise zero-blob via the seeded
  FSMonitor extension.)
- **`getattr` size hydration is fundamental to `blob:none`.** The exact size of
  an unmaterialized blob requires fetching it, so `ls -l` / `stat` faults each
  blob once. (This is separate from `git status`, which no longer stats seeded
  entries and faults zero blobs.) Not closeable without a server-side size
  manifest.
- **Content-file retention is correct via Linux fd survival.** An
  unlinked-but-open inode persists until the last fd closes.
- **Smudge-side `.gitattributes` / LFS serve the raw baseline blob.** A
  smudge-filtered file (`eol=crlf`, `ident`, an LFS pointer) reads as its stored
  bytes, not the smudged bytes; commits stay byte-correct because the clean
  filter is the inverse. Not closeable without filter-aware lazy sizing.

Priority order: stock-Git correctness → user-data durability → filesystem
correctness → transparent UX → measured laziness → large-repo perf → sharing.
