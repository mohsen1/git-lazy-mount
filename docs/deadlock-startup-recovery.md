# Git/FUSE deadlock analysis + startup/recovery state machines

This area of the [specification](design.md) covers deadlock, startup, recovery,
and lifecycle. Companion docs:
[`architecture.md`](architecture.md), [`requirements-checklist.md`](requirements-checklist.md).

This is design, not prose. It defines: the Git-inside-mount deadlock invariants
and how they are enforced in code; the mount **lifecycle** state machine and the
exact idempotent **startup transaction**; the **recovery** procedure. Every
"INV-…" and "REG-…" tag below is a testable invariant that becomes a regression
test.

Two crates here are **superseded** and must not be reused: the
`git lazy-mount git --` bridge in `crates/git-store/src/interop.rs` (it stands up
a throwaway operational gitdir, routes object I/O via `GIT_OBJECT_DIRECTORY`,
synthesizes a skip-worktree index, and *adopts* the resulting commit — every one
of these is an explicit anti-claim) and the custom `stage`/`workspace` branch+commit
model. The reusable substrate is `crates/git-store/src/proc.rs::harden_fds`,
`crates/git-store/src/batch.rs::BatchSession`, the `crates/object-provider`
coalescing scheduler, and the durable `crates/overlay`.

---

## 1. Git/FUSE deadlock analysis

### 1.1 The hazard

Git runs **inside** the mounted worktree (that is the whole point), so
any `git` process can issue kernel VFS calls that become FUSE callbacks served by
*our* daemon. A callback that, to answer, runs `git` or blocks on Git state the
requesting process holds is a cycle: **daemon → git → kernel → daemon**.

Three concrete cycles, all observed or latent in the current tree:

1. **Inherited-fd FLUSH deadlock** (already fixed by `proc.rs::harden_fds`, see
   its doc comment). A `git` subprocess forked by a callback inherits a file the
   *kernel* has open on this mount. At `exec` the kernel closes that descriptor,
   issuing a `FLUSH` back to the daemon. If the only thread that can answer
   `FLUSH` is the one blocked waiting for that `git` to exit → hard deadlock.
   Kernel stack: `fuse_flush → __fuse_simple_request`.

2. **Recursive lazy-fetch deadlock.** A passive read callback that shells out to
   `git` *without* `GIT_NO_LAZY_FETCH=1` lets Git spawn a `git fetch` subprocess
   tree to fault a missing promisor object. That subprocess inherits the mount fd
   and the `cat-file` pipe (cycle 1) **and** may itself touch the worktree. The
   `object-provider` already guards this: `filtered_blob` pre-faults
   `.gitattributes` via the explicit fetcher, then smudges with `no_lazy = false`
   meaning `GIT_NO_LAZY_FETCH=1` (`object-provider/src/lib.rs:255`, `store.rs:98`).

3. **Index-lock self-wait.** A callback that runs Git porcelain (or any command
   that opens `index.lock`) while the requesting `git` process *holds*
   `index.lock` blocks forever — the holder is itself blocked in the kernel
   waiting for our callback to return.

### 1.2 Invariants (each → a regression test)

These are the deadlock invariants made precise. They are enforced by *construction*
(typed policies, a single command-builder, fd hardening), not by review.

- **INV-D1 — No porcelain in callbacks.** A FUSE callback never invokes Git
  porcelain (`status`, `add`, `checkout`, `commit`, `log`, …). The only `git`
  invocations reachable from a callback are the plumbing whitelist in section 1.3.
- **INV-D2 — No worktree-scanning commands in callbacks.** Even plumbing that
  walks the worktree (`git status`, `git diff`, `git ls-files -o`, `git
  add`) is forbidden in a callback; these can recurse into the mount.
- **INV-D3 — Never wait on the requester's index lock.** No callback path opens,
  waits on, or stats-then-blocks on `$GIT_DIR/index.lock`. Object/attribute
  resolution in a read uses the bare admin gitdir's object DB and tree-ish
  `.gitattributes` lookups only — never the index.
- **INV-D4 — Object readers target the native gitdir directly.** Reads go through
  a long-lived `cat-file --batch-command` against `--git-dir <admin>` with
  `GIT_NO_LAZY_FETCH=1`, serving only locally-present objects
  (`batch.rs:41`). A missing promisor object is reported `missing`, never fetched
  inline.
- **INV-D5 — Only the fetch scheduler causes network.** Exactly one component may
  initiate network retrieval: the `object-provider` fetch scheduler via
  `GitStore::fetch_objects` / `fetch` / `push` (`store.rs:142,188,445`), which run
  with `no_lazy = false` (network allowed) and **off the callback thread**. No
  callback transitively reaches a network-enabled `git`.
- **INV-D6 — All session/mount fds are CLOEXEC.** Every long-lived fd the daemon
  holds (the `/dev/fuse` session fd, `cat-file` pipes, overlay/state fds, the
  control socket) is `O_CLOEXEC`. Every spawned child additionally runs
  `harden_fds` (`proc.rs:20`) to mark fds ≥ 3 close-on-exec defensively.
- **INV-D7 — Children never inherit the FUSE session fd.** The fuser session fd
  is opened `O_CLOEXEC` and is *not* among the std{in,out,err} of any child;
  combined with INV-D6 no `git` ever holds the mount fd past `exec`.
- **INV-D8 — Callbacks never block the dispatch loop.** Every potentially
  blocking callback dispatches onto a worker and replies from there, so the fuser
  read-loop stays free to service the `FLUSH` from cycle 1 (`fs-fuse/src/adapter.rs`
  module docs + `dispatch()`). The worker pool is bounded.
- **INV-D9 — No lock held across `git`/network.** No inode, namespace, handle, or
  state lock is held while a subprocess runs or while the network is touched
  (`object-provider` already documents and tests this — locks dropped before
  `fetcher.fetch`, `lib.rs:301`).
- **INV-D10 — Passive hydration runs no hooks.** A read/getattr never triggers a
  Git hook. Hooks fire only because the user invoked a porcelain command.
  Inspection subprocesses set `core.hooksPath` to an empty/no-op path defensively.

### 1.3 The plumbing whitelist (the only `git` a callback may reach)

A callback may reach **only** these, all via `GitStore::git(no_lazy=true)`
(`store.rs:98`), all read-only, all with `GIT_NO_LAZY_FETCH=1`,
`GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`, `harden_fds`, no `--work-tree`,
`stderr` to null/pipe (never inherited):

| Purpose | Command | Notes |
|---|---|---|
| object metadata/content | `cat-file --batch-command` (long-lived) | `batch.rs`; respawn on death |
| tree read (one-shot fallback) | `cat-file tree <oid>` | `store.rs:280` |
| raw blob (fallback) | `cat-file blob <oid>` | `store.rs:291` |
| smudge for projection | `cat-file --filters --path=<p> [--attr-source=<c>]` | `store.rs:309`; attrs pre-faulted |
| attr-source `.gitattributes` presence | `rev-parse <commit>:<dir>/.gitattributes` | `object-provider/src/lib.rs:189` |

Anything not in this table is an INV-D1/D2 violation. There is no `--work-tree`
on any of these, so none can scan the mount.

```rust
/// The fetch policy a code path is allowed to use. Callbacks are restricted to
/// the non-fetching variants by type, not by discipline.
pub enum FetchPolicy {
    /// Read-only callback path: serve from local store, never fetch, never
    /// touch the network. Maps to GIT_NO_LAZY_FETCH=1.
    CacheOnly,
    /// Same, but also asserts the object MUST already be present (debug-panics
    /// in tests if it is not — surfaces a residency bug).
    MustNotFetch,
    /// Only the fetch scheduler / explicit user prefetch may hold this.
    AllowNetwork,
    /// Background prefetch priority.
    Prefetch,
}
impl FetchPolicy { pub fn may_fetch(&self) -> bool { matches!(self, Self::AllowNetwork | Self::Prefetch) } }
```

**Enforcement point.** All FUSE callbacks call the object provider with
`FetchPolicy::CacheOnly`/`MustNotFetch`. The provider's `ensure_present_locally`
returns `OfflineMissingObject` rather than fetching when `!policy.may_fetch()`
(`object-provider/src/lib.rs:146`). The *daemon's background fetch scheduler*
(distinct thread pool) is the only holder of `AllowNetwork`. This makes
INV-D4/D5 a property of the type system: a callback cannot name a fetching policy
without going through the scheduler.

### 1.4 Thread/queue model backing the invariants

```
fuser read-loop (1 thread, never blocks)  ── dispatch() ──▶  callback worker pool (bounded)
                                                                  │  CacheOnly only
                                                                  ▼
                                       object-provider  ──coalesce──▶  fetch scheduler pool (bounded, AllowNetwork)
                                                                  │
                                                                  ▼  (separate semaphores: metadata / overlay-io / decompress / filter / network)
```

- The read-loop thread services `FLUSH` etc. while workers block (INV-D8).
- Callback workers may block on the overlay/object DB but **never** on the
  network and **never** on `git` porcelain.
- A missing object in a callback is *not* fetched inline; the callback either
  returns `EAGAIN/EIO`-class error (offline) or, for content reads, the provider
  hands the oid to the scheduler and the worker waits on a condvar **with no lock
  held** (`lib.rs:321`), while a *different* scheduler thread does the network.
- Cancellation: when the kernel interrupts a request or the requesting pid exits,
  the worker's wait is cancellable; the in-flight scheduler fetch is coalesced and
  reused by later callers (one retrieval for N waiters).

### 1.5 Testable deadlock regressions (REG-D…)

These run through a **real `/dev/fuse` mount** in Linux CI — mocked
callbacks cannot reproduce cycle 1.

- **REG-D1** Open a tracked file on the mount, then run `git status` from inside
  the mount in a subprocess that inherits the open fd; assert no hang (cycle 1).
- **REG-D2** `cat` a missing-promisor file with the daemon `--offline`; assert the
  read returns a clean offline error and the `cat-file` session is **still alive**
  afterward (no fatal exit; `batch.rs` contract).
- **REG-D3** Spawn 100 concurrent reads of one missing blob; assert exactly **1**
  scheduler fetch invocation (`metrics().fetch_invocations == 1`) and all readers
  get the same result (`object-provider` coalescing).
- **REG-D4** Hold `index.lock` (a long-running `git add -p` paused at a prompt)
  and concurrently `getattr`/`read` projected files; assert callbacks complete
  without touching the lock (INV-D3).
- **REG-D5** Audit test: enumerate every `Command::new("git")` reachable from a
  callback entry point; assert each sets `GIT_NO_LAZY_FETCH=1` and is in the
  section 1.3 whitelist (static + runtime hook via a `git` shim that records argv/env).
- **REG-D6** fd audit: after mount, `ls -l /proc/<daemon>/fd`; spawn a `git`
  child; assert the child's fd table contains **no** `/dev/fuse` fd and no
  `cat-file` pipe (INV-D6/D7).
- **REG-D7** A read callback must not invoke any hook: install a tripwire
  `post-index-change`/`reference-transaction` that touches a sentinel file; do a
  pure `cat`; assert the sentinel is untouched (INV-D10).

---

## 2. Mount lifecycle state machine

The prior controller created the mountpoint dir and **immediately** wrote
`MountState::Mounted` (`daemon/src/controller.rs:179` —
`state: MountState::Mounted` with only a `create_dir_all`, *no kernel mount*).
That is exactly the anti-claim "the mount registry says mounted
without a kernel mount." The design replaces the coarse
`registry.rs::MountState` (8 states) with the full lifecycle set and forbids `Mounted`
before a kernel mount + Git health checks.

### 2.1 States

```rust
/// Mount lifecycle. Persisted in the registry; the daemon is the single
/// writer. Only `Mounted` means "kernel mount live + git healthy".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountState {
    Creating,        // record reserved, preflight running
    Cloning,         // partial clone / fetch in progress
    InitializingGit, // configuring admin gitdir
    BuildingIndex,   // writing the real $GIT_DIR/index from the base tree
    StartingDaemon,  // daemon process / session coming up
    Mounting,        // kernel mount syscall issued, not yet validated
    Validating,      // running health checks
    Mounted,         // LIVE: kernel mount + git health both pass
    Quiescing,       // draining in-flight FUSE ops before unmount
    Unmounting,      // kernel unmount in progress
    Recovering,      // recovery running
    Failed,          // needs attention; carries a FailureReason
}
```

### 2.2 Transition table

| From | Event | To | Side effects / guard |
|---|---|---|---|
| (none) | `clone <url> <path>` | `Creating` | reserve registry record under workspace lock |
| `Creating` | preflight OK | `Cloning` | — |
| `Creating` | preflight fail | `Failed` | actionable error; record never claimed `Mounted` |
| `Cloning` | clone+fetch OK | `InitializingGit` | base commit/tree resolved |
| `InitializingGit` | config written | `BuildingIndex` | gitfile + `core.worktree` set |
| `BuildingIndex` | index written | `StartingDaemon` | 0 blobs fetched during build (assert) |
| `StartingDaemon` | session up | `Mounting` | session fd `O_CLOEXEC` (INV-D6) |
| `Mounting` | `mount()` returns | `Validating` | kernel mount exists |
| `Validating` | health checks pass | **`Mounted`** | publish `Mounted`; return success |
| `Validating` | any check fails | `Failed` | unmount kernel; cleanup; keep overlay |
| `Mounted` | `unmount` / shutdown | `Quiescing` | stop accepting new ops |
| `Quiescing` | drained | `Unmounting` | flush overlay, seal journal |
| `Unmounting` | kernel detached | (record `Unmounted`/removed) | idempotent |
| any | daemon start finds non-terminal state | `Recovering` | run recovery |
| `Recovering` | reconciled healthy | `Mounted` or `Unmounting` | per recovery outcome |
| `Recovering` | unrecoverable | `Failed` | quarantine; export path offered |

**INV-L1.** `Mounted` is written **only** from `Validating` after both the kernel
mount exists and all health checks pass. (Regression REG-L1: kill the daemon
between `Mounting` and `Validating`; assert the registry never shows `Mounted`
and recovery cleans up.)

**INV-L2.** Every state except `Mounted`/`Unmounted` is *non-terminal*: a daemon
that finds the registry in such a state on startup MUST enter `Recovering`, never
assume liveness (REG-L2).

**INV-L3.** State transitions are persisted **before** their externally-visible
effect where a crash-in-between must be recoverable (write-ahead): e.g.
`Mounting` is persisted before the `mount()` syscall so a crash mid-mount is
detectably "maybe mounted" and reconciled against the kernel.

```rust
pub enum FailureReason {
    Preflight(String), CloneRejected(String), IndexBuild(String),
    MountSyscall(String), HealthCheck(String), Recovery(String),
}
```

---

## 3. Startup as an idempotent transaction

Single entry point on the daemon; the CLI `git lazy-mount <url> <path>` calls it
and **blocks until `Mounted` or `Failed`**. Re-running on a partially
created workspace resumes from the persisted state (idempotent).

```rust
pub struct StartupRequest {
    pub url: String,            // redacted in all logs
    pub mountpoint: PathBuf,
    pub opts: CloneOptions,     // filter/branch/depth/allow_full_object_clone (controller.rs:21)
    pub offline: bool,
}
pub struct MountReady {
    pub spec: MountSpec,
    pub base: ObjectId,         // base commit the projection started at
    pub index_stats: IndexBuildStats, // honest reporting
}
pub fn start_mount(&self, req: &StartupRequest) -> Result<MountReady>; // → Mounted | Err(Failed)
```

The transaction is the ordered phase list below. Each phase is resumable; each
acquires the **inter-process workspace lock** (`flock` on
`workspaces/<id>/lock`) so two `git lazy-mount` invocations cannot race
(in-process mutexes are insufficient).

### 3.1 Phase 0 — preflight → `Creating`

Validate, failing fast with an actionable error (never prompting from a
callback; auth may be interactive *here* only):

```
git present & ≥ min version       fuse available (/dev/fuse, fusermount3)
mountpoint owned by user & empty   mountpoint not nested under another managed mount (registry scan)
data-dir perms (0700)              remote URL parses; credentials available (unless --offline)
partial-clone filter supported     case behavior / symlink support of the overlay fs
free disk space                    stale registry entries reaped; stale native git locks (index.lock/*.lock) cleared
```

Reuse `crates/platform/src/validate.rs` for path/case behavior and
`registry.find_for_path` (`registry.rs:123`) for the nesting check. Preflight
writes the `Creating` record **first** so a crash leaves a reconcilable marker.

### 3.2 Phase 1 — create the Git repository → `Cloning`

Native partial clone into the admin gitdir **outside** FUSE:

```
git clone --filter=blob:none --no-checkout --separate-git-dir=<admin> <url> <temp-anchor>
# full history by default; normal origin/branch/upstream config; then discard <temp-anchor>
```

(Or the equivalent `init --bare` + `fetch` the current code uses,
`controller.rs:90`.) If the remote rejects the filter and
`!allow_full_object_clone`, fail with an actionable message
(`controller.rs:129`). **A full-object clone still implies no checkout.**
Resolve and record the **base commit/tree** (the attached branch tip).

### 3.3 Phase 2 — initialize working-tree state

Record the baseline + empty overlay + first generation + first FSMonitor token —
no blobs:

```
baseline = base commit tree          overlay = empty (crates/overlay)
namespace generation = 1             fsmonitor token = (workspace, epoch=1, seq=0, gen=1)
```

### 3.4 Phase 3 — initialize the real index → `BuildingIndex`

Build a **full `$GIT_DIR/index`** from the base tree using the admin gitdir.
Correctness-first: O(tracked paths), **the index build itself fetches zero
blobs**. Use `git read-tree <base-tree>` against the admin gitdir (with
`core.worktree` set to the mountpoint but the mount **not yet live**, so no
callbacks fire). The `read-tree`'d entries carry no worktree stat data, so the
*first* clean `git status` after mount cannot be skipped by FSMonitor: stock git
populates each entry's stat (including the file SIZE) before it will trust the
content as clean, and under a `blob:none` clone the exact size requires the
blob — so the first status faults each tracked blob once (the getattr
size-hydration cost; see [`requirements-checklist.md`](requirements-checklist.md)).
Report honestly (never market as O(1)):

```rust
pub struct IndexBuildStats {
    pub wall: Duration, pub index_bytes: u64, pub peak_rss: u64,
    pub tree_objects_read: u64, pub blob_objects_fetched: u64, // MUST be 0
}
```
**INV-S1.** `blob_objects_fetched == 0` during index build (REG-S1).

### 3.5 Phase 4 — configure Git integration → `InitializingGit`

Write the synthetic gitfile and config; preserve user globals (never
touch identity/signing/editor/pager/aliases/credential-helpers/remote policy):

```
<mountpoint>/.git  ⇒  "gitdir: <abs admin gitdir>\n"   (synthetic, protected)
core.worktree=<mountpoint>   core.bare=false
core.fsmonitor=<abs hook path>   core.fsmonitorHookVersion=2
core.untrackedCache=true (after capability test)   index.version=4 (after compat test)
core.fileMode / core.symlinks / core.ignoreCase  ← from mount behavior (validate.rs)
```

The gitfile is a **projected synthetic entry**, not a real file in the overlay;
it has a reserved stable inode and is protected from
unlink/rename/replace/chmod/write/mkdir-beneath. A tree entry colliding with
`.git` fails safely.

### 3.6 Phase 5 — start + validate the mount → `StartingDaemon` → `Mounting` → `Validating` → `Mounted`

Bring up the session (fd `O_CLOEXEC`), persist `Mounting`, issue the kernel mount
(`fs-fuse::spawn_mount`), persist `Validating`, then run the health checks. These
checks themselves run `git` **inside** the now-live mount, so they exercise the
deadlock invariants for real:

```
test "$(cat <mnt>/.git)" = "gitdir: <admin>"
git -C <mnt> rev-parse --is-inside-work-tree   → true
git -C <mnt> rev-parse --show-toplevel         → <mnt>
git -C <mnt> symbolic-ref --short HEAD         → <branch>
git -C <mnt> status --porcelain=v2             → exit 0
# filesystem probes:
readdir(<mnt>/)            lookup one tracked path     read a small tracked file
create/write/delete a disposable untracked test path (then remove it)
fsmonitor query (token round-trip)
```

Only after **all** pass: persist `Mounted`, return `MountReady`. Any failure →
unmount the kernel mount, persist `Failed{HealthCheck}`, **preserve the overlay
and admin gitdir** (no destructive cleanup of user-reachable state).

**INV-S2.** The health-check `git status` exits 0 and is byte-faithful. It is
**not** zero-blob: this *first* clean status faults each tracked blob once to
hydrate the index stat size (a `blob:none` clone cannot know a blob's exact size
without it, and stock git will not mark an entry clean from FSMonitor without that
size). Only *subsequent* clean statuses are zero-blob, served from `core.fsmonitor`
plus the now-populated stat data (REG-S2). **INV-S3.** A crash at any phase leaves
the registry in a non-terminal state recoverable by recovery with no
acknowledged-write loss (crash-injection matrix: after each phase boundary).

---

## 4. Recovery procedure

Runs on daemon startup when the registry shows a non-terminal state (INV-L2), on
explicit `git lazy-mount recover <mnt>` / `--export <dir>`, and after a detected
backend/daemon restart (cf. the existing FSKit path
`crates/fs-fskit/src/recovery.rs` — same *shape*, but the design’s authority is
the real gitdir + durable overlay, not the oplog). The seven recovery steps, in
order, each a function returning structured findings:

```rust
pub struct RecoveryReport {
    pub healthy: bool,
    pub fsmonitor_invalidated: bool,        // true ⇒ next FSMonitor query returns "/"
    pub quarantined: Vec<RepoPath>,         // ambiguous files set aside, not deleted
    pub preserved_writes: Vec<RepoPath>,    // acknowledged user writes confirmed intact
    pub reconciled_temp: usize,             // torn temp files resolved
    pub kernel_reconciled: KernelState,     // Mounted | NotMounted | Stale
    pub issues: Vec<String>,                // redacted diagnostics
    pub outcome: MountState,                // Mounted | Unmounting | Failed
}
pub fn recover(&self, spec: &MountSpec, export: Option<&Path>) -> Result<RecoveryReport>;
```

### 4.1 Step 1 — validate the namespace DB

Open `state.sqlite` (SQLite WAL) read-write; run integrity checks (`PRAGMA
integrity_check`, foreign-key/parent-index consistency). If the WAL has an
uncommitted tail, let SQLite recover it. On corruption: do **not** discard —
snapshot the DB to the quarantine dir, attempt a parent-indexed rebuild from
overlay metadata records (`crates/overlay` already rebuilds its in-memory index
by scanning persisted `meta/*.json`, ignoring torn temp files — `overlay/src/lib.rs:67`).
Set `fsmonitor_invalidated = true` if the namespace generation can't be trusted.

### 4.2 Step 2 — reconcile temporary content files

The overlay publishes atomically: content temp → fsync → rename, *then* metadata
temp → fsync → rename (`overlay/src/lib.rs:147`, `atomic_write` at `:249`).
Recovery therefore:

- A `meta/*.json` whose referenced `content/<id>` is **absent or zero-len with a
  live temp sibling** ⇒ the write never completed ⇒ drop the metadata record
  (the user's app never got an `ack`).
- A `content/<id>` with **no** committed `meta` record ⇒ orphan temp ⇒ if it has
  no acknowledged-write marker, remove it; otherwise quarantine (step 7).
- Torn temp files (`*.tmp`/`NamedTempFile` leftovers) with no rename target ⇒
  remove (they are by construction pre-`ack`).

### 4.3 Step 3 — preserve acknowledged writes — **the cardinal rule**

**INV-R1.** No recovery step deletes a file that contains an *acknowledged* user
write. A write is "acknowledged" once the FUSE callback returned success to the
kernel; the overlay's two-phase publish guarantees that an acknowledged
`write`/`create`/`rename` has its content **and** metadata durably renamed before
the callback returns (`overlay` ordering above). Recovery confirms each such path
is intact and records it in `preserved_writes`; anything it cannot confirm goes to
quarantine, never to deletion (REG-R1: crash after `write` ack but before
unmount → byte-exact survival across recovery, cf.
`fs-fskit/recovery.rs::reattach_recovers_consistent_state_without_data_loss`).

### 4.4 Step 4 — reconcile mounted state with the kernel

Determine the **actual** kernel state independent of the registry:

```
is <mnt> a fuse mount of subtype "glm"?  →  scan /proc/self/mountinfo for the mountpoint+fstype
```

| Registry says | Kernel says | Action |
|---|---|---|
| `Mounting`/`Validating`/`Mounted` | mounted (ours) | adopt or re-validate; if validate fails → unmount + `Failed` |
| `Mounting`/`Validating`/`Mounted` | not mounted | crash before/after mount; safe to remount or finish unmount |
| any | mounted but **not ours** (stale/other) | refuse to overwrite; `Failed` with actionable message (mountpoint-substitution guard) |
| `Quiescing`/`Unmounting` | mounted | finish the unmount (idempotent) |

`KernelState = Mounted | NotMounted | Stale`. The `AutoUnmount` option already
mitigates wedged mounts when permitted (`fs-fuse/src/adapter.rs:425`); recovery
handles the cases where it was not.

### 4.5 Step 5 — reconcile native gitdir state

The gitdir is **authoritative**; the daemon's parses are disposable caches.
Recovery rebuilds its caches from disk and clears stale native locks left
by an interrupted `git`:

```
stale $GIT_DIR/index.lock  → remove iff no live git holds it (pid/owner check; never steal a live lock)
re-read HEAD, refs/, packed-refs, ORIG_HEAD, FETCH_HEAD, MERGE_HEAD, CHERRY_PICK_HEAD,
        REBASE_HEAD, REVERT_HEAD, BISECT_*, sequencer/, rebase-merge/, rebase-apply/
recompute base commit/tree; if HEAD moved while we were down, advance baseline per the baseline-advance rules
```

**INV-R2.** Recovery never *writes* Git refs/index to "fix" them — it reconciles
its own caches and clears only locks it can prove are abandoned. (The superseded
commit-adoption path is gone.)

### 4.6 Step 6 — invalidate FSMonitor continuity if uncertain

If *any* of these hold, set `fsmonitor_invalidated = true` so the next FSMonitor
query returns the full-invalidation token `/` (false positives OK, false
negatives never):

```
journal loss / epoch gap        DB rollback / rebuild      token from another workspace or future generation
journal compaction past a needed token   unreconciled crash (we were non-terminal)   external overlay modification
namespace generation bumped during recovery
```

The FSMonitor token is `(workspace, journal-epoch, monotonic-seq,
projection-generation)`. FSMonitor is wired to `core.fsmonitor` via the
`git-lazy-mount-fsmonitor` hook, which reads the durable change journal the daemon
writes synchronously; the journal is a durable append log that survives restart (and
when continuity cannot be proven, FSMonitor returns `/`). It delivers correct change
detection (no false negatives) and lets git
skip the redundant full-tree stat scan on subsequent clean statuses. Recovery
**bumps the journal epoch** when it cannot prove continuity; an epoch bump
deterministically forces `/` for any pre-restart token. Wire format of the
`/`-response: `<new-token>\0/\0`.

### 4.7 Step 7 — quarantine ambiguous files

Anything recovery cannot classify as (a) clean baseline, (b) confirmed
acknowledged write, or (c) provably-incomplete temp goes to
`workspaces/<id>/quarantine/<ts>/` with a manifest (original path bytes, source,
reason) — **never deleted**. `recover --export <dir>` copies quarantined +
preserved-but-unmountable content out for the user. Quarantine is also where a
corrupt namespace DB snapshot and orphaned content land.

### 4.8 Recovery outcome

```
all steps healthy, kernel adoptable      → re-validate → Mounted
healthy but registry was Unmounting      → finish Unmounting
unrecoverable (namespace unrebuildable, kernel stale-foreign, validate fails) → Failed + export offered
```

### 4.9 Testable recovery regressions (REG-R…)

- **REG-R1** acknowledged-write survival across crash+recovery (INV-R1), byte-exact.
- **REG-R2** torn temp file (crash between content-rename and meta-rename) ⇒ no
  spurious file, no metadata pointing at absent content.
- **REG-R3** stale `index.lock` from a killed `git add` ⇒ removed; a *live*
  holder's lock ⇒ **not** stolen (INV-R2 lock-ownership).
- **REG-R4** registry `Mounted` but kernel not mounted ⇒ recovery remounts +
  re-validates, never serves a phantom (INV-L1).
- **REG-R5** foreign mount occupying the mountpoint ⇒ `Failed`, no overwrite.
- **REG-R6** journal epoch gap ⇒ next FSMonitor query returns `/` (INV-R3).
- **REG-R7** corrupt `state.sqlite` ⇒ rebuilt from overlay meta where possible;
  ambiguous entries quarantined, none deleted.
- **REG-R8** crash-injection at each crash-injection point (overlay create/write/rename/
  unlink/fsync, index.lock, index replacement, ref txn prepared/committed,
  journal append, registry update, mount success, health check) ⇒ no acknowledged
  data lost; recovery converges to `Mounted`/`Unmounting`/`Failed`.

**INV-R3.** Uncertainty is always resolved toward *more* invalidation and *no*
deletion: FSMonitor returns `/` rather than risk a false-negative; files are
quarantined rather than removed. This is the single safety bias of recovery.

---

## 5. Invariant → test index (summary)

| Tag | Invariant | Regression |
|---|---|---|
| INV-D1..D2 | no porcelain / worktree-scan in callbacks | REG-D5/D7 |
| INV-D3 | never wait on requester index lock | REG-D4 |
| INV-D4 | object readers hit native gitdir, NO_LAZY_FETCH | REG-D2 |
| INV-D5 | only fetch scheduler causes network | REG-D3 |
| INV-D6..D7 | session/mount fds CLOEXEC, not inherited | REG-D6 |
| INV-D8 | callbacks never block dispatch loop | REG-D1 |
| INV-D9 | no lock across git/network | (provider test) |
| INV-D10 | passive hydration runs no hooks | REG-D7 |
| INV-L1 | `Mounted` only after kernel mount + health | REG-L1 |
| INV-L2 | non-terminal states ⇒ recover on startup | REG-L2 |
| INV-L3 | write-ahead state persistence | REG-S3 |
| INV-S1 | index build fetches 0 blobs | REG-S1 |
| INV-S2 | health-check status exits 0; first status faults each blob once, subsequent statuses zero-blob | REG-S2 |
| INV-S3 | crash-at-any-phase recoverable, no loss | REG-R8 |
| INV-R1 | acknowledged writes never deleted | REG-R1 |
| INV-R2 | recovery never rewrites git state / steals live locks | REG-R3 |
| INV-R3 | uncertainty ⇒ invalidate + quarantine, never delete | REG-R6/R7 |
