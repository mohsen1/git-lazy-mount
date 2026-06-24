# FUSE operations, inode/namespace model, file-handle state machine

Authoritative spec: [`design.md`](../../design.md), primarily **§14** (inode
model), **§15** (namespace), **§16** (required ops), **§17** (file handles),
**§28** (editor/build semantics), **§29** (rename). Cross-cuts §4.5–§4.9 (prior
mistakes turned into invariants), §18–§19 (executor/deadlock), §20–§23
(provider/filters), §32 (overlay durability), §38 (hydration budgets).

This is the design for the `fuse` and `namespace` crates of the design
workspace (§41). It supersedes the current `glm-fs-fuse` `FuseOps`
(`crates/fs-fuse/src/lib.rs`), the path-keyed `InodeTable`
(`crates/fs-common/src/inode.rs`), and the buffer-everything content path in
`glm-workspace` (`crates/workspace/src/lib.rs::read_file`/`write_at`). What is
reusable verbatim is called out per section; the custom stage, custom branch
state, commit-adoption (`Workspace::adopt_commit`), and the `git lazy-mount git
--` bridge are **gone** (§4) and play no part here.

Scope boundary: this doc owns the FUSE callback layer, the inode table, the
persistent namespace store, and the file-handle state machine. It does **not**
own index/FSMonitor strategy (`index-strategy.md`, `fsmonitor.md`), object
fetching/filters (`object-fetching.md`), or the
baseline+overlay content model itself (`worktree-model.md`) — it consumes them.

---

## 0. Invariant register (these become regression tests)

Every section emits invariants tagged `FS-*`. They are the acceptance tests for
this area and map to §43/§44 release criteria and §38 hydration budgets.

| Tag | Invariant | Spec |
|-----|-----------|------|
| FS-INO-1 | Repeated `lookup(parent,name)` for the same logical path returns the same `(ino, generation)` within a projection generation. | §14 |
| FS-INO-2 | `rename` preserves inode identity; open handles keep serving. | §14, §29 |
| FS-INO-3 | `unlink` removes the name but not open handles; storage survives until final `release`+`forget`. | §14, §17.4 |
| FS-INO-4 | Inode **numbers are never reused**; delete+recreate of a path yields a new number *and* a bumped generation. | §14 |
| FS-INO-5 | `forget` of all kernel references on an unlinked, handle-free inode frees it; never frees a live or open inode. | §14 |
| FS-INO-6 | Root `.git` gitfile has a reserved stable inode (`GITFILE_INO`); it is protected from unlink/rename/replace/chmod/write/mkdir-beneath. | §6, §14 |
| FS-INO-7 | A branch/baseline change bumps the projection generation; new inodes carry the new generation, existing open inodes keep theirs. | §14 |
| FS-NS-1 | `readdir(dir)` costs O(direct Git children + direct overlay children), independent of total dirty paths. | §4.5, §15, §38.2 |
| FS-NS-2 | `readdir` returns names + d_type + ino only — never sizes, never blob reads, never smudge filters. | §4.5, §38.2 |
| FS-NS-3 | Empty/untracked directories, tombstones, renames, and directory generations survive lookup, readdir, unmount, remount, daemon restart, `git clean -d`. | §4.9, §15 |
| FS-NS-4 | Case-collision detection answers without a full-namespace scan. | §15 |
| FS-FH-1 | A read of an unmaterialized clean file streams into a verified cache file and serves range reads from an FD; no `Vec<u8>` proportional to blob size. | §4.6, §17.1, §38.5 |
| FS-FH-2 | `open(O_WRONLY\|O_TRUNC)` / `create` seeds an empty overlay file and fetches **zero** baseline blob bytes. | §17.2, §38.7 |
| FS-FH-3 | Repeated 4 KiB writes to a 1 GiB file in one session perform in-place writes; no per-callback full-file rewrite, no 1 GiB allocation. | §4.6, §17.2, §38.8 |
| FS-FH-4 | `O_APPEND` writes are atomic w.r.t. concurrent writers (offset computed under the handle/inode lock). | §17.3 |
| FS-FH-5 | After `unlink` of an open file, existing handles remain readable and writable; namespace lookup fails. | §17.4 |
| FS-FH-6 | `rename` while open keeps existing handles bound to the same identity. | §17.5, §29 |
| FS-FH-7 | `flush`, `fdatasync`/`fsync`, and `release` have distinct, correct effects; no crash-durability claim beyond what the app fsynced. | §17.6 |
| FS-FH-8 | 100 concurrent first-reads of one missing file cause exactly one object retrieval (coalesced in the provider). | §20.1, §38.6 |
| FS-RN-1 | A clean file/subtree rename fetches zero blob contents (represented as a base-ref). | §29, §38.9 |
| FS-RN-2 | `RENAME_NOREPLACE`, `RENAME_EXCHANGE` (or documented `ENOSYS`), and case-only rename behave correctly. | §29 |
| FS-CB-1 | No FUSE callback spawns one OS thread per request; a bounded executor with backpressure serves them. | §4.8, §18 |
| FS-CB-2 | No FUSE callback runs Git porcelain, scans the worktree, waits on the caller's index lock, or initiates a network fetch; reads use `GIT_NO_LAZY_FETCH` cat-file against the native gitdir. | §19 |

---

## 1. The stable inode table

### 1.1 Identity model (§14)

Today's `InodeTable` (`crates/fs-common/src/inode.rs`) is **path-keyed**: a
single `path_to_ino: HashMap<RepoPath,u64>`, lookup/forget/rename/unlink mutate
in place, never reuses numbers, and carries a per-inode `generation`. That core
is correct (it already passes FS-INO-1..5) and is **reused as the substrate**.
Three changes are required for the design:

1. **Generation must be per-inode, surfaced through getattr/lookup/create.** The
   current `FuseOps::getattr` hardcodes `generation = 1` (`fs-fuse/src/lib.rs`
   line ~101). The kernel uses `(ino, generation)` as NFS-style file identity; a
   stale handle must never be confused with a recreated path. Fix: `getattr`
   reads the generation from the table.
2. **Open-handle count** must be tracked alongside lookup count so the
   deleted-but-open lifecycle (FS-FH-5) is driven by the table, not only by
   kernel `forget`.
3. **Identity is no longer *only* the path.** An inode survives `unlink` with
   `path = None`; the file-handle layer (§4) addresses it by inode, not path
   (§14: "Do not use path lookup as the only way to service an open handle").

### 1.2 Record

```rust
pub const ROOT_INO: u64 = 1;        // FUSE root convention
pub const GITFILE_INO: u64 = 2;     // reserved synthetic `.git` gitfile (§6, §14)
// dynamic inodes start at 3.

#[derive(Debug, Clone)]
struct InodeEntry {
    /// Current namespace identity, or `None` if unlinked while open (§17.4).
    path: Option<RepoPath>,
    /// Where this inode's content currently resolves from (§17 sources).
    source: InodeSource,
    /// Entry type at allocation time (RegularFile/Symlink/Dir/Gitlink).
    kind: EntryKind,
    /// Hard-link count. 1 for files/symlinks; 2 + subdir-count style for dirs.
    /// Overlay hard links may transiently exceed 1 (§30.2 policy: overlay-only).
    nlink: u32,
    /// Kernel lookup references (FUSE lookup/forget accounting).
    lookups: u64,
    /// Live open file handles referring to this inode (§14).
    open_handles: u32,
    /// Generation at allocation (§14, FS-INO-4/7).
    generation: u64,
    /// Set once the name is gone but a handle or kernel ref remains (§17.4).
    deleted_but_open: bool,
}

/// Where an inode's bytes live, decoupling identity from the current path.
#[derive(Debug, Clone)]
pub enum InodeSource {
    /// Reserved synthetic `.git` gitfile content (§6).
    Gitfile,
    /// Clean baseline tree entry: lazily hydrated (§8.5 / worktree-model).
    Baseline { oid: ObjectId, mode: GitMode },
    /// Materialized overlay file/symlink (native FD-backed).
    Overlay,
    /// A directory (baseline tree, overlay dir, or implied).
    Directory,
}
```

### 1.3 Operations

```rust
impl InodeTable {
    pub fn new() -> InodeTable;                       // root + reserved gitfile pre-allocated
    /// Allocate-or-find by path; +1 lookup ref. Returns (ino, generation).
    pub fn lookup(&self, path: &RepoPath, kind: EntryKind, src: InodeSource) -> (u64, u64);
    pub fn path_of(&self, ino: u64) -> Option<RepoPath>;
    pub fn entry(&self, ino: u64) -> Option<InodeView>;        // snapshot for getattr
    pub fn forget(&self, ino: u64, n: u64);                    // FS-INO-5
    pub fn rename(&self, old: &RepoPath, new: &RepoPath);      // FS-INO-2; evicts dst
    pub fn unlink(&self, path: &RepoPath);                     // FS-INO-3; sets deleted_but_open
    pub fn open_inc(&self, ino: u64);                          // on successful open/create
    pub fn open_dec(&self, ino: u64);                          // on release; may free if dead
    pub fn bump_generation(&self) -> u64;                      // FS-INO-7, baseline switch
    pub fn is_live(&self, ino: u64) -> bool;
}
```

**Free rule (FS-INO-5):** an inode is removed iff `lookups == 0 && open_handles
== 0 && path.is_none()`. `ROOT_INO` and `GITFILE_INO` are never freed.

**Generation rule (FS-INO-7):** `bump_generation` raises only the table-wide
counter used at *allocation*. Existing entries keep their generation so open
handles are unaffected. A baseline/branch transition (`post-checkout` hook →
daemon, see fsmonitor.md) calls `bump_generation` so kernel attr caches for
changed paths are invalidated on next lookup. The current
`bump_generation_for_new_allocations_only` test already encodes this.

### 1.4 Reserved `.git` gitfile (§6, §14, FS-INO-6)

`GITFILE_INO` is allocated at construction with
`source = Gitfile, kind = RegularFile, nlink = 1, generation = 1`, and its parent
is `ROOT_INO`. The namespace store (§2) hard-codes `lookup(ROOT, ".git") =
GITFILE_INO`. Protection is enforced at the op layer (§3.4): any `unlink`,
`rename` (as source or destination), `setattr`, `write`, `create`, or `mkdir`
that resolves to `GITFILE_INO` returns `EPERM`/`EACCES`. A Git **tree entry**
literally named `.git` is rejected at projection time (it can never reach the
namespace) and reported via `doctor` — the synthetic entry always wins
resolution order (§8 step 1).

---

## 2. The parent-indexed persistent namespace store

The current namespace is implicit: directory listing is computed on the fly in
`Workspace::list_dir` by reading the baseline tree for `dir` and then **scanning
`overlay.entries()` (every dirty path in the workspace)** to find immediate
children (`workspace/src/lib.rs` lines ~330–355). That scan is O(total dirty
paths) per `readdir` and violates §15/§4.5/FS-NS-1. The overlay's flat
`HashMap<RepoPath, OverlayKind>` (`overlay/src/lib.rs`) also cannot answer
`children(parent)`, `has_children`, subtree rename, or case collision in better
than O(N).

The design introduces a dedicated **`namespace` crate**: a persistent,
parent-indexed store (SQLite WAL per §32) that is the authority for overlay
*structure* (the overlay content store keeps owning bytes). It does **not**
store baseline tree structure — baseline children come from the object provider
(`provider.tree`) and are merged at readdir time.

### 2.1 Schema (SQLite, §32)

```sql
-- One row per overlay namespace node (file/dir/symlink/tombstone/rename).
CREATE TABLE ns_node (
  ino           INTEGER PRIMARY KEY,        -- stable inode number
  parent_ino    INTEGER NOT NULL,           -- parent directory inode (ROOT_INO at top)
  name          BLOB    NOT NULL,           -- final component, raw bytes (§31)
  name_fold     BLOB    NOT NULL,           -- case/normalization fold for collision checks (§15)
  kind          INTEGER NOT NULL,           -- File|ExecFile|Symlink|Dir|Tombstone|BaseRef|Gitlink
  generation    INTEGER NOT NULL,           -- inode generation (FS-INO-4/7)
  dir_gen       INTEGER NOT NULL DEFAULT 0, -- bumped when *direct* children change (§22, §15)
  content_id    BLOB,                       -- overlay content backing id (NULL for dir/tombstone)
  base_oid      BLOB,                       -- BaseRef target blob (clean rename; §29) — no bytes
  base_mode     INTEGER,                    -- BaseRef Git mode
  executable    INTEGER NOT NULL DEFAULT 0, -- Git exec bit
  open_unlinked INTEGER NOT NULL DEFAULT 0, -- retained for an open handle (§17.4)
  UNIQUE(parent_ino, name)
);
CREATE INDEX ns_children ON ns_node(parent_ino);
CREATE INDEX ns_fold     ON ns_node(parent_ino, name_fold);  -- O(siblings) collision check
```

`kind = Tombstone` rows shadow a baseline child of the same `(parent,name)`.
`kind = Dir` rows persist empty/untracked directories (§4.9). `kind = BaseRef`
is a renamed-clean entry pointing at `base_oid` with no stored bytes (reuses the
existing `OverlayKind::BaseRef` idea from `overlay/src/lib.rs`).

### 2.2 API

```rust
pub struct Namespace { /* rusqlite connection, WAL */ }

impl Namespace {
    pub fn open(dir: &Path, inodes: &InodeTable) -> Result<Namespace>;

    /// Overlay override for one child, or None to fall through to baseline.
    pub fn lookup(&self, parent_ino: u64, name: &[u8]) -> Result<Option<NsNode>>;

    /// Direct overlay children of a directory — O(direct children) (FS-NS-1).
    pub fn children(&self, parent_ino: u64) -> Result<Vec<NsNode>>;

    /// Whether a directory has any overlay child (for rmdir emptiness; §16).
    pub fn has_children(&self, parent_ino: u64) -> Result<bool>;

    /// Case/normalization collision among siblings — O(siblings) via ns_fold (§15).
    pub fn case_collision(&self, parent_ino: u64, name_fold: &[u8]) -> Result<Option<NsNode>>;

    /// Atomic subtree rename: rewrite parent_ino+name of the root node, leave
    /// descendants attached by parent_ino (no descendant blob reads; FS-RN-1).
    pub fn rename_subtree(&self, from_ino: u64, new_parent: u64, new_name: &[u8]) -> Result<()>;

    /// Atomic subtree delete: tombstone the root (if baseline-backed) and drop
    /// overlay descendants in one transaction.
    pub fn delete_subtree(&self, ino: u64) -> Result<()>;

    /// Bump a directory's generation (its direct children changed) (§22).
    pub fn bump_dir_gen(&self, parent_ino: u64) -> Result<u64>;

    pub fn put_file(&self, parent: u64, name: &[u8], content_id: &[u8], exec: bool) -> Result<NsNode>;
    pub fn put_symlink(&self, parent: u64, name: &[u8], content_id: &[u8]) -> Result<NsNode>;
    pub fn put_dir(&self, parent: u64, name: &[u8]) -> Result<NsNode>;           // empty dir (§4.9)
    pub fn put_base_ref(&self, parent: u64, name: &[u8], oid: &ObjectId, mode: GitMode) -> Result<NsNode>;
    pub fn tombstone(&self, parent: u64, name: &[u8]) -> Result<NsNode>;
    pub fn clear(&self, ino: u64) -> Result<()>;                                  // dematerialize
}
```

### 2.3 `readdir` cost (FS-NS-1)

The op layer builds a listing as:

```text
let base = match resolve_baseline_tree(dir) { Some(t) => provider.tree(t)?.entries, None => [] };
let over = namespace.children(dir_ino)?;           // O(direct overlay children)
merge: start from base names; apply each `over` node:
    Tombstone   -> remove the name
    File/Sym/Dir/BaseRef/Exec -> insert/replace with overlay kind
    (deeper paths cannot appear: children() returns only direct children)
```

No `overlay.entries()` full scan. Baseline tree read is one `provider.tree`
(trees are present under a `blob:none` clone — zero blob fetch; §38.2). This is
the single biggest behavioral change from `Workspace::list_dir`.

### 2.4 Persistence & recovery (§32, FS-NS-3)

The namespace DB is the durable record of overlay structure; the
`filtered-cache/` and overlay `content/` dirs hold bytes. On daemon
start/`recover` (§32.2): open the DB (WAL replay is automatic), reconcile
dangling `content_id` references (quarantine, never delete acknowledged
writes), and if the DB is unopenable or its generation is uncertain, force a
FSMonitor full-invalidation (`/`, fsmonitor.md). Empty dirs and tombstones are
rows, so they survive remount and `git clean -d` walks them like any directory
(FS-NS-3).

---

## 3. The required FUSE operation set (§16)

Implemented in the `fuse` crate. Each potentially-blocking callback runs on the
bounded executor (§18, FS-CB-1), never one thread per request as the current
`adapter.rs::dispatch` (`std::thread::spawn` per call) does — that
`spawn`-per-callback is explicitly replaced. The `fuser::Filesystem` adapter
(`crates/fs-fuse/src/adapter.rs`) is reused for FFI shape (errno mapping,
`ReplyDirectory` paging, CLOEXEC mount options) but rewired onto the executor +
handle table.

| Op | Behavior summary | Notes / spec |
|----|------------------|--------------|
| `init` | Negotiate kernel caps; **disable readdirplus** unless measured safe (§4.5). | §16, §4.5 |
| `destroy` | Drain executor, flush dirty handles, checkpoint namespace WAL. | §16 |
| `lookup` | Resolve via §8 order; allocate inode; +1 lookup ref; return attr+generation. | §14 |
| `forget` / `batch_forget` | Drop kernel refs; may free inode (FS-INO-5). | §14 |
| `getattr` | Exact size (§21); generation from inode table. | §21, §22 |
| `setattr` | size→`truncate` handle/inode op; mode→exec bit only (§28: Git tracks no other bits); time/uid/gid accepted+ignored. | §16, §28 |
| `open` | Allocate a real handle (§4); choose source state (§4). | §17, §4.7 |
| `create` | Create overlay file + handle in one step; `O_EXCL` honored. | §17.2 |
| `read` | Serve range from handle FD (FS-FH-1). | §17.1 |
| `write` | In-place write into handle's overlay FD (FS-FH-3); `O_APPEND` atomic (FS-FH-4). | §17.2/§17.3 |
| `flush` | Per-`close()` flush; not durability. Idempotent; may be called many times. | §17.6 |
| `fsync` / `fdatasync` | `fsync` = data+metadata; `fdatasync` = data only; on overlay FD. | §17.6 |
| `release` | Last handle ref → `open_dec`; if `deleted_but_open` and refs 0, reclaim storage. | §17.4 |
| `opendir` | Allocate dir handle; snapshot listing for stable offsets. | §16 |
| `readdir` | Names+d_type+ino only (FS-NS-2); O(direct children) (FS-NS-1). | §4.5, §15 |
| `releasedir` | Free dir handle + snapshot. | §16 |
| `mkdir` | Persist an empty dir node (§4.9); not a transient inode. | §4.9, §15 |
| `rmdir` | Refuse non-empty (baseline OR overlay children via `has_children`); tombstone if baseline-backed. | §16 |
| `unlink` | Tombstone (baseline) / clear (overlay-only); inode survives open (FS-FH-5). | §17.4 |
| `rename` / `rename2` | `RENAME_NOREPLACE`, `RENAME_EXCHANGE`(or `ENOSYS`), case-only; clean rename = base-ref, no fetch (FS-RN-1). | §29 |
| `symlink` | Overlay symlink (raw target bytes; never followed for overlay writes). | §30.1 |
| `readlink` | Raw target bytes from overlay or baseline blob (raw, unfiltered). | §30.1 |
| `link` | Overlay-only hard link until commit, else documented `EPERM` (choose+document, §30.2). | §30.2 |
| `access` | Permission check against synthetic mode; `X_OK` from exec bit. | §16 |
| `statfs` | Synthetic totals; large free space so tools don't refuse writes. | §16 |
| `getxattr`/`listxattr`/`setxattr`/`removexattr` | Policy: `ENOTSUP`/`ENODATA` by default; never silently drop user xattrs by claiming success. | §16 |
| `fallocate` | On overlay FD; `FALLOC_FL_PUNCH_HOLE` supported, else `EOPNOTSUPP`. | §16 |
| `copy_file_range` | Reflink/copy within overlay; may materialize source once. | §16 |
| `lseek` | `SEEK_DATA`/`SEEK_HOLE` from overlay FD; else `EINVAL`. | §16 |
| File locking (`getlk`/`setlk`/`flock`) | Advisory locks on the handle; documented policy (POSIX advisory, no lease). | §16, §28 |

**Deadlock invariants for every callback (FS-CB-2, §19):** no porcelain, no
worktree scan, never wait on the caller's `index.lock`; content resolution goes
through the object provider's `GIT_NO_LAZY_FETCH` cat-file batch session
(`object-provider/src/lib.rs`, `git-store/src/batch.rs`) against the **native
gitdir**; only the fetch scheduler causes network; mount/session FDs are CLOEXEC
(reuse `git-store/src/proc.rs::harden_fds`).

### 3.4 `.git` protection (FS-INO-6)

A guard at the op-layer entry of every mutating op: if the target inode is
`GITFILE_INO`, or `mkdir`/`create` would place a child under it, return `EPERM`
(`rename` with `.git` as src or dst → `EPERM`; `setattr`/`write` → `EACCES`).

---

## 4. The file-handle state machine (§17)

This is the heart of the design and the largest departure from the current
code, where `open`/`opendir` return `fh = 0` (`adapter.rs` lines 160–166) and
`read`/`write` re-resolve by **inode→path→buffer the whole file**
(`fs-fuse/src/lib.rs::read` calls `ws.read_file` → `Vec<u8>`;
`workspace/src/lib.rs::write_at` reads the entire file, mutates, rewrites). That
buffers whole blobs (§4.6 violation) and cannot do open-unlink/rename-while-open
(FS-FH-5/6). The design allocates a **real handle per successful open** and
services I/O from a file descriptor.

### 4.1 Handle record (§17)

```rust
pub struct Fh(pub u64);                 // returned to the kernel; key into HandleTable

pub struct FileHandle {
    pub ino: u64,
    pub generation: u64,
    pub flags: OpenFlags,               // O_RDONLY/WRONLY/RDWR, O_APPEND, O_TRUNC, O_EXCL
    pub access: Access,                 // Read | Write | ReadWrite
    pub append: bool,                   // O_APPEND
    pub source: HandleSource,           // see state machine below
    pub fd: Option<std::fs::File>,      // native FD into cache/overlay file
    pub dirty: bool,                    // any write happened (drives copy-up + journal)
    pub path_at_open: RepoPath,         // diagnostics only (may be stale after rename/unlink) (§17)
    pub deleted_but_open: bool,         // unlinked since open (§17.4)
}

pub enum HandleSource {
    /// Read-only view of a clean baseline file, served from a *verified cache
    /// file* (§17.1). FD points at filtered-cache/<key>.
    CacheFile { oid: ObjectId, cache_key: CacheKey },
    /// FD points at the overlay native file for this inode (writable / dirty).
    OverlayFile { content_id: ContentId },
    /// Synthetic `.git` gitfile content held in memory (tiny, fixed bytes).
    Gitfile,
    /// Symlink target (served via readlink, not read); no FD.
    Symlink,
}
```

`HandleTable` is `Mutex<Slab<FileHandle>>` (or sharded for write-concurrency);
`Fh` is the slab key. The kernel passes `fh` to every `read`/`write`/`flush`/
`fsync`/`release`, so I/O **never** needs a path lookup (§14: a file may have no
path after unlink).

### 4.2 `open` decision table

| Trigger (flags) | Inode source | Action | Fetch? |
|-----------------|--------------|--------|--------|
| `O_RDONLY`, clean baseline file | `Baseline{oid}` | Resolve filter context; ensure object present; stream working-tree representation into a **verified** `filtered-cache` file (§17.1, §23); open FD read-only; `HandleSource::CacheFile`. | blob (once, coalesced — FS-FH-8) |
| `O_RDONLY`, already overlay | `Overlay` | Open overlay FD read-only; `OverlayFile`. | none |
| `O_WRONLY\|O_TRUNC` or `create` | any | Seed an **empty** overlay file; namespace `put_file`; open FD; `OverlayFile`; `dirty=true`. **No baseline fetch** (FS-FH-2, §17.2). | **none** |
| `O_WRONLY`/`O_RDWR`, partial (no `O_TRUNC`), clean baseline | `Baseline{oid}` | Copy-up: materialize working-tree representation **once** (reflink/copy into overlay file), `put_file`, open FD; `OverlayFile`. Subsequent writes in place (FS-FH-3). | blob (once) |
| `O_RDWR`, already overlay | `Overlay` | Open overlay FD r/w. | none |
| `O_RDONLY`, `.git` | `Gitfile` | `HandleSource::Gitfile`, fixed bytes. | none |

`open_inc(ino)` on success. The "materialize once, then write in place" rule is
the §4.6/§17.2/FS-FH-3 fix: copy-up happens at most once per writable open, and
writes are `pwrite` into the FD, never read-modify-rewrite of the whole file.

### 4.3 `read` / `write`

```text
read(fh, off, size):  pread(handle.fd, off, size)  -> reply.data   # FS-FH-1
write(fh, off, data):
    if handle.append: off = fstat(fd).len under handle lock        # FS-FH-4
    pwrite(handle.fd, off, data); handle.dirty = true
    journal Modified(ino) (fsmonitor.md); bump_dir_gen lazily on size change
```

Reads of a `CacheFile` whose object is missing locally and policy forbids
network return a bounded error (`EIO`/offline) — never a hang, never an
interactive prompt (§19, §35). The provider coalesces concurrent first-reads
(FS-FH-8), so 100 readers of one missing blob → one fetch (existing
`GitObjectProvider::ensure_objects` already does this).

### 4.4 The state machine (per inode, across handles)

```text
States: UNMATERIALIZED  (clean baseline; no overlay row)
        CACHED          (read-only cache file exists; still clean)
        MATERIALIZED    (overlay file exists; may be dirty)
        DELETED_OPEN    (unlinked, but >=1 open handle)
        REAPED          (storage freed)

UNMATERIALIZED --open O_RDONLY-->        CACHED        (stream to verified cache file)
UNMATERIALIZED --open O_TRUNC/create-->  MATERIALIZED  (empty overlay, NO fetch)        [FS-FH-2]
UNMATERIALIZED --open writable partial-> MATERIALIZED  (copy-up once)                    [FS-FH-3]
CACHED         --open writable-->        MATERIALIZED  (copy-up from cache, no refetch)
MATERIALIZED   --write-->                MATERIALIZED  (in-place pwrite, dirty=true)      [FS-FH-3]
(any)          --unlink while open-->    DELETED_OPEN  (namespace name gone; FD survives) [FS-FH-5]
(any)          --rename while open-->    same state    (identity moves; FD unchanged)     [FS-FH-6]
DELETED_OPEN   --last release+forget-->  REAPED        (delete overlay/cache backing)
MATERIALIZED   --release, clean-equal--> UNMATERIALIZED (dematerialize: §8.5 conditions)
```

Dematerialization on `release` is allowed **only** under §8.2's guard:
content+mode match the baseline, no writable handle remains, no pending fsync, no
concurrent rename. Never on timestamp alone.

### 4.5 Open-unlink (§17.4, FS-FH-5)

`unlink` of an open file: namespace `tombstone`/`clear` removes the name;
`InodeTable::unlink` sets `path = None`, `deleted_but_open = true`. The overlay
backing file is **not** deleted (it is renamed into a `reaped/` holding area
keyed by inode, so a new file at the same path gets a fresh `content_id`).
Existing handles keep their FD and serve read/write. On final `release` with
`open_handles == 0`, the backing is deleted and the inode reaped (FS-INO-5). A
clean baseline file unlinked while open keeps serving from its `CacheFile` FD
even though its tombstone hides the name.

### 4.6 Rename-while-open (§17.5, §29, FS-FH-6)

`rename` moves the namespace row(s) and calls `InodeTable::rename` (identity
preserved). Open handles reference the inode + FD, not the path, so they are
untouched. A **clean** file/subtree rename writes a `BaseRef` node (no descendant
reads, FS-RN-1) — exactly the existing `Overlay::put_base_ref` mechanism, now in
the namespace. `rename2` flags:

- `RENAME_NOREPLACE`: fail `EEXIST` if dst resolves to anything.
- `RENAME_EXCHANGE`: swap two namespace nodes atomically, or documented
  `ENOSYS` in the first cut (§29 permits a documented unsupported error).
- case-only rename: detected via `name_fold`; allowed on case-sensitive Linux.

### 4.7 flush / fsync / release distinctions (§17.6, FS-FH-7)

| Callback | When | Effect | Durability claim |
|----------|------|--------|------------------|
| `flush` | every `close()` of a dup'd fd; possibly many times | flush userspace buffers to the overlay FD; cheap; errors surface the last write error | **none** — not a sync point |
| `fdatasync` | app `fdatasync()` | `fdatasync(overlay_fd)` (data only) | data persisted |
| `fsync` | app `fsync()` | `fsync(overlay_fd)` (data+metadata) | data+metadata persisted |
| directory fsync | app fsyncs a dir fd (editor save §28) | checkpoint namespace WAL for that dir's pending nodes | directory entry persisted |
| `release` | last fd closed | `open_dec`; reap if `deleted_but_open`; maybe dematerialize | best-effort flush, **not** a sync |

We never claim crash durability for bytes the app never fsynced beyond ordinary
filesystem guarantees (§17.6). The overlay's existing atomic-publish path
(`overlay/src/lib.rs::atomic_write`: temp→fsync→rename→dir-fsync) is the
durability primitive when the app *does* fsync, and for namespace-row publish.

---

## 5. Editor save patterns + the filesystem-semantics test list (§28)

The canonical atomic-save sequence (§28) must work end to end on the mount:

```text
open(existing) → write(tmp sibling) → fsync(tmp) → rename(tmp, original)
              → fsync(parent dir) → unlink(backup)
```

Walked through the model: `create` tmp (MATERIALIZED, empty overlay file, no
fetch), `write`+`fsync` (in-place pwrite + `fsync(fd)`), `rename` tmp→original
(`RENAME` over an existing baseline/overlay node: dst inode goes DELETED_OPEN if
held, else reaped; tmp's inode adopts the original name, identity preserved),
dir `fsync` (namespace WAL checkpoint), `unlink` backup. After this, plain `git
status` must see exactly the new content (§43 criterion 7–8) and the original's
old inode is gone (FS-INO-4 on recreate).

### 5.1 Required filesystem-semantics tests (real `/dev/fuse`, §40.2/§40.4)

Each is a regression test mapped to an invariant above:

```text
T-save-atomic        editor atomic save (above)                  FS-FH-2/6/7, §43.7
T-trunc-write        open(O_TRUNC); write; read back             FS-FH-2, §38.7
T-append             O_APPEND from two writers; no lost bytes    FS-FH-4
T-pwrite-partial     4 KiB pwrites across a 1 GiB file           FS-FH-3, §38.8
T-sparse-write       write at large offset; SEEK_HOLE/SEEK_DATA  §16 lseek
T-write-after-rename rename open file, keep writing              FS-FH-6
T-open-unlink        unlink open file; read+write survive        FS-FH-5, §43.22
T-rename-over-open   rename over an open destination             FS-FH-6
T-dir-rename         rename a subtree (clean): zero blob fetch   FS-RN-1, §38.9
T-case-rename        case-only rename                            FS-RN-2
T-rename-noreplace   RENAME_NOREPLACE → EEXIST                   FS-RN-2
T-rename-exchange    RENAME_EXCHANGE swap (or documented ENOSYS) FS-RN-2
T-read-while-write   reader + writer on same path concurrently   §40.6
T-mmap-write         writable mmap; msync; read back             §28
T-flock              advisory file lock round-trip               §16
T-empty-dir-remount  mkdir; unmount; remount; dir still there    FS-NS-3, §43.23
T-readdir-zero-hydra ls a 100k-file dir: zero blob fetch         FS-NS-1/2, §38.2
T-bigfile-bounded    multi-GiB read/truncate: bounded memory     FS-FH-1/3, §43.25
T-gitfile-protect    unlink/rename/write `.git` → EPERM/EACCES   FS-INO-6
T-concurrent-miss    100 readers of one missing blob → 1 fetch   FS-FH-8, §38.6
T-non-utf8-names     create/read/rename invalid-UTF-8 names      §31
T-crash-overlay      kill after write+fsync; data survives       §32, §43.27
```

Editor/tool save patterns to exercise specifically (§28): VS Code (write-temp +
rename), Vim/Neovim (backup + rename, or in-place with `:set nowritebackup`),
Emacs (`#file#` autosave + backup `file~`), JetBrains (safe-write rename),
plus ripgrep/Cargo/Make/Ninja as readers (must hit FS-NS-1/2 budgets).

---

## 6. What is reused vs. replaced

| Existing file | Disposition |
|---------------|-------------|
| `crates/fs-common/src/inode.rs` `InodeTable` | **Reused** as substrate; extended with `open_handles`, `InodeSource`, per-inode generation surfaced, reserved `GITFILE_INO`. |
| `crates/fs-fuse/src/adapter.rs` (fuser glue, errno, paging, CLOEXEC opts) | **Reused** for FFI shape; rewired off `spawn`-per-callback onto the bounded executor and onto real handles. |
| `crates/fs-fuse/src/lib.rs` `FuseOps` (fh=0, buffer-whole-file read/write) | **Replaced** by the handle table + FD-served I/O. |
| `crates/overlay/src/lib.rs` content store + `BaseRef`/tombstone/atomic_write | **Reused** for bytes; its flat `HashMap` *structure* role moves into the `namespace` crate. |
| `crates/workspace/src/lib.rs::list_dir` (full overlay scan) | **Replaced** by `Namespace::children` (O(direct children)). |
| `crates/object-provider/src/lib.rs` (coalescing, `GIT_NO_LAZY_FETCH`, presence authority) | **Reused** verbatim — already satisfies FS-FH-8, FS-CB-2. |
| `crates/git-store/src/batch.rs` (`cat-file --batch-command`), `proc.rs::harden_fds` | **Reused** verbatim for streaming reads and CLOEXEC. |
| `crates/workspace` stage/commit/branch/`adopt_commit`, `crates/stage`, git-interop bridge | **Superseded** (§4); not part of this layer. |

Streaming caveat (§4.6/§20): the current provider returns `Vec<u8>`
(`raw_blob`/`filtered_blob`/`smudge_blob` in `git-store/src/store.rs`). For
FS-FH-1 the read path must gain a streaming form — `open_worktree_file(...) ->
ContentHandle` (§20 trait) that writes the filtered representation into a
verified cache file and hands back an FD — so large files never allocate a full
`Vec`. That is owned by `object-fetching.md`; this layer
*consumes* the resulting cache-file FD as `HandleSource::CacheFile`.
