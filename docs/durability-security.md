# Overlay durability, auth/offline, security

This area of the [specification](design.md) covers how the writable overlay
reaches the disk durably, how authentication and offline behaviour are gated so
a filesystem callback never prompts, and the security model for treating
repository data as untrusted. Read alongside
[`architecture.md`](architecture.md) (baseline + overlay model,
two-sources-of-truth).

Scope. This document owns only the *working-tree bytes* the mount writes. Git's
gitdir owns refs, index, reflogs, and commits; we never duplicate or journal
them here. Related depth lives in its canonical owners:

- baseline/overlay/tombstone/`BaseRef` model and rename semantics:
  [`worktree-model.md`](worktree-model.md)
- `materialize_path`, single-flight, `smudge_blob`, exact size/metadata:
  [`object-fetching.md`](object-fetching.md)
- the FSMonitor seed, change journal, and zero-blob first status:
  [`fsmonitor.md`](fsmonitor.md)
- startup sequence and FUSE/git deadlock-avoidance:
  [`deadlock-startup-recovery.md`](deadlock-startup-recovery.md)
- the per-path `Durability`/`Residency` state axes:
  [`git-state-model.md`](git-state-model.md)

---

## 1. Overlay storage layout

The overlay (`crates/worktree/src/overlay.rs`) records local working-tree changes
on top of the read-only baseline: created/modified files, symlinks, explicit
(empty) directories, deletions (tombstones), and clean-rename base-refs. It owns
working-tree **bytes** only — never Git state.

Storage is split:

- **Content** lives in native files under `overlay/content/`, addressed by an
  opaque content id and served by FD, never buffered whole.
- **Namespace** is a parent-indexed map persisted as **one atomic JSON sidecar
  per entry** under `overlay/meta/`, named `id_for(path) = sha256(path bytes) +
  ".json"` (`overlay.rs` `id_for`). The in-memory index (`path -> OverlayEntry`
  plus a parent → child-names map for `O(direct children)` listing) is a
  disposable cache rebuilt from the sidecars on `Overlay::open`.

```
<workspace>/overlay/
  meta/
    <sha256(path)>.json     one sidecar per overlay entry (raw path + entry)
    <sha256(path)>.tmp      in-flight write (temp+fsync+rename)
  content/
    c<pid>-<seq>            native content blobs, served by FD
```

There is **no** SQLite database, no `namespace.sqlite`, no `entries` table, and
no SQL schema anywhere in the tree. The entry shape is the `OverlayEntry` enum:

```rust
pub enum OverlayEntry {
    File { content: String, executable: bool },  // bytes in overlay/content/<content>
    Symlink { target: Vec<u8> },                 // raw target stored inline
    Dir,                                          // explicit (e.g. created empty) dir
    Tombstone,                                    // a deleted baseline path
    BaseRef { oid: ObjectId, mode: GitMode },     // clean-rename target: an existing blob
}
```

A corrupt sidecar is skipped at open (the path falls through to the baseline)
rather than failing the whole mount.

### 1.1 Content identity and clean rename

Content is keyed by `id_for(path) = sha256(path bytes)` for its sidecar, and the
content file itself carries an opaque sequential id (`new_content_id`). A path
rename re-keys the overlay entry to the destination sidecar while **keeping** the
content file (`Overlay::rename`), so a materialized file's bytes are not copied.

A **clean** rename of an *unmaterialized* file fetches **zero blobs**: the
projection records a `BaseRef { oid, mode }` at the destination (the existing
baseline blob, by OID) and a tombstone at the source — no content is fetched or
copied. The detailed rename rules (RENAME_NOREPLACE honoured, RENAME_EXCHANGE
rejected, directory/subtree rename metadata-only) live in
[`worktree-model.md`](worktree-model.md).

> Considered / not built (possible future): keying content by a
> path-independent random nonce so even a *materialized* subtree rename touches
> only the namespace. Today the content file is kept across a rename but the
> sidecar is re-keyed per path; a single transactional namespace store would let
> a multi-path rename commit atomically instead of as N independent sidecar
> renames. Neither the nonce nor a namespace DB exists in the code.

### 1.2 Write protocol and durability ordering

One overlay mutation writes content first, then the namespace sidecar that
references it, each atomically:

```
1. content   -> create overlay/content/<id> (the write path does not fsync it;
                it is made durable only when the app calls fsync/fdatasync)
2. namespace -> write meta/<id>.tmp, fsync it, rename into place,
                then fsync the parent directory so the rename is durable
```

`atomic_write` (`overlay.rs`) performs the temp-write + `sync_all` +
`rename` + best-effort parent-directory `sync_all`; the parent-dir fsync is what
keeps an acknowledged create/rename from being lost after the file itself was
fsynced. Content is durable **before** the sidecar that references it, so a crash
never leaves a sidecar pointing at absent or torn content. Deletion is the
inverse order (drop the sidecar, then unlink the content file), so a crash leaves
at most an unreferenced orphan, never a dangling reference.

The per-path `Durability` ladder is defined in `crates/core/src/state.rs`. It is
a 5-variant ordered enum; a higher level implies all lower guarantees:

```rust
pub enum Durability { InMemory, Journaled, DataFsynced, MetadataCommitted, OperationSealed }
```

We only claim crash durability for writes the application actually `fsync`ed.
`flush`/`release` publish to the overlay but do not force an `fsync` unless the
app called `fsync`/`fdatasync`.

### 1.3 Change journal

The projection optionally carries a `ChangeJournal`
(`crates/worktree/src/journal.rs`): a durable NUL-separated **append log** at
`<gitdir>/glm-fsmonitor/changes.log`, replayed into an in-memory `Vec` on open.
`record()` is synchronous (`write_all` + `sync_data`) *before* the FUSE reply, so
a recorded change is on disk before the mutation is acknowledged. This journal
feeds FSMonitor continuity, not a second history; its token form and
full-invalidation rules are owned by [`fsmonitor.md`](fsmonitor.md).

Known hardening gap: the journal has **no compaction** — `State.paths` is kept
whole, so the log and its in-memory replay grow unbounded over the life of a
mount. Epoch and generation are hard-coded `1` and `0` and never bumped;
epoch-bump-on-crash is an explicit future refinement.

---

## 2. Process model (no daemon, no IPC)

There is no daemon and no IPC/control socket. `git lazy-mount <url> <path>`
clones, builds the index, seeds FSMonitor, then spawns a **detached hidden
`__serve` child** (`crates/cli/src/main.rs`) whose stdio is nulled and which is
reparented to init and not waited on. That child opens the `AdminRepo`,
`ChangeJournal`, and `Projection`, then calls `glm_fuse::mount`, which blocks
until unmount. The only CLI verbs are the default mount form, `Unmount`,
`Doctor`, and the internal `__serve`; there is no `list`, `recover`, or
`prefetch`, and no `--offline` flag.

Single-writer discipline is in-process: one `__serve` child holds the mount and
is the only overlay writer. There is no `WorkspaceLock`, no `locks/` directory,
no `flock` set, and no `boot_id` stale-lock logic.

> Considered / not built (possible future): a long-lived daemon owning the mount
> with a versioned Unix control socket (peer-credential authenticated), an
> interprocess `flock` lock set with `boot_id`-aware stale detection, and a
> `MountState` startup recovery state machine with a `RecoveryReport` and a
> `recover` subcommand. None of this exists; startup and its deadlock-avoidance
> invariants are documented in
> [`deadlock-startup-recovery.md`](deadlock-startup-recovery.md).

---

## 3. Authentication and offline

The principle: **interactive auth only at the `mount` command; every FUSE
callback is non-interactive.** This is enforced at the process layer.

- The clone in `AdminRepo::clone` (`crates/git-repo/src/lib.rs`) may use the
  user's normal credential helper interactively, but still sets
  `GIT_TERMINAL_PROMPT=0` so a non-interactive run fails fast instead of
  hanging.
- Every spawned `git` runs with stdin = `Stdio::null()` and hardened
  (CLOEXEC'd) fds (`crates/git-store/src/proc.rs`), so a callback-spawned `git`
  never inherits a terminal and a credential prompt cannot appear.

### 3.1 FetchPolicy gate

`FetchPolicy` (`crates/core/src/fetch.rs`) decides whether resolving an object
may touch the network. Filesystem callbacks read with a non-fetching policy, so a
read can never prompt or escalate to the network:

```rust
pub enum FetchPolicy { CacheOnly, AllowNetwork, Prefetch, MustNotFetch }
pub fn may_fetch(&self) -> bool { matches!(self, AllowNetwork | Prefetch) }
```

`CacheOnly` maps to `GIT_NO_LAZY_FETCH=1` on the git side, and `MustNotFetch`
additionally asserts the path never initiates I/O. On read paths `git-store`
serves objects with `GIT_NO_LAZY_FETCH` set (`crates/git-store/src/store.rs`);
hydration that *is* allowed to fetch goes through `materialize_path` (see
[`object-fetching.md`](object-fetching.md)).

| Caller | Policy | May prompt? | May hit network? |
|--------|--------|-------------|------------------|
| `mount` clone | git's own helper | yes (clone only) | yes |
| FUSE read/getattr | `CacheOnly` / `MustNotFetch` | no | no |
| `materialize_path` hydration | `AllowNetwork` | no (cached creds) | yes |
| user-run `git fetch`/`pull`/`push` | git's own | yes | yes |

### 3.2 Offline behaviour

When an object is genuinely missing and the policy forbids a fetch, the read
returns `ErrorCode::OfflineMissingObject`, which maps to **`EIO` (5)**
(`crates/core/src/error.rs`), promptly and without blocking. Two facts follow
from the storage model:

- **Dirty overlay content never depends on the network.** Overlay files are
  self-contained native bytes, so reads of locally-written content never call the
  fetcher.
- A `BaseRef` entry (clean rename of an unmaterialized file) whose blob is absent
  returns `OfflineMissingObject` on a **content** read, but its namespace
  presence (lookup/readdir/rename) needs no network.

When a fetch fails with expired credentials, `git-store` classifies it as
`ErrorCode::Authentication`, which maps to **`EACCES` (13)** and is retryable by
default (`error.rs` `default_retryable`). The recommended action points the user
at `git lazy-mount doctor`; a subsequent user-run `git fetch` re-primes the
credential helper and later reads of the same object succeed without a remount.

> Considered / not built (possible future): an `AuthFailureState` record per
> repo, an IPC `CredentialRefresh` flow, an `--offline` mount flag that pins
> every callback to `CacheOnly`, and a `prefetch --for-offline` warm-cache
> subcommand. The accurate, shipped mechanism is the `FetchPolicy` gate plus
> `GIT_TERMINAL_PROMPT=0`; the rest is unbuilt.

---

## 4. Synthetic metadata and racy-clean

`getattr` is served by the projection's `attr_of` (`crates/worktree`) and the
mount's `fuse_attr` (`crates/fuse/src/mount.rs`). There is no `SyntheticMeta`,
`SyntheticTime`, or `SizeState` struct; the behaviour is:

- A **clean** (unmaterialized) entry reports a stable mode and a fixed
  `UNIX_EPOCH` mtime — never the wall clock — so re-reading it never advances its
  timestamp.
- A **materialized** overlay file reports its real on-disk `stat` (mtime, size).
- Exact size of an unmaterialized blob is only computed when `getattr` genuinely
  needs it (`ls -l`/`stat` faults the blob once; `readdir` does not), because a
  tree carries no blob sizes.

This stability matters for git's racy-clean handling: git re-hashes an index
entry whose file mtime is `>=` the index timestamp. Because clean entries report
a fixed epoch mtime that is strictly earlier than any index git writes after
mount, clean unmaterialized files are never racy and a clean `git status` faults
zero blobs.

The first clean status faulting zero blobs depends on the **FSMonitor seed**
(`AdminRepo::seed_fsmonitor_valid`), which marks every index entry
`CE_FSMONITOR_VALID` so git's `refresh_cache_ent` early-returns before any
`lstat`. Under the default `--filter=tree:0` clone, a freshly `read-tree`'d index
carries no FSMonitor extension, so without the seed git would stat (and fault)
every entry on the first status. Paths under a checkout conversion
(`filter=` / `ident` / `working-tree-encoding=` / CRLF `eol`) are carved out:
the seed is **skipped wholesale** if any tracked `.gitattributes` declares such
an attribute, so git checks those paths normally. The seed, its token identity
requirement, and the conversion carve-out are owned by
[`fsmonitor.md`](fsmonitor.md).

---

## 5. Security model

Repository data is **untrusted**. Threats and the mechanisms that actually exist:

| Threat | Mitigation | Where |
|--------|------------|-------|
| path traversal | `RepoPath::from_bytes` rejects NUL, absolute, empty, and `.`/`..` components | `core/src/path.rs` (`PathError`) |
| malicious tree names | raw-byte paths, no shell construction, NUL-delimited git plumbing | `core/src/path.rs`, `git-store/src/tree_parse.rs` |
| corrupt object responses | `LocalObjectCorruption` on integrity failure → `EIO` | `core/src/error.rs` |
| hung/failed filters | `FilterFailure` → `EIO`; `ResourceLimit` → `ENOSPC` | `core/src/error.rs` |
| recursive lazy-fetch deadlock | object reads run with `GIT_NO_LAZY_FETCH`; smudge pre-faults `.gitattributes` | `git-store/src/store.rs` |
| credential leakage | URLs/tokens redacted in diagnostics; summaries/context carry no secrets | `git-store/src/proc.rs`, `core/src/error.rs` |
| protected `.git` | any op on the synthetic `.git` returns `Authentication`, not a path probe | `worktree` `child_path` |

### 5.1 Raw paths and traversal rejection

`RepoPath` stores paths as raw non-NUL bytes and is the only validated path
identity. `from_bytes` rejects a NUL byte, a leading `/`, an empty component
(`a//b`), and any `.`/`..` traversal component, returning a typed `PathError`.
Lossy Unicode is never used as an identity key: `RepoPath` exposes separate APIs
for identity (`as_bytes`), human display, and reversible escaping for logs/JSON.
A non-UTF-8 overlay path round-trips through reopen (the `Sidecar.path` is stored
as raw bytes).

### 5.2 Filter and hook safety

Passive hydration must never run a git hook and never silently execute an
untrusted command. The mount serves objects through `cat-file`-class readers and,
when a path declares a clean/smudge filter, `GitStore::smudge_blob` (which runs
`git cat-file --filters --path --attr-source`, `crates/git-store/src/store.rs`).
It never invokes git porcelain and never the hooks directory, so hooks run only
because the user invoked a git command that normally runs them. The
`GIT_NO_LAZY_FETCH` environment on read paths keeps a filter from triggering a
recursive lazy-fetch subprocess that could deadlock a callback.

> Considered / not built (possible future): a configurable `FilterTrust`
> { Trusted, BuiltinsOnly, ErrorOnExternal, Raw } policy defaulting to
> builtins-only. No such enum exists; the real safety behaviour is the
> conversion-attribute carve-out in the FSMonitor seed (section 4) plus serving
> the raw baseline blob when a smudge filter would otherwise run.

### 5.3 Redaction

Errors and logs carry only redacted breadcrumbs. `git-store`'s classifier keeps
only the first few lines of git's stderr and never echoes full URLs (which can
carry tokens) verbatim into a summary (`proc.rs`). In the `Error` model the
`summary` field is documented to contain no secrets and `context` breadcrumbs are
the caller's responsibility to redact (`core/src/error.rs`). File contents never
appear in errors or the change journal.

### 5.4 Private directories

Workspace and cache directories live under `data_dir()`
(`$XDG_DATA_HOME/git-lazy-mount`, else `~/.local/share/git-lazy-mount`), keyed by
a per-mountpoint hash, holding `git/`, `cache/`, `overlay/`, and `anchor/`. They
are private to the invoking user. The synthetic `.git` inside the mount is
protected not by a reserved inode but by `child_path` rejecting any operation on
`.git` with `ErrorCode::Authentication`.
