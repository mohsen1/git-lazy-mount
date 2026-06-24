# Durable FSMonitor v2 protocol + hook multiplexing

This area of the [specification](design.md) covers FSMonitor v2 and observing the
gitdir without replacing Git, with supporting context from config, the real index,
durability, and synthetic metadata. This is the M0/M3 design doc for the FSMonitor
durability protocol.

This is a **design**, not a refactor. The existing
[`crates/fsmonitor/src/lib.rs`](../../crates/fsmonitor/src/lib.rs) is a
process-local `Mutex<Vec<ChangeRecord>>` journal — exactly the non-durable shape
the design forbids ("A process-local `Mutex<Vec<ChangeRecord>>` is not a sufficient
FSMonitor implementation"). It is superseded wholesale. The
`git lazy-mount git --` interop bridge in
[`crates/git-store/src/interop.rs`](../../crates/git-store/src/interop.rs) is
also superseded: there is no wrapper, so FSMonitor must serve **stock
`git`** invoked directly inside the mount.

## 0. Where this fits

| Owner | Responsibility |
|-------|----------------|
| **Git** (native admin gitdir) | calls our FSMonitor hook; owns `.git/index` FSMonitor-valid bits, untracked cache, `HEAD`, refs |
| **Daemon** | the only FSMonitor server; owns the durable change journal, the projection generation, the hook multiplexer's notification side, gitdir watchers, and reconcile-on-restart |
| **Hooks** (tiny binaries) | FSMonitor query client; notification multiplexer (`post-index-change`, `reference-transaction`, `post-checkout`, …) |

Git configures at mount creation, via
[`GitStore::set_config`](../../crates/git-store/src/store.rs):

```
core.fsmonitor        = <abs path to glm-fsmonitor-hook>   # the query client
core.fsmonitorHookVersion = 2
core.untrackedCache   = true                               # after capability test
core.hooksPath        = <abs path to glm-hooks-dir>        # multiplexer dir
```

`core.fsmonitor` points at our **client binary**, never at a long script. The
binary does one round-trip to the daemon and prints the response: the hook should
remain a tiny IPC client. Heavy work belongs in the daemon.

---

## 1. The FSMonitor v2 wire protocol (Git ↔ client)

Git's FSMonitor v2 hook contract (the protocol our client implements toward
Git):

**Request** (argv to the hook): `argv[1] = "2"` (version), `argv[2] = <prev
token>` — an opaque string we minted on a previous query, or empty/`""` on first
use.

**Response** (hook stdout, exactly):

```
<new-token> NUL <path1> NUL <path2> NUL ... <pathN> NUL
```

- The leading token is everything up to the **first NUL**.
- After it, zero or more **NUL-separated, repo-root-relative** paths.
- Paths use `/`; bytes are emitted **verbatim** — never lossy-UTF-8.
  The daemon stores `RepoPath`
  ([`crates/core/src/path.rs`](../../crates/core/src/path.rs)) and writes
  `as_bytes()` directly to the pipe.
- The set is **inclusive**: it must contain every path that *might* have changed
  since `prev token`. **False positives are acceptable; false negatives are
  never acceptable**. A returned path that did not actually change only
  costs Git an extra `lstat`; a missing path corrupts `git status`.
- Directory paths in the response invalidate Git's untracked cache for that
  directory; the daemon includes a path's **parent directory** whenever
  a child is created/removed/renamed so directory mtime staleness cannot hide an
  untracked-cache entry.

### 1.1 Full-invalidation sentinel

When the daemon cannot prove continuity from `prev token` to now, the response
is a single path `/`:

```
<new-token> NUL / NUL
```

`/` tells Git to treat **the entire worktree** as possibly-changed and rescan
(it clears all FSMonitor-valid bits and statts everything). This is always
**correct but eager** — it is the safety valve, never a steady state.

### 1.2 Client binary (`glm-fsmonitor-hook`)

Tiny, synchronous, no `git`, no worktree scan:

```rust
// crate: glm-fsmonitor-hook (new), behind glm-ipc
fn main() -> ExitCode {
    let version = argv(1);            // "2"
    let prev    = argv(2).into_bytes(); // opaque; possibly empty
    if version != "2" { print_full_invalidation(); return ExitCode::SUCCESS; }
    let sock = resolve_daemon_socket_for_cwd();  // peer-cred authed
    match sock {
        Ok(s) => {
            // single request/response, bounded timeout
            let resp = s.query(FsmonRequest { version: 2, prev });
            match resp {
                Ok(r) => { stdout().write_all(&r.token); stdout().write_all(b"\0");
                           for p in r.paths { stdout().write_all(&p); stdout().write_all(b"\0"); } }
                Err(_) => print_full_invalidation(), // fail safe, never panic
            }
        }
        Err(_) => print_full_invalidation(),         // daemon down → full rescan
    }
    ExitCode::SUCCESS // exit 0: Git treats nonzero as "rescan everything" anyway
}
```

Invariants: the client **never** blocks indefinitely (bounded timeout, default
2 s; on timeout → full invalidation), **never** returns nonzero with garbage on
stdout, and is **CLOEXEC-clean** — it must not inherit the FUSE session fd (the
deadlock hazard in
[`crates/git-store/src/proc.rs::harden_fds`](../../crates/git-store/src/proc.rs)).
Because Git spawns it from inside the mount, the client must **not** touch
the worktree at all (no `cwd`-relative file reads beyond `.git` discovery).

The IPC message lives next to the existing control protocol
([`crates/ipc/src/lib.rs`](../../crates/ipc/src/lib.rs)). A new `RequestOp`
variant or a dedicated `fsmon` framed channel:

```rust
// glm-ipc
pub struct FsmonRequest { pub version: u8, pub prev: Vec<u8> }   // prev = opaque token bytes
pub struct FsmonResponse { pub token: Vec<u8>, pub paths: Vec<Vec<u8>> } // paths NUL-free
```

---

## 2. Token identity

A token is opaque **to Git** but **structured for the daemon**. It must identify
all four axes the token contract requires:

```rust
// glm-fsmonitor
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FsmonToken {
    pub workspace: WorkspaceId,     // on-disk <workspace-id>; see glm_core::WorkspaceId
    pub epoch: JournalEpoch,        // u64; bumped on any journal-loss / rebuild
    pub seq: u64,                   // monotonic record sequence at mint time
    pub projection: MountGeneration,// glm_core::MountGeneration; baseline/projection gen
}
```

Wire form (the bytes Git stores and replays — fixed, parseable, version-tagged):

```
glm1:<workspace-id>:<epoch>:<seq>:<projection>
```

- `glm1` is a format tag; a token without it → full invalidation (handles a
  token minted by an incompatible daemon version).
- `workspace-id` is the stable on-disk id
  ([`WorkspaceId`](../../crates/core/src/ids.rs)). A token whose `workspace`
  ≠ the daemon's current workspace → full invalidation (token from another
  workspace).
- `epoch` (`JournalEpoch(u64)`, new) is the **journal incarnation**. It is bumped
  whenever the durable journal cannot serve continuity from `seq 0` of the new
  incarnation (rebuild, compaction past the floor, corruption recovery). A token
  whose `epoch` ≠ current → full invalidation.
- `seq` is the journal's monotonic record sequence — the analogue of the
  existing `ChangedPathJournal::capture_sequence`, but now durable.
- `projection` is [`MountGeneration`](../../crates/core/src/ids.rs), bumped on
  baseline advancement / projection rebuild. A token from a **future**
  projection (`> current`) → full invalidation (token from a future
  generation); a token from a strictly older projection is acceptable **only**
  if every path that differs between the old and new projection has been
  journaled; otherwise full invalidation.

`epoch` strictly dominates `seq`: `seq` is only comparable **within** the same
`(workspace, epoch)`. This prevents a stale `seq` from a previous incarnation
being read as "recent".

---

## 3. Server algorithm (daemon side of the query)

```
fn serve_fsmon(prev: Vec<u8>) -> FsmonResponse:
    new = current_token()                       // mint AFTER capturing seq (barrier)
    cur = mint snapshot { epoch, seq, projection, workspace }
    match parse(prev):
        Err(_)                          -> return full(new)         # unparseable
        Ok(t) if t.workspace != cur.workspace -> return full(new)
        Ok(t) if t.epoch    != cur.epoch      -> return full(new)   # journal incarnation changed
        Ok(t) if t.projection > cur.projection -> return full(new)  # future generation
        Ok(t) if t.projection < cur.projection && !projection_delta_journaled(t.projection)
                                              -> return full(new)
        Ok(t) if t.seq > cur.seq              -> return full(new)   # impossible unless rollback
        Ok(t):
            if t.seq < journal.compaction_floor -> return full(new) # compacted past request
            paths = journal.paths_changed_in(t.seq .. cur.seq)      # inclusive window
            return FsmonResponse { token: serialize(new), paths }
```

`full(new)` returns `{ token: serialize(new), paths: vec![b"/".to_vec()] }`.

### 3.1 Full-invalidation triggers (complete list)

The daemon returns `/` for **any** of:

1. **Journal loss / rebuild** — the durable journal file was missing, truncated,
   or failed checksum on open (epoch bumped).
2. **Database rollback** — SQLite WAL rolled back below the last acked `seq`
   (detected: on-open `seq` < persisted high-water).
3. **Token from another workspace** — `token.workspace != current`.
4. **Token from a future generation** — `token.projection > current` (or
   `token.epoch`/`token.seq` ahead of durable state).
5. **Journal compaction beyond the requested token** — `token.seq <
   compaction_floor`.
6. **Unreconciled daemon crash** — daemon restarted and the crash-recovery
   reconciliation has not yet completed; queries during recovery return
   `/`.
7. **Backend event overflow** — the OS gitdir watcher (`inotify`/`fanotify`)
   reported a queue overflow (`IN_Q_OVERFLOW`) → we may have missed gitdir
   events → epoch is **not** bumped (worktree journal is intact) but the next
   query returns `/` once, then resumes (we mark a one-shot `overflow_pending`).
8. **External overlay modification** — the overlay store
   ([`crates/overlay/src/lib.rs`](../../crates/overlay/src/lib.rs)) was mutated
   out-of-band (mtime/inode of `overlay/` changed without a corresponding daemon
   write) — we cannot trust the journal's completeness.
9. **Unparseable / wrong-tag token** — any `prev` that does not start with
   `glm1:` or fails to parse.
10. **Version mismatch** — Git requested a protocol version we do not serve.

Every trigger is a **named, testable** branch (see the invariants below).

### 3.2 What gets journaled

The journal records exactly the mutation classes the protocol enumerates, sourced from
**FUSE callbacks** (the authoritative incremental feed — the daemon *sees* every
worktree write because it serves the filesystem) and from the
**gitdir watchers + notification hooks** for Git-driven worktree changes:

```
file creation                 unlink
content modification          rename: BOTH old and new name
truncation                    directory creation
chmod affecting Git mode      directory deletion
symlink creation/replacement  directory rename: old and new subtree roots
```

For a **rename**, journal both endpoints. For a **directory rename**, journal
the old and new directory paths *and* (inclusively) it is acceptable — and
cheaper — to journal just the two subtree roots and let Git rescan beneath them;
the response stays correct because Git treats a returned directory as
"everything under here might have changed". For any create/remove/rename, also
journal the **parent directory** (untracked-cache).

### 3.3 Record shape (replaces the in-memory `ChangeRecord`)

```rust
// glm-fsmonitor, durable
pub struct JournalRecord {
    pub seq: u64,            // monotonic within (workspace, epoch)
    pub epoch: u64,
    pub path: RepoPath,      // raw bytes; from_bytes-validated
    pub kind: ChangeKind,    // Created | Modified | Removed | Renamed
    pub also: Option<RepoPath>, // rename counterpart / parent dir
}
```

`paths_changed_in(lo..hi)` returns the **deduplicated** set of `path` + `also`
for `lo < seq <= hi`. Dedup is a correctness-neutral optimization (inclusive set
is unchanged); it bounds response size.

### 3.4 Projection-delta journaling

When the baseline advances (a checkout-like op updated the worktree, bumping
`MountGeneration`), the set of paths that *differ between old and new baseline*
must be journaled **before** the new projection generation is observable, so a
client holding an old-`projection` token gets those paths (not a silent
false-negative). The daemon computes this delta via `git diff-tree` against the
**native gitdir** (`GIT_NO_LAZY_FETCH=1`, never porcelain) at hook time
(`post-checkout`/`post-merge`). If the delta cannot be computed cheaply
(huge delta), the daemon **bumps epoch** instead → next query is `/`. This keeps
branch switches "correct but possibly eager", never wrong.

### 3.5 Testable invariants (→ regression tests)

- **I-NOFALSE-NEG**: For any sequence of worktree mutations between two queries,
  every actually-changed path appears in the second response (property test over
  random op sequences vs. a ground-truth oracle diffing a real checkout).
- **I-INCLUSIVE-OK**: A response superset of the true change set is accepted
  (false positives don't fail tests).
- **I-FULL-ON-RESTART**: Kill the daemon mid-journal; first post-restart query
  before reconciliation completes returns `/`.
- **I-FOREIGN-TOKEN**: A token minted by workspace A, replayed against
  workspace B, returns `/`.
- **I-FUTURE-GEN**: A token with `projection > current` returns `/`.
- **I-COMPACTION-FLOOR**: After compacting past `seq=k`, a token with `seq<k`
  returns `/`; a token with `seq>=k` returns the precise window.
- **I-EPOCH-DOMINATES**: A token from a prior epoch with a numerically-larger
  `seq` than the current epoch's `seq` still returns `/` (no cross-epoch `seq`
  comparison).
- **I-RAW-PATHS**: A changed path containing invalid UTF-8 / newline / tab is
  emitted byte-exact and NUL-delimited.
- **I-BOOTSTRAP-ZERO-BLOB**: First and every subsequent clean `git status`
  fetches **zero** blob contents (see the bootstrap section below).

---

## 4. Bootstrap: FSMonitor-valid without hashing

The design requires the **first** clean `git status` (and all later clean ones) to
fetch zero blobs and avoid statting every file. The challenge: Git normally only
sets a path's FSMonitor-valid bit after it has confirmed the path clean via a
stat/hash. The bootstrap:

1. After `init real index` — a full index built from the initial tree
   with **no blob fetches** — the index entries are valid-by-construction: every
   path equals its committed blob (overlay is empty in the initial state).
2. The daemon mints the **initial token** `glm1:<ws>:<epoch0>:<seq0=0>:<gen0>`.
3. On the first `git status`, Git calls the hook with an **empty** `prev`. An
   empty/unknown prev would normally mean `/` — but the daemon recognizes the
   **distinguished empty-prev bootstrap** when the journal is at `seq 0` of
   `epoch0` and overlay is empty: it returns an **empty path list** with the
   initial token, asserting "nothing changed since the index was built".
4. Git then trusts the index entries as FSMonitor-clean and sets their valid
   bits **without hashing** (it only stats paths *returned* as changed — here,
   none).

This is sound because the daemon **owns the filesystem** and **knows** no
worktree write has occurred since index construction (any write would have
incremented `seq`). The "empty prev = full invalidation" default is overridden
**only** in the proven-quiescent bootstrap case; if any FUSE write or hook
notification has advanced `seq` past 0, empty-prev → `/` as usual.

Invariant **I-BOOTSTRAP-ZERO-BLOB** above gates this.

---

## 5. Barrier semantics

A query must reflect every worktree mutation that the kernel **acknowledged**
before the query was issued. Because FUSE writes are processed asynchronously by
the daemon's bounded executor, there is a window where a write is acked to
the application but not yet journaled. The barrier closes it.

`current_token()` (called at the top of `serve_fsmon`) executes a **drain
barrier**:

```
fn current_token():
    cut = executor.capture_inflight_seq()   // highest FUSE-op seq admitted so far
    executor.await_journaled(cut)           // block until all ops <= cut are durable in journal
    return FsmonToken { workspace, epoch, seq: journal.high_water(), projection: gen }
```

The three modes from the legacy `SyncMode` map onto Git's needs:

| Mode | When | Behavior |
|------|------|----------|
| `Barrier` (default for FSMonitor queries) | every `git status` | wait until all ops captured before the query are journaled, then mint the token |
| `BestEffort` | diagnostics / `glm stats` | incorporate already-journaled ops, do not wait |
| `NoWait` | liveness probe | return the latest token, no drain |

Git's FSMonitor query always uses `Barrier`: a `git status` issued *after* an
editor's `write()+fsync()` returned must see that change. The barrier waits on
the **journal**, not on fsync of file data — FSMonitor continuity needs the
*namespace/seq* durable, not the bytes (durability separates these; an un-fsynced byte
write that was acked is still journaled).

**Invariant I-BARRIER-VISIBLE**: a write whose FUSE reply was sent before a
query is always in that query's response (concurrency test: editor write
concurrent with `git status`).

The barrier must **never** hold the journal write lock while waiting for the
executor (deadlock); it waits on a per-`seq` completion condvar, then takes
the lock only to read `high_water`.

---

## 6. Hook multiplexing

Git fires notification hooks for state changes we must observe:
`post-index-change`, `reference-transaction`, `post-checkout`, `post-merge`,
`post-commit`, `post-rewrite`, `post-applypatch`. We must **chain, not
overwrite** the user's hooks.

### 6.1 Layout

We set `core.hooksPath = <managed-hooks-dir>` (per workspace). Each managed hook
is the **same tiny multiplexer binary** (`glm-hook`), hard-linked/symlinked under
every hook name we care about. The user's original hooks are discovered (not
moved) at multiplex time:

```
<managed-hooks-dir>/
  post-index-change   -> glm-hook   (argv[0] names the hook)
  reference-transaction -> glm-hook
  post-checkout       -> glm-hook
  post-merge          -> glm-hook
  post-commit         -> glm-hook
  post-rewrite        -> glm-hook
  post-applypatch     -> glm-hook
```

The user's hooks are resolved from their **original** location: the value of
`core.hooksPath` *as it was before we set ours* (captured at mount time into
`mount.json`), else `<gitdir>/hooks`. We never edit files in the user's hook
directory.

### 6.2 Multiplexer algorithm (`glm-hook`)

```rust
fn main() -> ExitCode {
    let hook_name = basename(argv0());          // e.g. "post-checkout"
    let args: Vec<OsString> = argv[1..];
    let stdin = read_all(stdin());              // buffered once; replayed to user hook

    // (1) bounded, fire-and-forget notification to the daemon — never blocks Git
    if std::env::var_os("GLM_HOOK_REENTRANT").is_none() {
        let _ = notify_daemon(HookEvent {        // best-effort, bounded timeout
            hook: hook_name, args: &args, stdin: &stdin, cwd: cwd(),
        }); // failure is ignored: hooks are an optimization, not correctness
    }

    // (3) recursion guard: if WE are already inside a user hook that ran git,
    //     do not re-multiplex.
    if std::env::var_os("GLM_HOOK_REENTRANT").is_some() {
        return ExitCode::SUCCESS; // already handled at the outer layer
    }

    // (2) invoke the user's original hook with original argv/stdin/env, set guard
    match resolve_user_hook(hook_name) {
        Some(path) => {
            let status = Command::new(path)
                .args(&args)
                .env("GLM_HOOK_REENTRANT", "1")  // (3) prevent recursion
                .stdin_from(&stdin)              // replay captured stdin
                // stdout/stderr/env inherited; user hook behaves natively
                .status();
            // (4) propagate the USER hook's exit status verbatim
            ExitCode::from(status.code().unwrap_or(1) as u8)
        }
        None => ExitCode::SUCCESS,  // no user hook → success
    }
}
```

### 6.3 The four hook-chaining requirements, mapped

1. **Bounded notification to the daemon** — `notify_daemon` has a bounded
   timeout and is fire-and-forget; a slow/absent daemon never delays Git.
2. **Invoke the user hook with original args/stdin/env/exit semantics** —
   captured `stdin` replayed; `argv[1..]` forwarded; environment inherited;
   user hook's exit code propagated **verbatim**.
3. **Prevent recursive invocation** — `GLM_HOOK_REENTRANT=1` is set across the
   user-hook child. If the user hook itself runs `git` (which re-fires hooks
   from inside the mount), the inner `glm-hook` sees the env var, skips both the
   daemon notify and the user-hook invocation, and exits 0. This also stops the
   FUSE→git→hook→git cycle.
4. **No daemon locks held while the user hook runs** — the notification is a
   single bounded request that **returns before** the user hook is spawned; the
   daemon processes the event on its own threads and releases any journal lock
   immediately. `glm-hook` holds nothing.

**Exit-status invariant**: "Provider notification hooks that cannot affect
Git's result must not alter the user hook's intended exit status." So: if there
is **no** user hook, `glm-hook` exits 0 (our notification must not turn a hookless
operation into a failure). If there **is** a user hook, we return *its* code
unchanged, even on daemon-notify failure.

### 6.4 What each notification tells the daemon

| Hook | Daemon action |
|------|---------------|
| `post-index-change` | re-parse the real index: stage-0/1/2/3, skip-worktree, FSMonitor-valid bits, checksum — a **disposable cache** rebuild, never authoritative |
| `reference-transaction` | invalidate cached `HEAD`/refs parse; arrives at `prepared` and `committed` phases (read on `committed`) |
| `post-checkout` | baseline may have advanced: compute the projection delta, bump `MountGeneration`, journal the delta |
| `post-merge` | merge completed: baseline advanced; same delta handling |
| `post-commit` | refs moved; index unchanged-worktree; re-parse refs |
| `post-rewrite` (amend/rebase) | history rewrite; refs + possibly worktree changed |
| `post-applypatch` | worktree updated by `am` |

These are **optimizations and synchronization aids, not the only correctness
mechanism** — if a hook is missed, the gitdir watchers + reconcile-on-
restart and the `/` safety valve still keep FSMonitor sound.

### 6.5 Testable invariants

- **I-HOOK-CHAIN**: a user-installed `post-commit` that writes a file / exits N
  still runs, sees original argv+stdin, and `git commit` observes exit code N.
- **I-HOOK-NO-OVERWRITE**: mounting does not modify or delete any file in the
  user's hooks directory or unset their `core.hooksPath` value (we save+restore).
- **I-HOOK-NO-RECURSION**: a user hook that runs `git status`/`git log` does not
  cause unbounded hook re-entry (env-guard test).
- **I-HOOK-EXIT-NEUTRAL**: with **no** user hook, every notification hook exits 0
  even when the daemon socket is unreachable.
- **I-HOOK-NONBLOCKING**: with the daemon paused, `git commit` still completes
  within the notify timeout (hook does not hang Git).

---

## 7. Native gitdir watching + reconcile-on-restart

Hooks can be missed (daemon down during a Git command, `core.hooksPath`
overridden by a one-off `-c`, signals). So the daemon also **watches** native
admin-gitdir state and **reconciles from disk** on restart.

### 7.1 Paths to watch

Inside the native gitdir (`<workspaces>/<id>/git/`, **not** inside FUSE),
watch via `inotify`/`fanotify`:

```
index            index.lock          HEAD
packed-refs      refs/   (recursive)  logs/  (recursive)
MERGE_HEAD       CHERRY_PICK_HEAD     REVERT_HEAD
REBASE_HEAD      ORIG_HEAD            FETCH_HEAD
sequencer/       rebase-merge/        rebase-apply/
```

(Exactly the administrative-state list.) An event on these means Git changed
state we mirror in disposable caches; the daemon re-reads the affected piece
(index re-parse on `index`/`index.lock` removal; ref re-parse on `refs/`,
`packed-refs`, `logs/`; sequencer/rebase state on the merge/rebase files). A
watcher **queue overflow** triggers full-invalidation trigger #7.

Watching the index is belt-and-suspenders for the `post-index-change` hook:
whichever fires, the daemon re-parses once (debounced by index checksum).

### 7.2 Reconcile-on-restart

On daemon startup (before serving any FSMonitor query — queries return `/` until
this completes, trigger #6):

```
1. open + validate the durable journal; on failure → bump epoch (full inval)
2. re-parse the real index from the native gitdir into the disposable cache
3. re-read HEAD / refs / packed-refs; rebuild ref cache
4. re-read sequencer/rebase/merge state
5. reconcile the overlay namespace DB and validate overlay/ mtime vs.
   recorded high-water; mismatch → trigger #8 (full inval, bump nothing else)
6. recompute current MountGeneration / projection identity
7. mark reconciliation complete; resume FSMonitor service
```

After reconciliation, the daemon **cannot** prove which worktree mutations
happened while it was dead, so the **first** post-restart query returns `/`
(I-FULL-ON-RESTART). The journal's `epoch` is preserved across a *clean*
shutdown (so a quiesce/remount can serve a precise window), and bumped only on
*detected* loss — this is the distinction between "tokens must survive
daemon restarts" (clean) and "or the daemon must return a full-invalidation
response" (crash).

### 7.3 Testable invariants

- **I-RECONCILE-INDEX**: stop daemon, `git add` a file via a *second* short-lived
  process against the gitdir, restart daemon → re-parsed index reflects the new
  stage-0 entry without rewriting the index.
- **I-WATCH-REFMOVE**: a ref update with hooks disabled (`git -c core.hooksPath=
  /dev/null update-ref`) is still observed via the `refs/` watcher.
- **I-OVERFLOW-FULL**: a synthetic `IN_Q_OVERFLOW` yields one `/` then resumes.
- **I-CLEAN-RESTART-PRECISE**: a *clean* quiesce+restart with an unchanged gitdir
  serves a precise (non-`/`) window for a token minted before shutdown.

---

## 8. Durability

The journal is **durable** — the central fix vs. the legacy in-memory `Vec`.

### 8.1 Storage

The journal is a durable append log **or** SQLite WAL, under
`<workspaces>/<id>/journal/`. Recommended: **SQLite WAL** (already a workspace
dependency via the namespace DB; `rusqlite`):

```sql
CREATE TABLE fsmon_journal (
  seq    INTEGER PRIMARY KEY,   -- monotonic within epoch
  epoch  INTEGER NOT NULL,
  path   BLOB    NOT NULL,      -- raw RepoPath bytes (not TEXT: non-UTF-8)
  kind   INTEGER NOT NULL,
  also   BLOB                   -- rename counterpart / parent dir, nullable
);
CREATE TABLE fsmon_meta (
  k TEXT PRIMARY KEY, v BLOB
); -- holds: epoch, high_water_seq, compaction_floor, projection_gen, overlay_mtime_hwm
```

- `path`/`also` are **BLOB** so arbitrary bytes round-trip — no UTF-8
  coercion (the bug class the design forbids).
- `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;` — WAL gives append-like
  durability; `NORMAL` is sufficient because FSMonitor continuity tolerates
  losing the *tail* of un-checkpointed records **as long as we detect it**: on
  open, if `MAX(seq)` < persisted `high_water_seq`, that is a **rollback**
  (trigger #2) → bump epoch. We never silently serve a short journal.
- A single-writer discipline: the **daemon** is the only writer; hooks
  and the CLI go through IPC, never open the journal DB directly. Interprocess
  lock (the workspace lock) guards epoch bumps and compaction.

### 8.2 Append + barrier ordering

A FUSE mutation's journal append is committed (WAL frame durable) **before** the
FUSE reply is sent for operations whose acknowledgment implies durability
(`fsync`, atomic rename publish), so the barrier can guarantee
`await_journaled`. For ordinary buffered writes, the append is committed before
the corresponding `seq` is reported as journaled; the barrier waits on that.

### 8.3 Compaction

The journal grows unbounded otherwise. Compaction deletes records below a
**floor** once no plausible client token references them:

```
compaction_floor = max(seq retained)   // advanced under the workspace lock
```

A retention window (e.g. keep the last N records or T seconds, configurable;
default generous) bounds size. Any token with `seq < compaction_floor` → `/`
(trigger #5). Compaction **never** changes `epoch` (the incarnation is
unbroken); it only raises the floor. Compaction must be crash-safe: write the new
floor to `fsmon_meta` in the same transaction that deletes the rows.

### 8.4 Testable invariants

- **I-DURABLE-SURVIVES-RESTART**: append records, clean-restart the daemon, a
  pre-restart token gets a precise window (no `/`) when nothing else changed.
- **I-ROLLBACK-DETECTED**: truncate the WAL tail below `high_water_seq` → next
  open bumps epoch → `/`.
- **I-NO-UTF8-COERCION**: a journaled non-UTF-8 path BLOB round-trips byte-exact
  through SQLite and out the wire (joins the path tests).
- **I-COMPACTION-CRASH-SAFE**: crash mid-compaction never leaves the floor ahead
  of the surviving records (rows+floor in one txn).

---

## 9. Security / transport notes

- The FSMonitor client and `glm-hook` connect to the **per-user** daemon socket,
  authenticated by socket ownership + peer credentials (guarding against
  control-socket impersonation); a query/notification from another uid is
  rejected.
- Both binaries are **CLOEXEC-clean** and must not inherit the FUSE session fd
  (reuse the `harden_fds` discipline from
  [`crates/git-store/src/proc.rs`](../../crates/git-store/src/proc.rs)).
- Notification payloads (hook stdin can carry ref names / commit data) are
  size-bounded before sending; oversized stdin is truncated for the *daemon
  notification* only — the **user hook always receives the full original stdin**.
- Passive hydration **never** runs hooks: hooks run only because the user
  invoked a Git command. The FSMonitor query path runs **no** `git` and touches
  **no** worktree content, so it cannot trigger hydration or filters.

---

## 10. Crate plan

- `crates/fsmonitor/` (rewrite): `FsmonToken`, `JournalEpoch`, durable
  `Journal` (SQLite WAL), `serve_fsmon`, the barrier, compaction. Replaces the
  in-memory `ChangedPathJournal`. Keeps the `SyncMode`/`ChangeKind`
  vocabulary; drops `Mutex<Vec<_>>`.
- `crates/fsmonitor-hook/` (new, tiny): the `core.fsmonitor` client binary.
- `crates/git-hooks/` (new): the `glm-hook` multiplexer
  binary + hooks-dir installer (save/restore the user's `core.hooksPath`).
- `crates/ipc/` (extend): `FsmonRequest`/`FsmonResponse` + `HookEvent` framed
  messages, BLOB-safe (`Vec<u8>`) like the existing `fs.rs` shapes.
- `crates/daemon/` (extend): owns the `Journal`, the gitdir watchers, the
  reconcile-on-restart state machine, and the notification-event handlers.

Superseded and removed: legacy `ChangedPathJournal`
([`crates/fsmonitor/src/lib.rs`](../../crates/fsmonitor/src/lib.rs)) and the
interop bridge
([`crates/git-store/src/interop.rs`](../../crates/git-store/src/interop.rs)) —
neither survives the no-wrapper, durable-FSMonitor design.
