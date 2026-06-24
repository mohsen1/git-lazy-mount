# Git/FUSE deadlock analysis + startup/recovery state machines

Authoritative spec: [`redesign.md`](../../redesign.md) §19 (deadlock), §10
(startup), §32.2 (recovery), §4.1 (lifecycle). Companion docs:
[`architecture.md`](architecture.md), [`requirements-checklist.md`](requirements-checklist.md).

This is design, not prose. It defines: the Git-inside-mount deadlock invariants
and how they are enforced in code; the mount **lifecycle** state machine and the
exact idempotent **startup transaction**; the **recovery** procedure. Every
"INV-…" and "REG-…" tag below is a testable invariant that becomes a regression
test (§40, §45.7/§45.8).

Two crates here are **superseded** (§4) and must not be reused: the
`git lazy-mount git --` bridge in `crates/git-store/src/interop.rs` (it stands up
a throwaway operational gitdir, routes object I/O via `GIT_OBJECT_DIRECTORY`,
synthesizes a skip-worktree index, and *adopts* the resulting commit — every one
of these is a §44 anti-claim) and the custom `stage`/`workspace` branch+commit
model. The reusable substrate is `crates/git-store/src/proc.rs::harden_fds`,
`crates/git-store/src/batch.rs::BatchSession`, the `crates/object-provider`
coalescing scheduler, and the durable `crates/overlay`.

---

## 1. Git/FUSE deadlock analysis (§19)

### 1.1 The hazard

Git runs **inside** the mounted worktree (that is the whole point — §1, §2), so
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
   waiting for our callback to return (§23.1).

### 1.2 Invariants (each → a regression test)

These are the §19 invariants made precise. They are enforced by *construction*
(typed policies, a single command-builder, fd hardening), not by review.

- **INV-D1 — No porcelain in callbacks.** A FUSE callback never invokes Git
  porcelain (`status`, `add`, `checkout`, `commit`, `log`, …). The only `git`
  invocations reachable from a callback are the plumbing whitelist in §1.3.
- **INV-D2 — No worktree-scanning commands in callbacks.** Even plumbing that
  walks the worktree (`git status`, `git diff`, `git ls-files -o`, `git
  add`) is forbidden in a callback; these can recurse into the mount.
- **INV-D3 — Never wait on the requester's index lock.** No callback path opens,
  waits on, or stats-then-blocks on `$GIT_DIR/index.lock`. Object/attribute
  resolution in a read uses the bare admin gitdir's object DB and tree-ish
  `.gitattributes` lookups only — never the index (§23.1).
- **INV-D4 — Object readers target the native gitdir directly.** Reads go through
  a long-lived `cat-file --batch-command` against `--git-dir <admin>` with
  `GIT_NO_LAZY_FETCH=1`, serving only locally-present objects
  (`batch.rs:41`). A missing promisor object is reported `missing`, never fetched
  inline.
- **INV-D5 — Only the fetch scheduler causes network.** Exactly one component may
  initiate network retrieval: the `object-provider` fetch scheduler via
  `GitStore::fetch_objects` / `fetch` / `push` (`store.rs:142,188,445`), which run
  with `no_lazy = false` (network allowed) and **off the callback thread** (§18,
  §1.4). No callback transitively reaches a network-enabled `git`.
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
  module docs + `dispatch()`). The worker pool is bounded (§18).
- **INV-D9 — No lock held across `git`/network.** No inode, namespace, handle, or
  state lock is held while a subprocess runs or while the network is touched
  (§18; `object-provider` already documents and tests this — locks dropped before
  `fetcher.fetch`, `lib.rs:301`).
- **INV-D10 — Passive hydration runs no hooks.** A read/getattr never triggers a
  Git hook (§36). Hooks fire only because the user invoked a porcelain command.
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
/// the non-fetching variants by type, not by discipline (§3.13, §16).
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
(distinct thread pool, §18) is the only holder of `AllowNetwork`. This makes
INV-D4/D5 a property of the type system: a callback cannot name a fetching policy
without going through the scheduler.

### 1.4 Thread/queue model (§18) backing the invariants

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
  reused by later callers (one retrieval for N waiters, §20.1/§38.6).

### 1.5 Testable deadlock regressions (REG-D…)

These run through a **real `/dev/fuse` mount** in Linux CI (§40.2) — mocked
callbacks cannot reproduce cycle 1.

- **REG-D1** Open a tracked file on the mount, then run `git status` from inside
  the mount in a subprocess that inherits the open fd; assert no hang (cycle 1).
- **REG-D2** `cat` a missing-promisor file with the daemon `--offline`; assert the
  read returns a clean offline error and the `cat-file` session is **still alive**
  afterward (no fatal exit; `batch.rs` contract).
- **REG-D3** Spawn 100 concurrent reads of one missing blob; assert exactly **1**
  scheduler fetch invocation (`metrics().fetch_invocations == 1`) and all readers
  get the same result (§20.1, `object-provider` coalescing).
- **REG-D4** Hold `index.lock` (a long-running `git add -p` paused at a prompt)
  and concurrently `getattr`/`read` projected files; assert callbacks complete
  without touching the lock (INV-D3).
- **REG-D5** Audit test: enumerate every `Command::new("git")` reachable from a
  callback entry point; assert each sets `GIT_NO_LAZY_FETCH=1` and is in the §1.3
  whitelist (static + runtime hook via a `git` shim that records argv/env).
- **REG-D6** fd audit: after mount, `ls -l /proc/<daemon>/fd`; spawn a `git`
  child; assert the child's fd table contains **no** `/dev/fuse` fd and no
  `cat-file` pipe (INV-D6/D7).
- **REG-D7** A read callback must not invoke any hook: install a tripwire
  `post-index-change`/`reference-transaction` that touches a sentinel file; do a
  pure `cat`; assert the sentinel is untouched (INV-D10).

---

## 2. Mount lifecycle state machine (§4.1)

The prior controller created the mountpoint dir and **immediately** wrote
`MountState::Mounted` (`daemon/src/controller.rs:179` —
`state: MountState::Mounted` with only a `create_dir_all`, *no kernel mount*).
That is exactly the §4.1 / §44 anti-claim "the mount registry says mounted
without a kernel mount." The redesign replaces the coarse
`registry.rs::MountState` (8 states) with the full §4.1 set and forbids `Mounted`
before a kernel mount + Git health checks.

### 2.1 States

```rust
/// Mount lifecycle (§4.1). Persisted in the registry; the daemon is the single
/// writer (§32.1). Only `Mounted` means "kernel mount live + git healthy".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MountState {
    Creating,        // record reserved, preflight running
    Cloning,         // partial clone / fetch in progress
    InitializingGit, // configuring admin gitdir
    BuildingIndex,   // writing the real $GIT_DIR/index from the base tree
    StartingDaemon,  // daemon process / session coming up
    Mounting,        // kernel mount syscall issued, not yet validated
    Validating,      // running §10.6 health checks
    Mounted,         // LIVE: kernel mount + git health both pass
    Quiescing,       // draining in-flight FUSE ops before unmount
    Unmounting,      // kernel unmount in progress
    Recovering,      // §32.2 recovery running
    Failed,          // needs attention; carries a FailureReason
}
```

### 2.2 Transition table

| From | Event | To | Side effects / guard |
|---|---|---|---|
| (none) | `clone <url> <path>` | `Creating` | reserve registry record under workspace lock (§32.1) |
| `Creating` | preflight OK (§10.1) | `Cloning` | — |
| `Creating` | preflight fail | `Failed` | actionable error; record never claimed `Mounted` |
| `Cloning` | clone+fetch OK (§10.2) | `InitializingGit` | base commit/tree resolved |
| `InitializingGit` | config written (§10.5) | `BuildingIndex` | gitfile + `core.worktree` set |
| `BuildingIndex` | index written (§10.4) | `StartingDaemon` | 0 blobs fetched (assert) |
| `StartingDaemon` | session up | `Mounting` | session fd `O_CLOEXEC` (INV-D6) |
| `Mounting` | `mount()` returns | `Validating` | kernel mount exists |
| `Validating` | health checks pass (§10.6) | **`Mounted`** | publish `Mounted`; return success |
| `Validating` | any check fails | `Failed` | unmount kernel; cleanup; keep overlay |
| `Mounted` | `unmount` / shutdown | `Quiescing` | stop accepting new ops |
| `Quiescing` | drained | `Unmounting` | flush overlay, seal journal |
| `Unmounting` | kernel detached | (record `Unmounted`/removed) | idempotent |
| any | daemon start finds non-terminal state | `Recovering` | run §3 recovery |
| `Recovering` | reconciled healthy | `Mounted` or `Unmounting` | per §3 outcome |
| `Recovering` | unrecoverable | `Failed` | quarantine; export path offered |

**INV-L1.** `Mounted` is written **only** from `Validating` after both the kernel
mount exists and all §10.6 checks pass. (Regression REG-L1: kill the daemon
between `Mounting` and `Validating`; assert the registry never shows `Mounted`
and recovery cleans up.)

**INV-L2.** Every state except `Mounted`/`Unmounted` is *non-terminal*: a daemon
that finds the registry in such a state on startup MUST enter `Recovering`, never
assume liveness (REG-L2).

**INV-L3.** State transitions are persisted **before** their externally-visible
effect where a crash-in-between must be recoverable (write-ahead): e.g.
`Mounting` is persisted before the `mount()` syscall so a crash mid-mount is
detectably "maybe mounted" and reconciled against the kernel (§3.4).

```rust
pub enum FailureReason {
    Preflight(String), CloneRejected(String), IndexBuild(String),
    MountSyscall(String), HealthCheck(String), Recovery(String),
}
```

---

## 3. Startup as an idempotent transaction (§10)

Single entry point on the daemon; the CLI `git lazy-mount <url> <path>` calls it
and **blocks until `Mounted` or `Failed`** (§1, §9). Re-running on a partially
created workspace resumes from the persisted state (idempotent §10).

```rust
pub struct StartupRequest {
    pub url: String,            // redacted in all logs (§36)
    pub mountpoint: PathBuf,
    pub opts: CloneOptions,     // filter/branch/depth/allow_full_object_clone (controller.rs:21)
    pub offline: bool,
}
pub struct MountReady {
    pub spec: MountSpec,
    pub base: ObjectId,         // base commit the projection started at
    pub index_stats: IndexBuildStats, // §10.4 honest reporting
}
pub fn start_mount(&self, req: &StartupRequest) -> Result<MountReady>; // → Mounted | Err(Failed)
```

The transaction is the ordered phase list below. Each phase is resumable; each
acquires the **inter-process workspace lock** (`flock` on
`workspaces/<id>/lock`, §32.1) so two `git lazy-mount` invocations cannot race
(in-process mutexes are insufficient, §32.1).

### 3.1 Phase 0 — preflight (§10.1) → `Creating`

Validate, failing fast with an actionable error (never prompting from a
callback; auth may be interactive *here* only, §10.1, §35):

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

### 3.2 Phase 1 — create the Git repository (§10.2) → `Cloning`

Native partial clone into the admin gitdir **outside** FUSE (§6):

```
git clone --filter=blob:none --no-checkout --separate-git-dir=<admin> <url> <temp-anchor>
# full history by default; normal origin/branch/upstream config; then discard <temp-anchor>
```

(Or the equivalent `init --bare` + `fetch` the current code uses,
`controller.rs:90`.) If the remote rejects the filter and
`!allow_full_object_clone`, fail with the §10.2 message
(`controller.rs:129`). **A full-object clone still implies no checkout** (§10.2).
Resolve and record the **base commit/tree** (the attached branch tip).

### 3.3 Phase 2 — initialize working-tree state (§10.3)

Record the baseline + empty overlay + first generation + first FSMonitor token —
no blobs (§38.1):

```
baseline = base commit tree          overlay = empty (crates/overlay)
namespace generation = 1             fsmonitor token = (workspace, epoch=1, seq=0, gen=1)  [§12.1]
```

### 3.4 Phase 3 — initialize the real index (§10.4) → `BuildingIndex`

Build a **full `$GIT_DIR/index`** from the base tree using the admin gitdir.
Correctness-first: O(tracked paths), **fetch zero blobs**. Use
`git read-tree <base-tree>` against the admin gitdir (with `core.worktree` set to
the mountpoint but the mount **not yet live**, so no callbacks fire). Then mark
entries FSMonitor-valid via the bootstrap (§12.2) without hashing worktree
content. Report honestly (§10.4, never market as O(1)):

```rust
pub struct IndexBuildStats {
    pub wall: Duration, pub index_bytes: u64, pub peak_rss: u64,
    pub tree_objects_read: u64, pub blob_objects_fetched: u64, // MUST be 0 (§38.1)
}
```
**INV-S1.** `blob_objects_fetched == 0` during index build (REG-S1).

### 3.5 Phase 4 — configure Git integration (§10.5) → `InitializingGit`

Write the synthetic gitfile and config; preserve user globals (§10.5 — never
touch identity/signing/editor/pager/aliases/credential-helpers/remote policy):

```
<mountpoint>/.git  ⇒  "gitdir: <abs admin gitdir>\n"   (synthetic, protected; §6)
core.worktree=<mountpoint>   core.bare=false
core.fsmonitor=<abs hook path>   core.fsmonitorHookVersion=2
core.untrackedCache=true (after capability test)   index.version=4 (after compat test)
core.fileMode / core.symlinks / core.ignoreCase  ← from mount behavior (validate.rs)
```

The gitfile is a **projected synthetic entry**, not a real file in the overlay;
it has a reserved stable inode (§14) and is protected from
unlink/rename/replace/chmod/write/mkdir-beneath (§6). A tree entry colliding with
`.git` fails safely (§6).

### 3.6 Phase 5 — start + validate the mount (§10.6) → `StartingDaemon` → `Mounting` → `Validating` → `Mounted`

Bring up the session (fd `O_CLOEXEC`), persist `Mounting`, issue the kernel mount
(`fs-fuse::spawn_mount`), persist `Validating`, then run the health checks. These
checks themselves run `git` **inside** the now-live mount, so they exercise the
deadlock invariants (§1) for real:

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

**INV-S2.** The health-check `git status` fetches **0** working blobs (§38.4,
post-bootstrap) (REG-S2). **INV-S3.** A crash at any phase leaves the registry in
a non-terminal state recoverable by §3-recovery with no acknowledged-write loss
(crash-injection matrix §40.5: after each phase boundary).

---

## 4. Recovery procedure (§32.2)

Runs on daemon startup when the registry shows a non-terminal state (INV-L2), on
explicit `git lazy-mount recover <mnt>` / `--export <dir>`, and after a detected
backend/daemon restart (cf. the existing FSKit path
`crates/fs-fskit/src/recovery.rs` — same *shape*, but the redesign’s authority is
the real gitdir + durable overlay, not the oplog). The seven §32.2 steps, in
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

### 4.1 Step 1 — validate the namespace DB (§32.2.1)

Open `state.sqlite` (SQLite WAL, §32) read-write; run integrity checks (`PRAGMA
integrity_check`, foreign-key/parent-index consistency). If the WAL has an
uncommitted tail, let SQLite recover it. On corruption: do **not** discard —
snapshot the DB to the quarantine dir, attempt a parent-indexed rebuild from
overlay metadata records (`crates/overlay` already rebuilds its in-memory index
by scanning persisted `meta/*.json`, ignoring torn temp files — `overlay/src/lib.rs:67`).
Set `fsmonitor_invalidated = true` if the namespace generation can't be trusted.

### 4.2 Step 2 — reconcile temporary content files (§32.2.2)

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

### 4.3 Step 3 — preserve acknowledged writes (§32.2.3) — **the cardinal rule**

**INV-R1.** No recovery step deletes a file that contains an *acknowledged* user
write. A write is "acknowledged" once the FUSE callback returned success to the
kernel; the overlay's two-phase publish guarantees that an acknowledged
`write`/`create`/`rename` has its content **and** metadata durably renamed before
the callback returns (`overlay` ordering above). Recovery confirms each such path
is intact and records it in `preserved_writes`; anything it cannot confirm goes to
quarantine, never to deletion (REG-R1: crash after `write` ack but before
unmount → byte-exact survival across recovery, cf.
`fs-fskit/recovery.rs::reattach_recovers_consistent_state_without_data_loss`).

### 4.4 Step 4 — reconcile mounted state with the kernel (§32.2.4)

Determine the **actual** kernel state independent of the registry:

```
is <mnt> a fuse mount of subtype "glm"?  →  scan /proc/self/mountinfo for the mountpoint+fstype
```

| Registry says | Kernel says | Action |
|---|---|---|
| `Mounting`/`Validating`/`Mounted` | mounted (ours) | adopt or re-validate (§10.6); if validate fails → unmount + `Failed` |
| `Mounting`/`Validating`/`Mounted` | not mounted | crash before/after mount; safe to remount or finish unmount |
| any | mounted but **not ours** (stale/other) | refuse to overwrite; `Failed` with actionable message (§36 mountpoint-substitution guard) |
| `Quiescing`/`Unmounting` | mounted | finish the unmount (idempotent) |

`KernelState = Mounted | NotMounted | Stale`. The `AutoUnmount` option already
mitigates wedged mounts when permitted (`fs-fuse/src/adapter.rs:425`); recovery
handles the cases where it was not.

### 4.5 Step 5 — reconcile native gitdir state (§32.2.5)

The gitdir is **authoritative** (§7); the daemon's parses are disposable caches
(§7). Recovery rebuilds its caches from disk and clears stale native locks left
by an interrupted `git`:

```
stale $GIT_DIR/index.lock  → remove iff no live git holds it (pid/owner check; never steal a live lock)
re-read HEAD, refs/, packed-refs, ORIG_HEAD, FETCH_HEAD, MERGE_HEAD, CHERRY_PICK_HEAD,
        REBASE_HEAD, REVERT_HEAD, BISECT_*, sequencer/, rebase-merge/, rebase-apply/   (§13)
recompute base commit/tree; if HEAD moved while we were down, advance baseline per §8.2 rules
```

**INV-R2.** Recovery never *writes* Git refs/index to "fix" them — it reconciles
its own caches and clears only locks it can prove are abandoned. (The superseded
commit-adoption path is gone, §4.3.)

### 4.6 Step 6 — invalidate FSMonitor continuity if uncertain (§32.2.6)

If *any* of these hold, set `fsmonitor_invalidated = true` so the next FSMonitor
query returns the full-invalidation token `/` (§12 — false positives OK, false
negatives never):

```
journal loss / epoch gap        DB rollback / rebuild      token from another workspace or future generation
journal compaction past a needed token   unreconciled crash (we were non-terminal)   external overlay modification
namespace generation bumped during recovery
```

The redesign’s FSMonitor token is `(workspace, journal-epoch, monotonic-seq,
projection-generation)` (§12.1) — note the current
`crates/fsmonitor/src/lib.rs` journal is an in-memory `Mutex<Vec<…>>` (§4.10
anti-pattern) and must become a durable WAL/append log that survives restart or
returns `/`. Recovery **bumps the journal epoch** when it cannot prove continuity;
an epoch bump deterministically forces `/` for any pre-restart token. Wire format
of the `/`-response (§12): `<new-token>\0/\0`.

### 4.7 Step 7 — quarantine ambiguous files (§32.2.7)

Anything recovery cannot classify as (a) clean baseline, (b) confirmed
acknowledged write, or (c) provably-incomplete temp goes to
`workspaces/<id>/quarantine/<ts>/` with a manifest (original path bytes, source,
reason) — **never deleted**. `recover --export <dir>` copies quarantined +
preserved-but-unmountable content out for the user. Quarantine is also where a
corrupt namespace DB snapshot and orphaned content land.

### 4.8 Recovery outcome

```
all steps healthy, kernel adoptable      → re-validate (§10.6) → Mounted
healthy but registry was Unmounting      → finish Unmounting
unrecoverable (namespace unrebuildable, kernel stale-foreign, validate fails) → Failed + export offered
```

### 4.9 Testable recovery regressions (REG-R…)

- **REG-R1** acknowledged-write survival across crash+recovery (INV-R1), byte-exact.
- **REG-R2** torn temp file (crash between content-rename and meta-rename) ⇒ no
  spurious file, no metadata pointing at absent content (§32.2.2).
- **REG-R3** stale `index.lock` from a killed `git add` ⇒ removed; a *live*
  holder's lock ⇒ **not** stolen (INV-R2 lock-ownership).
- **REG-R4** registry `Mounted` but kernel not mounted ⇒ recovery remounts +
  re-validates, never serves a phantom (INV-L1, §32.2.4).
- **REG-R5** foreign mount occupying the mountpoint ⇒ `Failed`, no overwrite (§36).
- **REG-R6** journal epoch gap ⇒ next FSMonitor query returns `/` (§12, INV-R3).
- **REG-R7** corrupt `state.sqlite` ⇒ rebuilt from overlay meta where possible;
  ambiguous entries quarantined, none deleted (§32.2.1/.7).
- **REG-R8** crash-injection at each §40.5 point (overlay create/write/rename/
  unlink/fsync, index.lock, index replacement, ref txn prepared/committed,
  journal append, registry update, mount success, health check) ⇒ no acknowledged
  data lost; recovery converges to `Mounted`/`Unmounting`/`Failed`.

**INV-R3.** Uncertainty is always resolved toward *more* invalidation and *no*
deletion: FSMonitor returns `/` rather than risk a false-negative; files are
quarantined rather than removed. This is the single safety bias of recovery.

---

## 5. Invariant → test index (summary)

| Tag | Invariant | §ref | Regression |
|---|---|---|---|
| INV-D1..D2 | no porcelain / worktree-scan in callbacks | §19 | REG-D5/D7 |
| INV-D3 | never wait on requester index lock | §19,§23.1 | REG-D4 |
| INV-D4 | object readers hit native gitdir, NO_LAZY_FETCH | §19 | REG-D2 |
| INV-D5 | only fetch scheduler causes network | §19,§18 | REG-D3 |
| INV-D6..D7 | session/mount fds CLOEXEC, not inherited | §19 | REG-D6 |
| INV-D8 | callbacks never block dispatch loop | §18 | REG-D1 |
| INV-D9 | no lock across git/network | §18 | (provider test) |
| INV-D10 | passive hydration runs no hooks | §36 | REG-D7 |
| INV-L1 | `Mounted` only after kernel mount + health | §4.1 | REG-L1 |
| INV-L2 | non-terminal states ⇒ recover on startup | §4.1 | REG-L2 |
| INV-L3 | write-ahead state persistence | §4.1 | REG-S3 |
| INV-S1 | index build fetches 0 blobs | §10.4,§38.1 | REG-S1 |
| INV-S2 | health-check status fetches 0 blobs | §38.4 | REG-S2 |
| INV-S3 | crash-at-any-phase recoverable, no loss | §10,§40.5 | REG-R8 |
| INV-R1 | acknowledged writes never deleted | §32.2.3 | REG-R1 |
| INV-R2 | recovery never rewrites git state / steals live locks | §32.2.5 | REG-R3 |
| INV-R3 | uncertainty ⇒ invalidate + quarantine, never delete | §32.2 | REG-R6/R7 |
