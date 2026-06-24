# Overlay durability, recovery journal, auth/offline, security

Authoritative spec: [`redesign.md`](../../redesign.md), primarily §32 (overlay
storage and durability), §33 (optional operation journal), §35 (auth/offline),
§36 (security model), §22 (stable synthetic metadata + racy-clean). Read
alongside [`architecture.md`](architecture.md) (baseline+overlay model,
two-sources-of-truth) and [`requirements-checklist.md`](requirements-checklist.md).

This document covers the daemon's **own** durable state — the writable working
tree. Git's gitdir owns refs/index/reflogs/commits (§7); we never duplicate
them and never journal them here (§33). The redesign **supersedes** the old
custom-stage / commit-adoption / `git lazy-mount git --` bridge: the operation
journal in this design is a *crash-recovery* artifact, not a second history.

Crate map (target §41 layout vs. existing): `overlay/` keeps native content +
the namespace DB; `fsmonitor/` keeps the durable token journal; `daemon/` keeps
mount ownership/locking/recovery; `object-provider/` keeps offline policy;
`platform/` keeps path/security validation. The existing `crates/oplog`,
`crates/stage`, and `crates/git-store/src/interop.rs` (the skip-worktree commit
bridge) are **removed**: their journal *shape* (atomic-write + `CURRENT` + crash
points) is reused as the recovery-journal substrate (§33), but the
view-history/commit-adoption semantics are not.

---

## 1. Overlay storage layout (§32)

Native files hold content; a transactional SQLite-WAL database holds the
namespace. **Large content is never stored in SQLite** (§32). The daemon is the
**single writer** (§32.1).

```
~/.local/share/git-lazy-mount/workspaces/<id>/
  git/                       real native gitdir (NOT ours, NOT in FUSE)
  namespace.sqlite           the namespace DB  (WAL mode)
  namespace.sqlite-wal
  namespace.sqlite-shm
  overlay/
    content/
      ab/<content-id>        native content blobs, sharded by first byte
      ab/<content-id>.tmp    in-flight (reconciled/quarantined on startup)
  filtered-cache/            validated working-tree representations (§20.2)
  journal/
    recovery.sqlite          recovery journal (§33) — WAL, bounded, compactable
  fsmonitor/
    tokens.sqlite            FSMonitor durability journal (§12.1) — WAL
  quarantine/                ambiguous files preserved by recovery (§32.2 step 7)
  mount.json                 daemon-written mount record (IPC-only writers, §32.1)
  locks/                     advisory lockfiles (§32.1)
  logs/
```

The current `crates/overlay/src/lib.rs` stores each entry as a per-path JSON
file under `meta/` plus a content file under `content/`, addressed by
`sha256(path bytes)`. That atomic-write-content-then-metadata discipline
(`overlay::atomic_write`, lib.rs:249) is correct and is **kept**; the change is
to move the *namespace* (the `meta/*.json` set) into the transactional DB so a
multi-path rename/subtree operation is one atomic commit instead of N
independent file renames (§15, §29 subtree rename). Content blobs stay as native
files exactly as today (lib.rs:147–161).

### 1.1 Content identity

Content is addressed by an opaque **content id** (a random 128-bit nonce, hex),
*not* by `sha256(path)` and *not* by blob OID. Decoupling content id from path
is what makes rename/subtree-rename O(namespace rows) with **zero content
copies** (§29): a rename rewrites the `path → content_id` mapping, the content
file is untouched. (The current `id_for(path)` scheme forces a content rename on
every path rename; the redesign drops it.)

```rust
/// Opaque, location-independent handle to one overlay content file.
pub struct ContentId([u8; 16]);          // random; hex = overlay/content/<aa>/<id>
```

### 1.2 Namespace DB schema (§32 field list)

One row per overlay entry. `path` is **raw bytes** (`BLOB`), never UTF-8 (§31).

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;          -- see §1.4 for the fsync ordering rule
PRAGMA foreign_keys = ON;

CREATE TABLE entries (
  ino            INTEGER PRIMARY KEY,    -- stable inode identity (§14)
  generation     INTEGER NOT NULL,       -- bumped on delete+recreate (§14)
  parent_ino     INTEGER NOT NULL,       -- parent identity (directory namespace, §15)
  name           BLOB    NOT NULL,       -- final component, raw bytes
  kind           INTEGER NOT NULL,       -- File|Symlink|Dir|BaseRef|Tombstone|Gitlink
  executable     INTEGER NOT NULL DEFAULT 0,   -- Git-relevant mode bit only
  content_id     BLOB,                   -- NULL for dirs/tombstones/base-refs
  base_oid       BLOB,                   -- BaseRef only: referenced blob (§29 clean rename)
  base_mode      INTEGER,                -- BaseRef only
  dir_generation INTEGER NOT NULL DEFAULT 0,   -- bumped on direct-child change (§22)
  open_unlinked  INTEGER NOT NULL DEFAULT 0,   -- retained-but-unnamed (§17.4)
  rename_src     BLOB,                   -- diagnostic provenance only
  size_hint      INTEGER,                -- validated exact size if known (§22, §21)
  created_unix   INTEGER NOT NULL,
  modified_unix  INTEGER NOT NULL,
  UNIQUE (parent_ino, name)              -- one entry per (parent,name)
);
CREATE INDEX entries_by_parent ON entries(parent_ino);
CREATE INDEX entries_by_content ON entries(content_id);

CREATE TABLE meta (k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID;
-- meta keys: schema_version, projection_generation, baseline_commit,
--            inode_high_water, fsmonitor_epoch
```

`kind` enumerates `OverlayKind` from `overlay/src/lib.rs:27` plus `Dir` and
`Gitlink`. `BaseRef` (lib.rs:41) is preserved verbatim — it is the clean-rename
optimization (§29): place content at a new path referencing an existing blob OID
with **no fetch**.

Invariant **NS-1** (`UNIQUE(parent_ino,name)`): a directory never lists two
entries with the same name; case/normalization collisions are handled by the
platform layer (`platform::validate::detect_collisions`, validate.rs:223), not by
the DB.

### 1.3 Why SQLite, what stays native

| Data | Store | Reason |
|------|-------|--------|
| namespace rows (small, transactional, queried by parent) | SQLite WAL | atomic multi-row rename/subtree (§15, §29); `O(direct children)` readdir |
| file/symlink **content** | native file in `overlay/content/` | §32 "do not store large file contents in SQLite"; streaming FD I/O (§17, §38.8) |
| filtered working-tree representations | native file in `filtered-cache/` | §20.2; validated + atomically published |
| FSMonitor tokens/events | SQLite WAL (`fsmonitor/tokens.sqlite`) | §12.1 durable token journal |
| recovery journal | SQLite WAL (`journal/recovery.sqlite`) | §33; bounded, compactable |

### 1.4 Write protocol and the durability ladder (§32, §17.6)

A single namespace mutation:

```
1. content   -> write overlay/content/<id>.tmp, fsync FILE
2. content   -> rename .tmp into place (publish)                         [DataFsynced]
3. namespace -> BEGIN IMMEDIATE; upsert/delete rows; COMMIT (WAL)        [MetadataCommitted]
4. (optional) directory fsync of overlay/content/<shard>
```

Content is durable **before** the namespace row that references it — so a crash
never yields a row pointing at absent/torn content (the existing
overlay invariant, lib.rs:13). Order is the inverse of delete: to remove
content, first delete the row (COMMIT), then unlink the content file; a crash
leaves an unreferenced orphan, never a dangling reference.

Durability levels (the existing `Durability` ladder, `core/src/state.rs:103`, is
reused, dropping `OperationSealed` which belonged to the superseded op-history):

```rust
pub enum Durability { InMemory, Journaled, DataFsynced, MetadataCommitted }
```

We only claim crash durability for writes the application actually `fsync`ed
beyond ordinary fs guarantees (§17.6). `flush`/`release` publish to the overlay
but do **not** force `fsync` unless the app called `fsync`/`fdatasync`.

Invariant **DUR-1**: after a `COMMIT` returns, the entry and its content survive
`SIGKILL` of the daemon and a remount. (Crash-injection test, §40.5.)

Invariant **DUR-2**: an acknowledged `write()`/`fsync()` from a user process is
never silently lost; on ambiguous recovery the bytes are quarantined, not
deleted (§32.2 step 7).

---

## 2. Single writer, interprocess locking (§32.1)

The daemon is the authoritative overlay writer. CLI tools and hooks **never**
open the namespace DB for writing or rewrite `mount.json`; they go through IPC
(`crates/ipc`, the versioned control protocol, ipc/src/lib.rs:18). In-process
mutexes alone are insufficient (§32.1): use OS advisory file locks.

### 2.1 Lock set and wire shape

Each lock is an `flock`-style exclusive lock on a file under `locks/` (Linux
`flock(2)` / `fcntl` `F_OFD_SETLK`; the lock file records the holder for
diagnostics). Locks are CLOEXEC so a spawned `git`/filter never inherits them
(§19).

| Lock file | Guards | Held by | Held during |
|-----------|--------|---------|-------------|
| `locks/mount.lock`     | mount ownership — one daemon owns the mount | daemon | whole `mounted` lifetime |
| `locks/startup.lock`   | daemon startup race | starting daemon | preflight → live |
| `locks/migration.lock` | namespace DB schema migration | migrator | migration only |
| `locks/recovery.lock`  | crash recovery pass | recoverer | recovery only |
| `locks/git-init.lock`  | administrative gitdir creation (§10.2) | initializer | clone/init only |

```rust
pub enum LockScope { Mount, Startup, Migration, Recovery, GitInit }

/// RAII exclusive interprocess lock. `try_acquire` returns `MountLifecycle`
/// (EBUSY-class, error.rs:119) with the recorded holder pid in `context`.
pub struct WorkspaceLock { scope: LockScope, _fd: OwnedFd }
impl WorkspaceLock {
    pub fn try_acquire(ws_dir: &Path, scope: LockScope) -> Result<WorkspaceLock>;
    pub fn holder(ws_dir: &Path, scope: LockScope) -> Option<HolderInfo>; // pid, started_at
}
pub struct HolderInfo { pub pid: u32, pub started_unix: i64, pub boot_id: String }
```

Invariant **LOCK-1**: at most one process holds `mount.lock` for a workspace; a
second `mount`/daemon attempt fails fast with `MountLifecycle` and names the
holder, rather than corrupting the DB.

Invariant **LOCK-2**: a **stale** lock (holder pid dead, or `boot_id` differs
from current boot) is breakable only by the recovery path under
`recovery.lock`, never silently. `boot_id` (Linux `/proc/sys/kernel/random/boot_id`)
distinguishes "process still alive" from "machine rebooted, pid reused" — a
stale-PID-file attack vector (§36).

### 2.2 `mount.json` and the registry

`mount.json` and the user-level registry (`crates/daemon/src/registry.rs`) are
written **only** by the daemon, via the same temp-file + fsync + atomic rename
discipline already in `registry.rs:86` (`Registry::store`). `MountState`
(registry.rs:12) must reach `Mounted` only after a real kernel mount + Git health
checks (§4.1, §10.6) — the registry must never assert `Mounted` without a kernel
mount (§44). CLI `list`/`doctor` read it; they never write it.

---

## 3. Recovery (§32.2) — startup state machine

Recovery runs under `recovery.lock` (§2.1) before the mount goes live. It never
deletes acknowledged user data (§32.2 step 3, DUR-2).

State table (the `MountState` set, registry.rs:12, drives this):

| State | Entry condition | Actions | Exit |
|-------|-----------------|---------|------|
| `Recovering` | daemon start finds a non-clean shutdown marker, or `recover` CLI | run steps R1–R7 below | → `Mounting` (ok) / `Failed` (unrecoverable) |
| `Mounting` | recovery clean | start FUSE | → `Mounted` / `Failed` |
| `Mounted` | kernel mount + Git health (§10.6) pass | serve | — |
| `Failed` | unrecoverable inconsistency | preserve everything; surface diagnostic | operator / `recover --export` |

Recovery steps (§32.2):

```
R1 validate namespace DB        SQLite integrity_check + schema_version;
                                roll the WAL forward (SQLite does this);
                                if corrupt -> Failed, DB never truncated.
R2 reconcile content files      every entries.content_id must have a content
                                file; orphan content files (no row) -> quarantine,
                                NOT delete. Dangling rows (no file) -> the row's
                                bytes were never durable per DUR-1; mark the path
                                for full-invalidation, do not fabricate content.
                                *.tmp files: a finished write renamed them; any
                                surviving .tmp is incomplete -> quarantine.
R3 preserve acknowledged writes any content file that R2 keeps and a row
                                references is preserved verbatim (DUR-2).
R4 reconcile kernel mount       is the mountpoint actually a FUSE mount? lazy-
                                umount stale mounts; never report Mounted without
                                a kernel mount (§44).
R5 reconcile native gitdir      compare meta.baseline_commit to the gitdir HEAD
                                via long-lived cat-file (§19); if HEAD moved while
                                we were down (external git), baseline is behind ->
                                force FSMonitor full-invalidation (R6).
R6 FSMonitor continuity         if any of R1–R5 is uncertain, bump fsmonitor_epoch
                                so the next token mismatches -> "/" full response
                                (§12, §3.7 below).
R7 quarantine ambiguous         move-aside into quarantine/ with a manifest; never
                                in-place delete.
```

```rust
pub struct RecoveryReport {
    pub healthy: bool,
    pub quarantined: Vec<QuarantinedItem>,   // path-or-content-id, reason
    pub forced_full_invalidation: bool,      // R6 fired
    pub baseline_behind: bool,               // R5
    pub issues: Vec<String>,                 // redacted
}
pub struct QuarantinedItem { pub content_id: ContentId, pub reason: String, pub bytes: u64 }
```

CLI (§32.2):

```
git lazy-mount recover <mountpoint>
git lazy-mount recover <mountpoint> --export <dir>   # copy quarantine/ + any
                                                     # recoverable uncommitted
                                                     # working files out
```

The existing `oplog::recover` (oplog/src/lib.rs:275) and the FSKit `reattach`
report (fs-fskit/src/recovery.rs:34) are the structural template (`healthy` /
`stale` / `issues`), retargeted from the superseded view-history to the namespace
DB + content reconciliation above.

Invariant **REC-1**: recovery is idempotent — running it twice yields the same
result and never re-quarantines already-quarantined items.

Invariant **REC-2**: recovery never requires network (DUR / §35: dirty overlay
content never depends on network for recovery — see §6.4).

---

## 4. Recovery journal (§33) — strictly bounded purpose

A filesystem **recovery** journal is allowed. It must **not** become a second
Git history (§33). Its only purposes (§33):

```
overlay namespace crash recovery
mount lifecycle transitions
FSMonitor continuity
diagnostic audit
recovery of uncommitted working files
```

It explicitly does **not** record: commits, branch moves, ref updates,
merge/rebase progress — Git's refs and reflogs are the history of those (§33,
§7). We do **not** build a Jujutsu-style operation log here (§33 last line); the
existing `crates/oplog` view-history and `crates/stage` are removed.

### 4.1 Shape

WAL SQLite at `journal/recovery.sqlite`, append-mostly, **compactable**:

```sql
CREATE TABLE journal (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,   -- monotonic within an epoch
  epoch        INTEGER NOT NULL,                    -- bumped on each clean start / R6
  ts_unix      INTEGER NOT NULL,
  kind         INTEGER NOT NULL,   -- ContentWritten|NamespaceCommitted|Renamed|
                                   -- Unlinked|Lifecycle|FsmonitorEpochBump|Quarantine
  path         BLOB,               -- raw bytes, may be NULL (lifecycle)
  content_id   BLOB,
  detail       BLOB                -- small CBOR; never file contents (§36 redaction)
);
CREATE TABLE journal_meta (k TEXT PRIMARY KEY, v BLOB);
```

The journal entry is appended in the **same** SQLite transaction as the
namespace row mutation (§1.4 step 3) — one `COMMIT` covers both, so the journal
can never disagree with the namespace it describes.

### 4.2 Compaction

Once the namespace DB is checkpointed and durable to `MetadataCommitted`, journal
rows older than the last checkpoint are unreferenced and may be pruned. Pruning
is purely a recovery/audit optimization; losing old rows costs nothing because
the namespace DB is itself the authoritative state.

Invariant **JRN-1**: the journal is never the source of truth for working-tree
bytes — dropping `journal/` entirely still leaves a fully usable workspace
(namespace DB + content files). Tested by deleting the journal and asserting the
mount still serves correct content.

Invariant **JRN-2**: the journal contains no secrets and no file contents (§36
redaction) — `detail` is bounded structured metadata only.

---

## 5. Stable synthetic metadata + racy-clean (§22)

For unmaterialized **clean** files we synthesize stable metadata; the existing
`FileAttr` / `unix_mode_of` (fs-common/src/attr.rs) is the shape. The redesign
nails down *stability* and *racy-clean* (§22), which are durability concerns
because Git compares index timestamps against what we project.

### 5.1 Stable values within a projection generation

```rust
pub struct SyntheticMeta {
    pub ino: u64,            // from the namespace inode table (§14); stable per identity
    pub generation: u64,     // bumped only on delete+recreate (§14)
    pub mode: u32,           // 0o100644/0o100755/0o120777/0o040755 (attr.rs:44)
    pub uid: u32,            // daemon's euid (private mount, §36)
    pub gid: u32,            // daemon's egid
    pub mtime: SyntheticTime,
    pub ctime: SyntheticTime,
    pub size: SizeState,     // Unknown until first getattr that needs it (§21)
}

/// One fixed timestamp per projection generation, NOT wall-clock-on-read.
pub enum SyntheticTime { ProjectionEpoch(u64) }   // -> a fixed unix time per generation

pub enum SizeState { Unknown, Exact(u64) }        // never fake a size (§21)
```

Rules (§22):

- **STBL-1**: for a given `(workspace, projection_generation, path identity)`,
  `getattr` returns byte-identical `ino/mode/uid/gid/mtime/ctime` across repeated
  lookups. The synthetic mtime/ctime is the **projection epoch's** fixed time,
  derived from `meta.baseline_commit` commit time (or mount time), *not* the
  current clock — re-reading a clean file must not advance its mtime.
- **STBL-2**: a directory's `mtime`/`dir_generation` (the `dir_generation` column,
  §1.2) changes when a **direct** child is created/removed/renamed (§22, §15) —
  but not when an unrelated dirty path elsewhere changes. This is what lets Git's
  untracked cache stay valid (§12.3). The existing constant-mtime hazard
  (§4.9 / §12.3 "do not use one constant synthetic directory mtime forever") is
  fixed by bumping `dir_generation` on direct-child mutation.
- **STBL-3**: `size` is `Unknown` until a `getattr` that genuinely needs it; we
  never fabricate a size to dodge hydration (§21), and `readdir` never asks for
  it (§4.5, §38.2). Once known it is recorded in `entries.size_hint` and is
  stable.

### 5.2 Racy-clean (§22)

Git treats an index entry as **racy-clean** when the file's mtime is `>=` the
index timestamp; it then re-hashes the file to decide clean-vs-dirty. Two
hazards:

1. If our synthetic mtime equals the index write time, Git re-hashes the file on
   the next status → a clean file triggers content hydration (violates §38.4
   "clean status fetches zero blobs").
2. If we ever move a synthetic mtime *forward* on a passive read, Git flags a
   clean file modified (violates §22 "do not mark a path modified merely because
   a synthetic timestamp differs").

Mitigation:

- **RACY-1**: synthetic mtimes are set to the **projection epoch**, which is
  strictly **earlier** than any index Git writes after mount, so clean
  unmaterialized files are never racy. The FSMonitor bootstrap (§12.2) marks
  initial entries FSMonitor-valid so status skips the mtime check entirely on the
  clean path.
- **RACY-2**: when a file *is* materialized (overlay content), its mtime is the
  real native file mtime (the overlay file's `stat`, like `content_len`,
  overlay/src/lib.rs:215) — honest, and FSMonitor reports the change so Git
  knows to look.
- **RACY-3**: never narrow racy detection by lying; if exact size/mtime is
  unknown we report the *stable synthetic* value, and any real edit goes through
  the overlay + FSMonitor journal, so Git sees it through the change feed, not a
  timestamp guess.

Tested against a real `git status --porcelain=v2` differential vs. a normal
checkout at the same commit (§40.1): clean status fetches zero blobs (§38.4),
and an edit is reported exactly once.

---

## 6. Authentication and offline (§35)

The principle (§10.1, §35): **interactive auth only at the `mount` command;
every FUSE callback is non-interactive.** This is already enforced at the
process layer — `git-store::proc::run` sets `Stdio::null()` for stdin and
hardens fds (proc.rs:84), so a spawned `git` "never inherits a terminal (so
credential prompts cannot appear)".

### 6.1 Interactive at mount, non-interactive after

- `git lazy-mount <url> <path>` may use the user's normal credential helper
  interactively during `git-init.lock` / clone (§10.2, §35). The initial clone is
  the only place a prompt is allowed.
- After mount, the **only** code allowed to cause network retrieval is the fetch
  scheduler in the object provider (§19, §20.1); it runs non-interactively.

### 6.2 FetchPolicy gate (the mechanism)

Filesystem callbacks pass a non-fetching policy so a read can never prompt or
even touch the network. This is implemented today: `FetchPolicy::may_fetch()`
(core/src/fetch.rs) and the provider's `ensure_present_locally`
(object-provider/src/lib.rs:137) return an offline error instead of fetching when
the policy forbids it.

```rust
// object-provider/src/lib.rs — callback path uses CacheOnly/MustNotFetch:
if !policy.may_fetch() { return Err(offline(id)); }  // OfflineMissingObject
```

| Caller | FetchPolicy | May prompt? | May hit network? |
|--------|-------------|-------------|------------------|
| `mount` clone | (git's own) | yes (helper) | yes |
| FUSE read/getattr | `CacheOnly`/`MustNotFetch` | **no** | only via scheduler, non-interactive |
| explicit `prefetch` | `AllowNetwork`/`Prefetch` | no (uses cached creds) | yes |
| `git fetch`/`pull`/`push` (user-run) | git's own | yes | yes |

### 6.3 Credential expiry (§35)

When the scheduler's fetch fails with expired credentials, `git-store::classify`
already maps it to `ErrorCode::Authentication` with the action "refresh
credentials (e.g. `git lazy-mount doctor`) and retry" (proc.rs:152, error.rs).
Required behavior (§35):

```
1. return a bounded filesystem error      -> Authentication -> errno EACCES(13)
                                             (error.rs:102), NOT a hang.
2. record the failed object + cause        -> daemon AuthFailureState (per repo_id)
3. surface a daemon diagnostic             -> git lazy-mount doctor / stats
4. allow refresh without remount           -> IPC CredentialRefresh{repo_id}
                                             (ipc/src/lib.rs:83); or a normal
                                             user-run `git fetch` re-primes the helper
5. retry subsequent reads                  -> Authentication is retryable
                                             (error.rs:88 default_retryable)
```

```rust
pub struct AuthFailureState {
    pub repo_id: RepoId,
    pub failed_oids: BoundedSet<ObjectId>,   // capped; for diagnostics
    pub last_cause: ErrorCode,               // Authentication
    pub since_unix: i64,
}
```

Invariant **AUTH-1**: a read of a missing object with expired credentials returns
a bounded errno promptly; it never blocks a FUSE callback waiting for an
interactive prompt (§35, §18 cancellation).

Invariant **AUTH-2**: after `CredentialRefresh` (or a user `git fetch`), a
previously failing read of the same object succeeds without unmount/remount
(§35 "allow doctor or normal git fetch to refresh credentials; retry").

### 6.4 Offline mode and prefetch (§35)

```
git lazy-mount <url> <path> --offline       # mount; never fetch on callbacks
git lazy-mount prefetch <path> --for-offline # warm caches for offline use
```

- `--offline`: the daemon pins every callback policy to `CacheOnly`. Cached
  content stays readable; a missing object returns a **clear**
  `OfflineMissingObject` error (object-provider/src/lib.rs:368, `offline()`),
  errno `EIO(5)`, with action "prefetch while online or rerun without --offline".
- `prefetch --for-offline`: walks the requested subtree's trees and enqueues all
  reachable blobs through the scheduler with `FetchPriority::Prefetch`, then
  filters+caches them so an offline read needs no network.

Invariant **OFF-1**: **dirty overlay content never depends on the network** (§35
last line, §32.2). Overlay files are self-contained native bytes; recovery (§3)
and reads of locally-written content never call the fetcher. Tested: take a
workspace offline, edit/create files, kill the daemon, recover — all dirty bytes
intact, zero fetch attempts (REC-2).

Invariant **OFF-2**: a `BaseRef` entry (clean rename of an unmaterialized file,
§1.1) whose blob is absent offline returns `OfflineMissingObject` on **content**
read, but its **namespace** presence (lookup/readdir/rename) never needs network
(§38.9 clean rename fetches zero blobs).

---

## 7. Security model (§36)

Repository data is **untrusted** (§36). Threats and mitigations:

| §36 threat | Mitigation | Where |
|------------|------------|-------|
| path traversal | `RepoPath` rejects `..`, absolute, empty components, NUL | core/src/path.rs (`PathError`) |
| symlink races | overlay writes never follow repo symlinks; write to `content/` by content-id, never via a repo-relative path; O_NOFOLLOW on internal opens | §30.1; §1.1 |
| malicious tree names | raw-byte paths, no shell construction, NUL-delimited plumbing | core/src/path.rs; §31 |
| case/normalization attacks | `collision_key` / `detect_collisions`; `UNIQUE(parent,name)` NS-1 | platform/src/validate.rs:188,223 |
| cache poisoning | filtered-cache + tree-cache keyed by content-addressed digests; atomic publish; validate on read | metadata/src/lib.rs; §20.2 |
| corrupt object responses | `LocalObjectCorruption` on integrity failure | error.rs:28 |
| decompression bombs | bounded decode in object provider; size caps before materialize | §36; provider |
| unbounded filter output | resource limits + timeouts on external filters | §23.3; §7.2 below |
| hung filters | filter timeout -> `FilterFailure` (errno EIO) | error.rs:69; §23.3 |
| credential leakage | URLs/headers/tokens redacted everywhere (§7.3) | repo_id.rs:13; proc.rs:138 |
| control-socket impersonation | Unix socket owner-only perms + peer-cred check | §7.1 below; ipc |
| stale PID files | `boot_id`-aware stale detection (LOCK-2) | §2.1 |
| mountpoint substitution | preflight validates mountpoint ownership/emptiness/nesting | platform/src/validate.rs; §10.1 |
| unsafe repo ownership | gitdir + workspace dirs are 0700, owned by euid | §7.4 below |

### 7.1 Control socket (§9, §36 control-socket impersonation)

The daemon's Unix-domain control socket (ipc, §9) is created in a 0700
user-private directory, the socket file is mode 0600, and every connection is
authenticated by **peer credentials** (`SO_PEERCRED` on Linux): the connecting
uid must equal the daemon's uid. The protocol is versioned
(`PROTOCOL_VERSION`, ipc/src/lib.rs:18) so a mismatched client is rejected, not
misparsed. No root privileges are required (§9).

Invariant **SEC-1**: a process with a different uid cannot drive the control
socket (peer-cred check), and the socket is never world-accessible.

### 7.2 External filter / hydration safety (§23.2, §36)

Passive hydration must **never** unexpectedly execute an untrusted command
(§23.2) and must **never run Git hooks** (§36). Filter trust policy (§23.2):

```rust
pub enum FilterTrust { Trusted, BuiltinsOnly, ErrorOnExternal, Raw }
```

- At mount, detect whether projected reads may need an executable filter (§23.2);
  default to `BuiltinsOnly`. An external filter under `BuiltinsOnly`/`ErrorOnExternal`
  yields `FilterFailure` rather than silently executing repo-controlled code.
- External filters that *are* permitted run with resource limits + timeouts
  (§23.3); a timeout/over-limit is `FilterFailure`/`ResourceLimit`
  (error.rs:69/54).
- The provider already smudges with `GIT_NO_LAZY_FETCH` and pre-faults
  `.gitattributes` so a filter cannot trigger a recursive lazy-fetch subprocess
  that deadlocks a callback (object-provider/src/lib.rs:189,268 — §19).

Invariant **SEC-2**: **passive hydration never runs a Git hook** (§36). Hooks run
only because the user invoked a Git command that normally runs them. The daemon's
hydration path spawns only `cat-file`-class object readers and (policy-permitting)
declared filters — never `git` porcelain (§19), and never the hooks directory.

Invariant **SEC-3**: passive hydration never executes an untrusted external
command (§23.2) — under the default policy, an unknown external filter errors
instead of running.

### 7.3 Redaction (§36)

Redact in **all** outputs (logs, errors, journal, stats):

```
credentials in URLs        -> repo_id strips user:pass (repo_id.rs:126 test proves
                              `secrettoken` never survives)
authorization headers      -> never logged
secret query parameters    -> stripped with the URL
private paths when configured -> path redaction list (§36)
file contents              -> never in errors/journal (Error.summary "must not
                              contain secrets", error.rs:140; JRN-2)
```

The error model already carries only **redacted** breadcrumbs
(`ErrorRepr.context`, "caller must ensure it is redacted", error.rs:212) and
`git-store::classify` keeps "only the first few lines and never echo[es] full
URLs verbatim" (proc.rs:138).

Invariant **SEC-4**: no credential, token, authorization header, or file content
appears in any log line, error JSON, journal row, or `stats`/`trace`/`doctor`
output. (Regression: feed a URL with an embedded token and a file with a secret
through clone+read+error paths; assert the token/secret never appears in any
emitted bytes.)

### 7.4 Private directories (§36)

All cache and workspace directories are **private to the user** (§36): created
mode 0700, owned by the daemon's euid; content files 0600. The data roots
(`platform::DataRoots`, roots.rs) place them under `XDG_STATE_HOME` /
`~/Library/Application Support` / `%LOCALAPPDATA%`. Preflight refuses to operate
on a workspace dir owned by another uid or with group/other write bits
(unsafe-ownership, §36).

Invariant **SEC-5**: `namespace.sqlite`, `overlay/`, `filtered-cache/`,
`journal/`, and `quarantine/` are never group/other readable or writable.

---

## 8. Testable invariants (regression suite)

Each becomes a regression test (§40), most through a **real** `/dev/fuse` mount
(§40.2) and differential against a normal checkout (§40.1).

Durability / overlay (§32, §40.5 crash injection):
- **DUR-1** committed namespace+content survives `SIGKILL` + remount.
- **DUR-2** an acknowledged user `write`/`fsync` is never silently lost; ambiguous
  files are quarantined, not deleted.
- **NS-1** no two entries share `(parent_ino, name)`.
- crash injected after each of: content `.tmp` create, content publish, namespace
  COMMIT, rename, unlink, fsync (§40.5) — no acknowledged data lost; DB never
  corrupt.
- non-UTF-8 path content survives reopen (already tested,
  overlay/src/lib.rs:323 `non_utf8_path_overlay`).
- clean rename fetches zero blobs (`BaseRef`, §38.9, OFF-2).

Locking / recovery (§32.1, §32.2):
- **LOCK-1** second daemon on the same mount fails with `MountLifecycle`, names
  the holder.
- **LOCK-2** a stale lock (dead pid / new `boot_id`) is broken only under recovery.
- **REC-1** recovery is idempotent.
- **REC-2** recovery needs no network.
- **JRN-1** deleting `journal/` leaves a usable workspace.
- **JRN-2** journal rows contain no secrets/contents.

Synthetic metadata / racy-clean (§22):
- **STBL-1** `getattr` is byte-stable across reads within a generation.
- **STBL-2** directory mtime/`dir_generation` changes on direct-child change only.
- **STBL-3** size is never fabricated; `readdir` never requests it.
- **RACY-1** clean unmaterialized files are never racy → clean status fetches zero
  blobs (§38.4).

Auth / offline (§35):
- **AUTH-1** read with expired creds returns a bounded errno, never hangs.
- **AUTH-2** `CredentialRefresh` / user `git fetch` re-enables reads without remount.
- **OFF-1** dirty overlay content never depends on network for recovery.
- **OFF-2** offline read of an absent blob returns a clear `OfflineMissingObject`.

Security (§36):
- **SEC-1** non-owner uid cannot drive the control socket.
- **SEC-2** passive hydration never runs a Git hook.
- **SEC-3** passive hydration never runs an untrusted external filter (default policy).
- **SEC-4** no credential/secret/content in any log/error/journal/stats output.
- **SEC-5** workspace/cache dirs are user-private (0700/0600).
