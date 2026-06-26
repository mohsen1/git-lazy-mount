# `git-lazy-mount`: design specification

`git-lazy-mount` is a **Linux-only** FUSE filesystem that lazily mounts a Git
repository without a full clone. One command —

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

— partial-clones the repo, mounts a transparent virtual working tree, validates
it, and returns. Afterward **stock Git, editors, and builds work directly**, with
no wrapper, alias, environment activation, or `git lazy-mount` workflow verb.
Files materialize (hydrate) on read or edit.

This is the authoritative specification. It is written to match the shipped code;
where it summarizes a subsystem, the linked area document under [`docs/`](.) owns
the implementation detail. macOS (FSKit) and Windows (ProjFS) backends were
explored and retired — their notes survive only as roads not taken under
[`future-platforms/`](future-platforms/).

The design is deliberately clean rather than an incremental refactor: there is no
custom stage, no custom branch/commit state, no commit-adoption bridge, and no
headless-first architecture. The real `.git/index` is the only stage; stock Git
owns refs, HEAD, commits, merges, and conflict stages.

The executable is named `git-lazy-mount`, so Git exposes it as `git lazy-mount`.

---

# 1. Product contract

The primary command replaces the initial `git clone` for one working copy:

```bash
git lazy-mount https://github.com/example/huge-repo ~/huge-repo
```

It must not return successfully until:

1. the partial Git repository exists;
2. the virtual filesystem is mounted;
3. the projected root is readable;
4. the synthetic `.git` entry resolves correctly;
5. stock Git recognizes the repository;
6. the FSMonitor integration is live;
7. a basic Git health check succeeds.

After the command returns, the following must use the user's ordinary Git
executable with no wrapper, shell alias, environment activation, or workflow verb:

```bash
cd ~/huge-repo
ls; cat README.md; $EDITOR src/main.rs
git status; git diff; git add -p; git commit -m "…"; git commit --amend
git log; git branch; git switch feature; git checkout other
git merge topic; git rebase main; git stash
git fetch; git pull; git push
cargo build; make; rg 'pattern'
```

Running an unrelated command such as `git clone …` from inside the mount must
behave like ordinary Git.

## 1.1 The `git lazy-mount` verb surface

`git lazy-mount` itself stays out of the daily workflow. The shipped subcommands
are limited to lifecycle and diagnostics:

```bash
git lazy-mount <url> <path>        # mount (primary form)
git lazy-mount unmount <path>      # release the kernel mount
git lazy-mount doctor <path>       # report mountpoint / mounted / show-toplevel (--json)
```

There is also one hidden, internal subcommand, `__serve`, spawned by the mount
flow to hold the kernel mount after the parent command returns (see
[§5.5](#55-command-surface)). It is not a user-facing command.

Providing alternative workflow commands such as `git lazy-mount add | commit |
branch | switch | push | git --` would mean transparency had failed; they do not
exist.

---

# 2. Definition of "stock Git works"

The compatibility target is an unmodified, upstream Git executable discovered from
the user's `PATH`.

Allowed integration points:

```text
a standard .git gitfile
normal Git configuration, hooks, refs, and reflogs
core.fsmonitor
the real Git index
normal partial-clone configuration
normal alternates (a possible later optimization)
```

Not allowed as the product design:

```text
shadowing or replacing the git executable
shell aliases for built-in Git commands
a required command wrapper
LD_PRELOAD interception
process-specific filesystem lies
a disposable Git repository per command
importing or "adopting" commits after Git exits
a second staging database
a second authoritative branch database
a second implementation of Git commit semantics
```

The filesystem exposes the same namespace and contents to Git, editors, builds,
and ordinary applications. Results never depend on the caller's process name.

---

# 3. Two separate success dimensions

Every Git command is classified along two independent axes.

**Compatibility:** `correct` / `partially correct` / `unsupported`. Correct means
exit status, stdout/stderr, HEAD, refs, reflogs, index, pseudorefs, working-tree
contents, conflicts, hooks, and resulting commits match a normal checkout.

**Laziness and performance:** `fully lazy` / `bounded hydration` / `potentially
eager`. A command is not "fully supported at scale" merely because it produces the
right result after fetching every changed blob. For example, unmodified Git may
materialize every path changed by a branch switch — measure that behavior rather
than hiding it. The per-command matrix lives in
[`compatibility.md`](compatibility.md).

---

# 4. Lessons as invariants

Each item below is enforced by the architecture and covered by tests. They are the
core design invariants the implementation upholds.

**Do not report a mount before mounting.** The mount is reported ready only after
the kernel mount is live and Git health checks pass. Readiness is observed by
polling for the synthetic `.git` to appear, then running `git rev-parse
--show-toplevel` / `--is-inside-work-tree`. Mount identity is a monotonic
`MountGeneration` counter (`crates/core/src/ids.rs`) used to detect stale kernel
and view entries; there is no named mount state machine and no mount registry.

**Do not maintain two stages.** The single stage is the real `$GIT_DIR/index`.
`git add`, `git add -p`, `git reset`, `git restore --staged`, merges, and conflict
stages operate on that index directly through stock Git.

**Do not maintain two ref models.** Git owns HEAD, `refs/heads/*`,
`refs/remotes/*`, `refs/tags/*`, `ORIG_HEAD`/`FETCH_HEAD`/`MERGE_HEAD`/…,
sequencer and rebase state, and reflogs. Plain `git commit`, `git amend`, `git
rebase`, and `git push` update normal Git state directly.

**Do not use skip-worktree as a universal trick.** The correctness baseline works
without marking every index entry skip-worktree. The mount builds a full real
index from HEAD (`git read-tree HEAD`) and relies on FSMonitor, not on
sparse/skip-worktree semantics, to keep `git status` cheap. (The interop bridge in
`crates/git-store/src/interop.rs` *does* mark every entry skip-worktree, but that
is a throwaway operational index for running stock Git against the shared store —
not the mount hot path.)

**Directory listing must not hydrate file contents.** `readdir` returns names,
inode IDs, and `d_type` only — never exact sizes or blob reads. It merges baseline
tree and overlay children at O(direct children). `readdirplus` is not enabled.

**Do not buffer entire files per callback.** All content paths are
file-descriptor / streaming based. Reads are serviced by `pread` into a cache
file; writes by `pwrite` into an overlay FD. Memory use is not proportional to
blob size (reading a 64 MiB blob grows RSS by roughly one request-sized buffer,
far below the blob size; the test pins it under 24 MiB for a 64 MiB blob).

**Real file handles; never `fh = 0`.** Each successful open allocates a unique
handle (`next_fh` is an `AtomicU64` starting at 1). Reads and writes are serviced
strictly by handle, so open-unlink and rename-while-open work without a path.

**Bounded pools, not thread-per-callback.** FUSE callbacks run on two fixed-size
worker pools (16 object-I/O threads + 4 metadata threads), never one OS thread per
request.

**Empty directories are real workspace state.** An untracked empty directory is a
durable overlay `Dir` entry, not just an inode-table row; it survives lookup,
`readdir`, unmount, and remount. Git omits empty directories from commits.

**The change journal must be durable.** A process-local `Mutex<Vec<…>>` is not a
sufficient FSMonitor backing store. Changes are appended to a durable on-disk log
that is replayed on open, and any discontinuity yields a full-invalidation
response.

**Baseline + overlay model the working tree.** The virtual working tree is a
read-only baseline tree plus a writable overlay (see [§5.2](#52-working-tree-model-baseline--overlay)).

**Linux only; no premature platform scaffolding.** A crate that merely compiles
on another OS is not platform support. The shipped system is Linux / FUSE only.

---

# 5. Architecture

## 5.1 Two sources of truth

| Owner | State |
|-------|-------|
| **Git** (the native admin gitdir) | HEAD, branches, refs, reflogs, remote-tracking refs, **the real `.git/index`** (the only stage), conflict stages 1/2/3, commit/amend, merge/rebase/cherry-pick/stash/bisect/sequencer state, tags, push/fetch config |
| **The projection** (custom state) | only the **virtual working-tree bytes**: baseline + overlay + tombstones + the synthetic `.git`; the FUSE projection; the durable change journal; the content cache; inode/handle tables |

The projection's parsed views of Git state are disposable caches, rebuilt from the
real gitdir. We never mirror Git state into a second authoritative model, never
import commits after Git exits, and never keep a second stage or branch database.
See [`git-state-model.md`](git-state-model.md).

## 5.2 Working-tree model: baseline + overlay

```text
working tree(path) =
  1. synthetic entry (root .git gitfile)   reserved, protected
  2. overlay file / dir / symlink          local writes
  3. overlay tombstone                      deletions (incl. tombstoned ancestor)
  4. overlay clean-rename base-ref          rename/subtree mapping
  5. baseline Git tree entry (HEAD tree)    lazy, unmaterialized
  6. absent
```

Initial state: `baseline = checked-out commit tree`, `overlay = empty`. The
baseline answers "what would this unmaterialized path contain in the logical
working tree" — not what is staged, what HEAD is, or what branch is checked out;
those come from Git. This is why a baseline is necessary: index-only operations
(`git reset --mixed`, `git restore --staged`, `git rm --cached`) change the index
without changing working-tree bytes, and the baseline/overlay split preserves the
working tree while the real index moves independently.

The baseline tree is **fixed at projection open** from the HEAD tree and is
immutable for that projection's life; there is no baseline-advancement machinery.
Branch-changing commands stay correct because stock Git writes every changed path
through the FUSE write path into the overlay (see
[worktree-model.md](worktree-model.md) and the eagerness note in
[§7](#7-required-plain-git-compatibility-surface)).

## 5.3 Crates

The workspace is exactly nine crates (Cargo package names in parentheses):

| Crate | Package | Role |
|-------|---------|------|
| `core` | `glm-core` | Backend-agnostic vocabulary: `ObjectId` (format-tagged raw bytes, never assumed SHA-1), `GitMode`, `RepoPath` (raw non-NUL bytes), `TreeObject`, `MountGeneration`, the per-path state axes `Source`/`SemanticStatus`/`Residency`/`Durability`, `FetchPolicy`, and `Error`/`ErrorCode` with stable string codes + Linux errno mapping. |
| `git-store` | `glm-git-store` | Git-CLI object access: long-lived `cat-file --batch-command` sessions, `object_size`, `smudge_blob`, fd hardening (`CLOEXEC`, `GIT_NO_LAZY_FETCH`, `GIT_OPTIONAL_LOCKS`), byte-exact tree parsing, and the `interop.rs` operational-index bridge. |
| `git-repo` | `glm-git-repo` | `AdminRepo`: the transparent clone, `build_index` (`git read-tree HEAD`), and the FSMonitor seed. |
| `worktree` | `glm-worktree` | The `Projection` (baseline + overlay + content cache + single-flight), the `Overlay` (JSON sidecars), and the `ChangeJournal`. |
| `fs-common` | `glm-fs-common` | `InodeTable`: stable inode identity with generations; `ROOT_INO = 1` is the only pre-allocated inode. |
| `fuse` | `glm-fuse` | `TransparentFs`: the `fuser::Filesystem` implementation, real handle table, and the two bounded worker pools. |
| `cli` | `glm-cli` | The `git-lazy-mount` binary and the `git-lazy-mount-fsmonitor` hook binary. |
| `sgrep` | `glm-sgrep` | A separate standalone remote-grep CLI that reads the overlay/journal directly so search fetches nothing. Not part of the mount. |
| `testkit` | `glm-testkit` | Shared test helpers. |

There is no `daemon`, `ipc`, `namespace`, `overlay`, `object-provider`,
`filtered-cache`, `fsmonitor`, or `platform` crate.

## 5.4 On-disk layout

```text
$XDG_DATA_HOME/git-lazy-mount/        (else ~/.local/share/git-lazy-mount/)
  workspaces/<16-hex-hash-of-mountpoint>/
    git/        real native partial-clone admin gitdir   (NOT inside FUSE)
      glm-fsmonitor/changes.log        durable NUL-separated change journal
    cache/      content-addressed cache files (materialized working-tree bytes)
    overlay/
      meta/     one atomic JSON sidecar per overlay entry  (sha256(path).json)
      content/  native files holding writable content bytes
    anchor/     temporary clone anchor, discarded after init
```

The mounted worktree (`~/huge-repo/`) projects one synthetic read-only regular
file `.git` whose bytes are `gitdir: /abs/.../workspaces/<id>/git`. The admin
gitdir is configured with `core.worktree = <mountpoint>`, so stock Git resolves
the repo through the gitfile and operates on the mounted worktree using its normal
index, refs, locks, and hooks. The admin gitdir lives on a native filesystem,
never inside FUSE, so Git's `index.lock`, `packed-refs`, ref/config locks,
reflogs, sequencer/merge/rebase state, and atomic renames all work normally.

The synthetic `.git` is protected: any create/replace/delete/rename/chmod/write of
it, or any attempt to create a path beneath it, fails safely. Protection is
enforced in `Projection::child_path` (returns `ErrorCode::Authentication`), not by
a reserved inode. A repo tree entry literally named `.git` never shadows the
synthetic one.

Storage durability (atomic sidecars + fsync, content retention via Linux fd
survival) is owned by [`durability-security.md`](durability-security.md); the
overlay/baseline/tombstone model is owned by
[`worktree-model.md`](worktree-model.md).

## 5.5 Command surface

The mount flow does **not** run a per-user daemon and uses no IPC socket. Instead
the parent command spawns a single **detached, hidden `__serve` child** that holds
the kernel mount:

- `cmd_mount` does the clone/index/seed work, then spawns `git-lazy-mount __serve
  --gitdir … --mountpoint … --cache … --overlay …` with stdio nulled. The child is
  reparented to init and is **not** waited on; the parent polls for the synthetic
  `.git`, runs health checks, prints success, and exits.
- `cmd_serve` (the `__serve` body) opens the `AdminRepo`, opens the
  `ChangeJournal`, builds the `Projection` with that journal, and calls
  `glm_fuse::mount`, which blocks until the mount is released.
- `cmd_unmount` runs `fusermount3 -u` (falling back to `fusermount -u`, then
  `umount`); the serving child's `mount()` returns and it exits.
- `cmd_doctor` reports mountpoint, mounted-or-not, and `show-toplevel`
  (optionally as JSON).

The FSMonitor hook (`git-lazy-mount-fsmonitor`) and `sgrep` read the durable
journal file directly — there is no socket and no protocol version to negotiate.

## 5.6 Startup sequence

`cmd_mount` runs as a straight-line sequence:

1. **Preflight.** Create the mountpoint if needed and require it to be empty;
   derive the deterministic per-mountpoint workspace paths.
2. **Clone.** `AdminRepo::clone` runs `git clone --no-checkout
   --separate-git-dir=<gitdir>` with the default `--filter=tree:0`, sets
   `core.worktree=<mountpoint>`, adds `--no-single-branch` when `--depth` is set,
   and runs with `GIT_TERMINAL_PROMPT=0`. A full-object clone (filter rejected
   with `--allow-full-object-clone`) still implies **no** checkout.
3. **Build the index.** `build_index` runs `git read-tree HEAD`, faulting the HEAD
   tree hierarchy and fetching **zero** blobs. This is O(tracked paths) of
   metadata work and is reported honestly, never marketed as O(1).
4. **Configure FSMonitor.** Set `core.fsmonitor` to the
   `git-lazy-mount-fsmonitor` binary beside the executable, and
   `core.fsmonitorHookVersion=2`. (Nothing else is configured — no
   `untrackedCache`, no `hooksPath`, no `index.version`.)
5. **Seed first status.** Open the (empty) change journal, then
   `seed_fsmonitor_valid` so the *first* `git status` faults zero blobs (see
   [§6.4](#64-fsmonitor-v2--change-journal)).
6. **Serve.** Spawn the detached `__serve` child.
7. **Validate.** Poll `mountpoint/.git` for readiness (up to 1000 × 10 ms), then
   run the health checks and print success.

The startup ordering and its deadlock-avoidance constraints are owned by
[`deadlock-startup-recovery.md`](deadlock-startup-recovery.md).

## 5.7 The `tree:0` default and its rationale

The default partial-clone filter is **`tree:0`**: every commit is fetched (so
history, merge-base, `git log`, and branch switching all work), but no trees or
blobs are. `build_index` then faults only the HEAD tree hierarchy; blobs hydrate
on read. This is both correct and cheap.

Two alternatives are deliberately not the default:

- **`--depth 1` (shallow)** grafts the commits, which breaks `git merge` / `git
  rebase` and hides history. Not a default.
- **`blob:none`** with full history would download **every tree from all of
  history** — slow and large on big repos. Not a default.

`blob:none` and `--depth` remain valid **explicit** overrides:

```bash
git lazy-mount <url> <path> --branch main
git lazy-mount <url> <path> --depth 1
git lazy-mount <url> <path> --filter blob:none
git lazy-mount <url> <path> --allow-full-object-clone
```

> A stale code comment in `crates/git-repo/src/lib.rs` still says the filter
> "defaults to `blob:none`"; the `Default` impl sets `tree:0`. Trust the impl.

---

# 6. Subsystems

Each subsystem is summarized here at spec altitude; its area document owns the
depth.

## 6.1 Worktree model

*Area doc: [`worktree-model.md`](worktree-model.md).*

The `Projection` layers the durable overlay over the fixed baseline HEAD tree.
`resolve()` follows the order in [§5.2](#52-working-tree-model-baseline--overlay).
`readdir()` merges baseline and overlay children at O(direct children) with no
blob fetch. Overlay entries are `File` / `Symlink` / `Dir` / `Tombstone` /
`BaseRef{oid,mode}`; a clean file or subtree rename is metadata-only (overlay
re-key plus baseline base-refs at the destination, source tombstoned) and fetches
no blob contents. `RENAME_NOREPLACE` is honored; `RENAME_EXCHANGE` is rejected.
Repository paths are raw `RepoPath` byte sequences (never lossily converted to
UTF-8), so paths with invalid UTF-8, newlines, leading dashes, quotes, and control
characters round-trip correctly.

## 6.2 FUSE semantics

*Area doc: [`fuse-semantics.md`](fuse-semantics.md).*

`TransparentFs` implements these `fuser` operations: `init` (which negotiates
`FUSE_ATOMIC_O_TRUNC`), `lookup`, `forget`, `getattr`, `setattr`, `readlink`,
`open`, `create`, `read`, `write`, `flush`, `fsync`, `release`, `mkdir`, `unlink`,
`rmdir`, `rename`, `symlink`, `opendir`, `readdir`, `releasedir`, `access`, and
`statfs`. Not implemented (these fall through to the `fuser` default, `ENOSYS`):
`link`, `mknod`, all xattr ops, `fallocate`, `copy_file_range`, `lseek`, file
locking (`getlk`/`setlk`/`flock`), `destroy`, and `batch_forget`.

Handles are real (`Handle::Read` / `Handle::Write`, `fh` from 1, never 0); reads
and writes are serviced strictly by `fh` via `pread`/`pwrite` into an FD, with no
whole-file `Vec<u8>` buffering. First writable `O_TRUNC` seeds an empty overlay
file with no baseline fetch; a partial overwrite copies up once and then writes in
place. Open-unlink and rename-while-open keep working through the FD because
service does not depend on a path. Inode identity (`InodeTable`) is stable across
repeated lookups, with a per-inode generation; `ROOT_INO = 1` is the only
pre-allocated inode. Two fixed bounded pools run callbacks: 16 object-I/O threads
and 4 metadata threads (so `ls` stays responsive while reads hydrate). There are
no separate decompress/filter/network pools and no backpressure/cancellation
machinery.

## 6.3 Object fetching

*Area doc: [`object-fetching.md`](object-fetching.md).*

`materialize_path` streams a baseline blob through `cat-file` into a
content-addressed cache file and serves range reads from its FD via a
`ContentHandle` (`pread`, bounded RSS). Concurrent faults of the same object are
coalesced by a per-`ObjectId` single-flight map (`inflight:
Mutex<HashMap<ObjectId, Arc<Mutex<()>>>>`), so a hundred concurrent reads of one
missing blob cause one retrieval. Smudge/working-tree content is produced by `git
cat-file --filters --path --attr-source`. Fetch eligibility is gated by
`FetchPolicy` (`CacheOnly` / `MustNotFetch` / `AllowNetwork` / `Prefetch`); a
missing object under an offline policy maps to a bounded filesystem error
(`OfflineMissingObject → EIO`, `NotFound → ENOENT`).

`getattr` is the one metadata op that may fault: a Git tree entry carries no exact
working-tree size, so `ls -l` / `stat` of an unmaterialized file faults its blob
once. This is fundamental to lazy-blob fetching and is separate from `git status`,
which faults zero blobs (next section).

## 6.4 FSMonitor v2 + change journal

*Area doc: [`fsmonitor.md`](fsmonitor.md).*

Git's FSMonitor v2 hook receives `(version, previous_token)` and returns a new
token, a NUL, then the relative paths changed since that token. Responses are
**inclusive**: false positives are acceptable, false negatives are not. The token
wire form is `glm1:workspace:epoch:seq:generation`; `epoch` and `generation` are
fixed at 1 and 0.

The `ChangeJournal` (`<gitdir>/glm-fsmonitor/changes.log`) is a durable
NUL-separated append log replayed into memory on open. `record()` writes and
`sync_data()`s **before** the FUSE reply, so an acknowledged mutation is always
visible to the next query. `query()` returns a full invalidation (`/`) for any
token it cannot place: an empty token while `seq > 0`, an unparseable token, a
workspace/epoch/generation mismatch, or a `seq` beyond the current sequence.

**The zero-blob first-status finding (canonical here).** The *first* clean `git
status` faults zero blobs, the same as every later one. A freshly `read-tree`'d
index carries no FSMonitor extension, so without intervention Git stats (and so
faults) every entry on the first status before writing the extension. The fix,
applied at mount right after `read-tree`, pre-seeds the FSMonitor index extension
(`seed_fsmonitor_valid`): every entry is marked `CE_FSMONITOR_VALID` carrying the
journal's seq-0 token, so Git's `refresh_cache_ent` early-returns before any
`lstat` and the hook answers "nothing changed" at the seq-0 token. Two carve-outs
keep it correct: (a) checkout conversion attributes
(`filter`/`ident`/`working-tree-encoding`/CRLF `eol`) are detected by reading the
tracked `.gitattributes` blobs directly; if any path declares such an attribute
the entire seed is skipped, so Git's first status checks every path normally and
never hides a diff — bounded by a 20-second attribute-read timeout; (b) the seeded token
must match the hook's identity, else Git falls back safely to the eager scan.
Verified zero-fault on an 81k-file real mount.

## 6.5 Git state model

*Area doc: [`git-state-model.md`](git-state-model.md).*

The transparent design — `git clone --separate-git-dir` + `core.worktree` + a
synthetic `.git` gitfile served by the projection — gives stock Git an ordinary
repository whose working tree happens to be virtual. Git owns all repository state
([§5.1](#51-two-sources-of-truth)); the projection owns only working-tree bytes.
Index-only updates leave baseline and overlay untouched; working-tree updates
flow through FUSE into the overlay exactly as ordinary filesystem operations
would. We never infer a working-tree update from a changed index.

## 6.6 Index strategy

*Area doc: [`index-strategy.md`](index-strategy.md).*

The mount uses a **full real index** built by `git read-tree HEAD` (faulting HEAD
trees, fetching zero blobs) — the maximum-compatibility correctness baseline.
`git-store`'s `interop.rs` synthesizes a *separate* throwaway operational index
(every entry skip-worktree) only to let stock Git run against the shared store off
the mount hot path; it is exercised by the store integration tests, not the FUSE
path. Scalability of larger-repo index strategies (sparse / dynamic skip-worktree
/ a minimal provider extension) is discussed there; the shipped choice is the
full index.

## 6.7 Durability and security

*Area doc: [`durability-security.md`](durability-security.md).*

Overlay durability is per-entry: one atomic JSON sidecar per entry
(`id_for(path) = sha256(path)+".json"`) under `meta/`, content bytes in native
files under `content/`, each published temp-file → `fsync` → `rename` with a
parent-directory fsync so an acknowledged create/rename survives a crash. There is
no SQLite, no namespace database, and no content-id nonce; the in-memory overlay
index is a disposable cache rebuilt from the sidecars on open. Authentication uses
the user's normal credential helper during the initial mount only; FUSE callbacks
are non-interactive (`GIT_TERMINAL_PROMPT=0`) and gate fetches through
`FetchPolicy`. The threat model treats repository data as untrusted (path
traversal, symlink races, decompression bombs, credential redaction); raw paths
are escaped safely for display and JSON.

## 6.8 Deadlock, startup, and recovery

*Area doc: [`deadlock-startup-recovery.md`](deadlock-startup-recovery.md).*

Git processes run *inside* the mount and can trigger FUSE callbacks; callbacks need
Git objects. The invariants that prevent deadlock:

```text
FUSE callbacks never invoke Git porcelain or a worktree-scanning command
FUSE callbacks never wait on the index lock held by the requesting Git process
object readers target the native gitdir directly (long-lived cat-file --batch-command)
GIT_NO_LAZY_FETCH=1 on inspection subprocesses that must not recursively fetch
all mount/session file descriptors are CLOEXEC and not inherited by children
```

Bounded worker pools keep the `fuser` dispatch loop free to answer a `FLUSH`
during a fork/exec (the prior single-threaded deadlock).

---

# 7. Required plain-Git compatibility surface

Do not claim transparent Git compatibility until these commands pass mounted
end-to-end tests through a real `/dev/fuse` mount without a wrapper. The full
per-command correctness-and-laziness matrix is in
[`compatibility.md`](compatibility.md).

```text
discovery/inspection : rev-parse --show-toplevel, status [--porcelain=v2],
                       diff [--cached], log, show, ls-files, cat-file,
                       branch, tag, remote -v
staging/commit       : add [path|-A|-u|-p], reset path, restore --staged,
                       commit [-a|--amend|--fixup|-S], rm [--cached], mv
branch/worktree       : branch, switch [-c], checkout [-- path], restore,
                       reset [--soft|--mixed|--hard]
history              : merge [--abort], rebase [--continue|--abort],
                       cherry-pick [--continue|--abort], revert, stash [pop]
remote               : fetch [--prune], pull [--rebase], push
                       [--force-with-lease|--tags]
working-tree utils   : clean [-n|-fd], grep, blame, bisect, mergetool, difftool
maintenance          : fsck, gc, maintenance run, repack, prune
```

Plain `git push` is required; there is no bespoke push command and no second lease
model. Git's refs, remote-tracking refs, reflogs, and push safety are
authoritative.

**Eagerness is measured, not hidden.** Branch-changing commands
(`switch`/`checkout`/`reset --hard`/`merge`/`rebase`) are correct but potentially
eager: stock Git writes every changed path through the FUSE write path. Measured,
a branch switch over an M-of-N delta touches O(M) blobs (the delta), not O(N) (the
repo). A release may ship stock-Git-compatible while labeling branch transitions
"potentially eager"; it must not claim lazy branch switching until demonstrated.

---

# 8. Hydration budgets

These are automated assertions, not aspirations.

| Operation | Budget |
|-----------|--------|
| **Mount** (`tree:0`) | fetch zero working-file blobs merely to project the tree; the full-index build does O(tracked paths) metadata work and reports it honestly |
| **`ls <dir>`** | zero child blobs, zero smudge filters, O(direct children) namespace work |
| **`ls -l <dir>`** | may fault each blob once for exact size (fundamental); reported distinctly |
| **Clean `git status --porcelain=v2`** | zero blobs, zero smudge filters, no full per-file stat scan — for the **first** and all subsequent clean statuses (via the seed in [§6.4](#64-fsmonitor-v2--change-journal)) |
| **`cat path`** | at most the one required blob plus its attribute/filter metadata |
| **100 concurrent reads of one missing file** | one underlying object retrieval (single-flight) |
| **`open(O_WRONLY\|O_TRUNC)`** | does not fetch the old blob |
| **Repeated 4 KiB writes to a 1 GiB file** | no full-file read/rewrite per callback; no allocation proportional to file size |
| **Clean rename of an unmaterialized file/subtree** | zero blob contents |
| **`git log` / `branch` / `tag` / `status`** | do not hydrate working-tree blobs to inspect metadata |

---

# 9. Limitations

By-design and deferred behaviors are registered in
[`limitations.md`](limitations.md). The load-bearing ones:

- **`getattr` size hydration is fundamental to lazy blobs.** The exact size of an
  unmaterialized file requires its blob, so `ls -l` / `stat` faults it once. Not
  closeable without a server-side size manifest.
- **Smudge-side `.gitattributes` / LFS serve the raw baseline blob.** A
  smudge-filtered file (`eol=crlf`, `ident`, an LFS pointer) reads as its stored
  bytes, not the smudged bytes. Commits stay byte-correct because the clean filter
  is the inverse, and Git's content comparison stays clean.
- **Branch transitions are potentially eager** ([§7](#7-required-plain-git-compatibility-surface)).
- **The change journal has no compaction** and a fixed epoch/generation (1/0).
- **LFS end-to-end and nested lazy submodules are deferred** (some submodule tests
  are `#[ignore]`'d).

---

# 10. Project status

Linux-only, real-`/dev/fuse`-CI tested (CI runs on `ubuntu-latest` only). The
transparent mount drives the full stock-Git surface: stock Git, editors, and
builds operate directly on the virtual working tree with no wrapper.

Not supported yet: end-to-end LFS, full nested submodules, and a shared object
cache across workspaces. Other platforms (macOS/Windows) are out of scope; see
[`future-platforms/`](future-platforms/).

Build: `cargo build --release -p glm-cli --features fuse` produces
`git-lazy-mount`; the `git-lazy-mount-fsmonitor` hook is built alongside and must
sit next to it. Requires libfuse3 and system Git (≥ 2.36).

---

# 11. Implementation discipline

The priority order, highest first:

```text
stock Git correctness
user-data durability
filesystem correctness
transparent UX
measured laziness
large-repository performance
shared-cache optimization
additional platforms
```

The system must never claim to be transparent while any of these hold: a registry
says "mounted" without a kernel mount; plain Git cannot discover the repository; a
gitdir is generated per command; commits must be imported after Git exits; the
custom stage differs from `.git/index`; status only works through a wrapper; push
only works through a bespoke command; `ls` hydrates every file in a directory;
read allocates the complete blob; each write rewrites the whole file; open handles
are path lookups in disguise; open-unlink fails; empty untracked directories
vanish; one FUSE callback spawns one OS thread; FSMonitor state disappears silently
on restart; or Git paths are converted lossily to UTF-8.
