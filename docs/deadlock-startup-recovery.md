# Startup and Git/FUSE deadlock-avoidance

This area of the [specification](design.md) covers two real, load-bearing
concerns: the **startup sequence** that `git lazy-mount <url> <path>` runs, and
the **deadlock invariants** that keep `git` running *inside* the mount from
wedging the filesystem that serves it. Companion docs:
[`architecture.md`](architecture.md), [`fuse-semantics.md`](fuse-semantics.md),
[`object-fetching.md`](object-fetching.md), [`fsmonitor.md`](fsmonitor.md).

This is explanation grounded in code. Each `INV-…` below is an invariant the
shipped system upholds; where a regression test exists, it is named. The deadlock invariants are the substantive content here — there is **no
daemon, registry, persisted state machine, recovery command, or namespace DB**.

The reusable substrate the rest of this doc leans on is real:
`crates/git-store/src/proc.rs::harden_fds`, the `GIT_NO_LAZY_FETCH` /
`GIT_OPTIONAL_LOCKS` discipline in `crates/git-store`,
`crates/git-store/src/batch.rs::BatchSession`, and the bounded worker-pool
offload in `crates/fuse/src/pool.rs` + `crates/fuse/src/mount.rs`. The interop
bridge (`GitStore::interop_run`) in `crates/git-store/src/interop.rs` is **not**
part of the mount hot path, but it is **live** — stock `git status`/`commit` run
against the shared store through a throwaway operational gitdir it stands up; it
is exercised by `store_integration.rs`.

---

## 1. Git/FUSE deadlock analysis

### 1.1 The hazard

Git runs **inside** the mounted worktree (that is the whole point), so any `git`
process can issue kernel VFS calls that become FUSE callbacks served by *our*
serve process. A callback that, to answer, runs `git` or blocks on Git state the
requesting process holds is a cycle: `serve → git → kernel → serve`.

Two concrete cycles, both real in the current tree:

1. **Inherited-fd FLUSH deadlock** (prevented by `proc.rs::harden_fds`). A `git`
   subprocess forked by a callback inherits a file the *kernel* has open on this
   mount. At `exec` the kernel closes that descriptor, issuing a `FLUSH` back to
   the serving process. If the only thread that can answer `FLUSH` is the one
   blocked waiting for that `git` to exit → hard deadlock. Observed kernel stack:
   `fuse_flush → __fuse_simple_request` (see the `harden_fds` doc comment,
   `crates/git-store/src/proc.rs:8-19`).

2. **Recursive lazy-fetch deadlock.** A passive read/metadata callback that
   shells out to `git` *without* `GIT_NO_LAZY_FETCH=1` lets Git spawn a `git
   fetch` subprocess tree to fault a missing promisor object. That subprocess
   inherits the mount fd (cycle 1) and may itself touch the worktree. Passive
   object access therefore runs with `GIT_NO_LAZY_FETCH=1` and reports a missing
   promisor object as *missing*, never fetching inline.

### 1.2 Invariants

These hold by construction (fd hardening, a hook-free / lock-light command
environment, the bounded-pool offload), not by review.

- **INV-D1, no porcelain in callbacks.** A FUSE callback never invokes Git
  porcelain (`status`, `add`, `checkout`, `commit`, `log`, …). The mount hot
  path never runs porcelain at all; object access goes through `glm-git-store`.
- **INV-D2, no worktree-scanning commands in callbacks.** No callback runs a
  command that walks the worktree (`git status`, `git diff`, `git ls-files -o`).
  The object readers below set no `--work-tree`, so none can recurse into the
  mount.
- **INV-D3, never wait on the requester's index lock.** No callback path opens,
  waits on, or stats-then-blocks on `$GIT_DIR/index.lock`. Object reads use the
  admin gitdir's object DB only, never the index. Reinforced by
  `GIT_OPTIONAL_LOCKS=0` on every store subprocess
  (`crates/git-store/src/store.rs:102`, `crates/git-store/src/batch.rs:48`).
- **INV-D4, object readers target the native gitdir directly with no lazy
  fetch.** Passive reads go through a long-lived `cat-file --batch-command`
  session against the admin gitdir with `GIT_NO_LAZY_FETCH=1`
  (`crates/git-store/src/batch.rs:45-48`), serving only locally-present objects.
  The caller is the residency authority and must materialize/confirm residency
  *before* querying: querying a missing promisor object makes Git refuse the lazy
  fetch and terminate the session (`alive=false`), surfaced as an error so the
  owner can respawn it (`BatchSession`, `crates/git-store/src/batch.rs:25-30`,
  `:92`). A genuinely-absent *non-promisor* object is reported missing without
  killing the session.
- **INV-D5, hydration fetch is off the offending path, not inline-with-a-held-fd
  cycle.** Blob hydration is the one place network fetch is permitted, and it
  happens in `Projection::materialize_path` via `GitStore::blob_to_file(oid,
  allow_fetch=true, …)` (`crates/worktree/src/lib.rs:596-627`,
  `crates/git-store/src/store.rs:322-346`) — a `cat-file blob` run *without*
  `GIT_NO_LAZY_FETCH`, so Git's own promisor lazy-fetch supplies the object. It
  is `harden_fds`-hardened and holds no inode/projection lock across the
  subprocess (only a per-oid single-flight lock, see INV-D9).
- **INV-D6, all session/mount fds are CLOEXEC.** Every spawned child runs
  `harden_fds` (`crates/git-store/src/proc.rs:20`), marking fds ≥ 3
  close-on-exec, so no `git` ever holds the `/dev/fuse` session fd or a
  `cat-file` pipe past `exec`. The long-lived `cat-file` session is hardened the
  same way (`crates/git-store/src/batch.rs:52-56`).
- **INV-D7, callbacks never block the dispatch loop.** Every blocking callback is
  dispatched onto a bounded worker pool and replies from there, so the fuser
  read-loop stays free to service the `FLUSH` from cycle 1. See section 1.3.
- **INV-D8, no projection lock held across `git`/network.** `materialize_path`
  drops the `inflight` map lock before taking the per-oid lock, and holds only
  that per-oid lock — never an inode or directory lock — across the `cat-file`
  subprocess (`crates/worktree/src/lib.rs:603-616`).
- **INV-D9, single-flight hydration.** Concurrent reads of the same missing blob
  coalesce: the first caller fetches under a per-oid `Arc<Mutex<()>>`, later
  callers block on it and then find the published cache file
  (`crates/worktree/src/lib.rs:601-610`). This is a plain mutex, **not** a
  condvar/scheduler; there is no separate fetch-scheduler thread pool.

### 1.3 Thread model backing the invariants

`TransparentFs` (`crates/fuse/src/mount.rs`) runs fuser's serial dispatch loop on
one thread and offloads every potentially-blocking callback to one of **two
bounded pools** (`crates/fuse/src/pool.rs`):

```
fuser read-loop (serial, never blocks)
  ├─ pool       (POOL_THREADS = 16)  object-IO callbacks: lookup, getattr, read,
  │                                   open, create, write, fsync, unlink, rename…
  └─ meta_pool  (META_THREADS  = 4)   fast, non-faulting metadata: opendir/readdir
```

- The read-loop thread services `FLUSH` etc. while workers block (INV-D7).
- Workers may block on the overlay/object DB and on the *one* hydration fetch
  (INV-D5), but never on Git porcelain.
- The pool is fixed-size (not thread-per-callback); each job is panic-isolated so
  one bad callback cannot shrink the pool toward a wedged mount
  (`crates/fuse/src/pool.rs:36-41`).

There are **no** separate decompress/filter/network pools and **no**
backpressure/cancellation machinery. `META_THREADS` exists so `readdir` stays
responsive under heavy object IO.

### 1.4 `FetchPolicy`: a vocabulary type, not an enforced gate

`crates/core/src/fetch.rs` defines `FetchPolicy` (`CacheOnly`, `AllowNetwork`,
`Prefetch`, `MustNotFetch`) with `may_fetch()`. It is the intended vocabulary for
"may this path touch the network", and it documents the rule that callbacks
should run `MustNotFetch`/`CacheOnly` while only a fetch path escalates to
`AllowNetwork`.

In the shipped code that rule is **enforced positionally, not by the type**:
passive object access hard-codes `GIT_NO_LAZY_FETCH=1` (`BatchSession`,
`GitStore::git(no_lazy=true)`), and hydration hard-codes `allow_fetch=true`.
Nothing outside `crates/core` consumes `FetchPolicy` as a gate — `grep FetchPolicy
crates/{fuse,worktree,git-store}` finds no call sites. It is a defined
vocabulary type that is **not wired** as the single enforcement point.

---

## 2. Startup sequence

Startup is a straight-line function in the CLI, **not** a daemon transaction.
`cmd_mount` (`crates/cli/src/main.rs:140-183`) runs entirely outside FUSE, then
hands off to a detached serve child. There is no inter-process lock, no persisted
state, and no resume-from-partial-state; re-running on a non-empty mountpoint
fails fast (`crates/cli/src/main.rs:143-149`).

### 2.1 Phases

1. **Preflight.** Create the mountpoint if needed and require it empty
   (`main.rs:142-149`). Derive the deterministic per-mountpoint workspace layout
   `data_dir()/workspaces/<16-hex>/{git,cache,overlay,anchor}`
   (`workspace_paths`, `main.rs:114-128`).

2. **Partial clone (outside FUSE).** `AdminRepo::clone` runs a native partial
   clone into the admin gitdir with the signature defaults: **`--no-checkout`**,
   **`--separate-git-dir=<admin>`**, and **`--filter=tree:0`** (full commit
   history; trees and blobs fault lazily). `tree:0` is the default because a
   shallow `--depth 1` grafts commits (breaking `git merge`/`rebase` and hiding
   history) and `blob:none` would download every tree from all of history; `tree:0`
   keeps merge/rebase/log/branch-switch working *and* cheap
   (`crates/cli/src/main.rs:155-168`, rationale at
   `crates/git-repo/src/lib.rs:44-49`). `--no-single-branch` is added when
   `--depth` is set; `GIT_TERMINAL_PROMPT=0` throughout.

3. **Build the real index.** `repo.build_index()` runs `git read-tree HEAD`
   against the admin gitdir. This faults the HEAD trees and **zero blobs**, then
   writes a full `$GIT_DIR/index` of `O(tracked paths)` (`main.rs:169-170`).

4. **Configure + seed FSMonitor.** `configure_fsmonitor` points `core.fsmonitor`
   at the `git-lazy-mount-fsmonitor` hook next to the binary and sets
   `core.fsmonitorHookVersion=2` — nothing else (`main.rs:188-209`). If the hook
   is found, `seed_first_status` opens the (empty) change journal and calls
   `seed_fsmonitor_valid`, which marks every index entry `CE_FSMONITOR_VALID` at
   the seq-0 token (`main.rs:215-223`). This is why the *first* clean `git status`
   faults zero blobs, not just later ones. The seed is owned and fully explained
   by [`fsmonitor.md`](fsmonitor.md) (including the conversion-attribute carve-out
   that skips the seed); other docs link there rather than restate it.

5. **Spawn a detached serve child.** `mount_and_validate` spawns
   `git-lazy-mount __serve --gitdir … --mountpoint … --cache … --overlay …` with
   all stdio nulled. The child is **reparented to init and never waited on**; it
   holds the kernel mount after the parent command returns
   (`main.rs:226-245`). The hidden `__serve` verb is internal
   (`main.rs:73-84`); `cmd_serve` opens the `AdminRepo`, `ChangeJournal`, and
   `Projection::with_journal`, then calls `glm_fuse::mount`, which blocks until
   unmount (`main.rs:285-305`).

6. **Poll for readiness.** The parent polls `mountpoint/.git` up to 1000 × 10 ms
   (~10 s) for the synthetic `.git` to appear, i.e. for the mount to serve
   (`main.rs:247-259`).

7. **Health checks.** The parent runs stock `git` *inside the now-live mount* —
   `rev-parse --show-toplevel` (must equal the mountpoint),
   `rev-parse --is-inside-work-tree` (must be `true`), and `symbolic-ref --short
   HEAD` for the success line (`main.rs:261-277`). These checks deliberately
   exercise the deadlock invariants of section 1 against a real `/dev/fuse` mount.

On success the parent prints the mounted/branch line and exits; the detached
child keeps serving. On a failed readiness poll or health check the parent
returns an error, but it does **not** unmount or roll back — there is no
transactional cleanup.

### 2.2 Invariants

- **INV-S1.** The index build (`read-tree HEAD`) fetches **zero** blobs
  (`crates/git-repo/src/lib.rs:502` notes `read-tree` runs with
  `GIT_NO_LAZY_FETCH=1`, so its success proves the trees are local).
- **INV-S2.** The first clean `git status` after mount is **zero-blob**, because
  the FSMonitor extension was pre-seeded at mount (`seed_fsmonitor_valid`) so
  git's `refresh_cache_ent` early-returns on `CE_FSMONITOR_VALID` before any
  `lstat`, and the hook answers "nothing changed" at the seq-0 token. Subsequent
  clean statuses are likewise zero-blob, served from `core.fsmonitor`. Owned by
  [`fsmonitor.md`](fsmonitor.md).
- **INV-S3.** Health checks run real `git` inside the live mount, so a deadlock
  regression (cycle 1 or 2) surfaces at mount time, not only under load.
