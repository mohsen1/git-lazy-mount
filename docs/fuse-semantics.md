# FUSE op set, inode/handle model, file-handle semantics

This is the [specification](design.md) chapter for the FUSE callback layer: the
implemented operation set, the inode and file-handle model, the two bounded
worker pools, and the copy-on-write / open-unlink / rename-while-open behaviors.

It is implemented by `TransparentFs` (`crates/fuse/src/mount.rs`), a
`fuser::Filesystem` over a [`Projection`](worktree-model.md)
(`crates/worktree/src/lib.rs`). The inode table is `glm_fs_common::InodeTable`
(`crates/fs-common/src/inode.rs`).

Scope boundary: this doc owns the FUSE callbacks, the inode table, and the
file-handle model. It does **not** own the baseline+overlay content model
([worktree-model.md](worktree-model.md)), object fetching and exact-size faults
([object-fetching.md](object-fetching.md)), the FSMonitor change journal
([fsmonitor.md](fsmonitor.md)), or the index build
([index-strategy.md](index-strategy.md)). Where those topics surface here, this
doc states the FUSE-visible effect and links the owner rather than restating it.

---

## 0. Invariant register

The invariants this layer is responsible for, tagged `FS-*`. Cross-cutting
invariants owned by other docs (zero-blob `readdir`, zero-fetch clean rename,
single-flight fetch) are listed by their owner and only referenced here.

| Tag | Invariant |
|-----|-----------|
| FS-INO-1 | Repeated `lookup(parent, name)` for the same logical path returns the same `(ino, generation)`. |
| FS-INO-2 | `rename` preserves inode identity; open handles keep serving. |
| FS-INO-3 | `unlink` removes the name but not an open handle; the inode survives until the final `forget`. |
| FS-INO-4 | Inode **numbers are never reused**; delete+recreate of a path yields a new number. |
| FS-INO-5 | `forget` of all kernel references on an unlinked inode frees it; it never frees a still-linked inode. |
| FS-FH-1 | A read of an unmaterialized clean file serves range reads by `pread` from a cache-file FD; no `Vec<u8>` proportional to the blob size. |
| FS-FH-2 | A real file handle is allocated per successful `open`/`create` from an `AtomicU64` starting at 1 — `fh` is never 0. |
| FS-FH-3 | `read`/`write` are serviced strictly by `fh` (`pread`/`pwrite` into an FD); no re-resolve-by-path, no whole-file rewrite. |
| FS-FH-4 | After `unlink` of an open file, the open FD keeps serving reads and writes (deleted-but-open); `getattr` falls back to the live FD's size. |
| FS-FH-5 | `O_TRUNC` opens are delivered atomically (negotiated `FUSE_ATOMIC_O_TRUNC`), so a truncating open never copies the old blob up first. |
| FS-CB-1 | No FUSE callback spawns one OS thread per request; blocking callbacks run on a bounded pool, and non-faulting structural callbacks run on a separate pool. |

The deleted `.git` protection and zero-fetch/zero-blob invariants live with
their owners: [worktree-model.md](worktree-model.md) (`.git` protection, clean
rename, `readdir` cost) and [object-fetching.md](object-fetching.md)
(single-flight materialization).

---

## 1. The implemented operation set

`TransparentFs` implements exactly these `fuser::Filesystem` callbacks. Each row
states the FUSE-visible behavior; the heavy lifting is delegated to `Projection`
methods named in the table.

| Op | Behavior |
|----|----------|
| `init` | Negotiate `FUSE_ATOMIC_O_TRUNC` (`1 << 3`) so a truncating open arrives as one `open(O_TRUNC)` instead of `open` + `setattr(0)`; falls back silently if unsupported (FS-FH-5). |
| `lookup` | `Projection::lookup` — resolve a child, allocate a stable inode, reply with attr + generation. |
| `forget` | `Projection::forget` — drop kernel lookup references; may free the inode (FS-INO-5). |
| `getattr` | `Projection::getattr` — exact size and generation. On a vanished path with a live FD, falls back to a regular-file attr sized from that FD (FS-FH-4). An unmaterialized clean blob's exact size faults the object once (see [object-fetching.md](object-fetching.md)). |
| `setattr` | size → `Projection::truncate`; mode → `Projection::set_executable` (only the exec bit; Git tracks no other mode bits); time/uid/gid accepted and ignored. |
| `readlink` | `Projection::readlink` — raw target bytes (overlay inline, or baseline blob). |
| `open` | Allocate a real handle. Writable intent → `Projection::open_write(ino, truncate)` → `Handle::Write`; read intent → `Projection::open_content` → `Handle::Read`. |
| `create` | `Projection::create` — new empty overlay file + writable FD in one step; no baseline fetch. |
| `read` | Serve by `fh`: `ContentHandle::read_at` (pread) for `Handle::Read`, `FileExt::read_at` on the overlay FD for `Handle::Write` (FS-FH-1/3). |
| `write` | Serve by `fh`: `FileExt::write_at` (pwrite) into the overlay FD; a `Handle::Read` fh is `EBADF` (FS-FH-3). |
| `flush` | No-op `ok()`: writes go straight to the overlay FD, so there is no per-handle buffer to flush. |
| `fsync` | By `fh`: `sync_data()` if `datasync`, else `sync_all()` on the overlay FD; a read handle replies `ok()`. |
| `release` | Drop the handle from the table. The deleted-but-open inode is reclaimed by the projection's inode accounting on `forget`. |
| `mkdir` | `Projection::mkdir` — a persisted empty-directory overlay entry. |
| `unlink` | `Projection::unlink` — tombstone a baseline path, else clear an overlay-only entry; the inode survives an open handle (FS-INO-3, FS-FH-4). |
| `rmdir` | `Projection::rmdir` — refuse non-empty (baseline or overlay children); tombstone if baseline-backed. |
| `rename` | `Projection::rename` — honors `RENAME_NOREPLACE`, **rejects** `RENAME_EXCHANGE`; clean file/subtree moves are metadata-only base-refs, no blob fetch ([worktree-model.md](worktree-model.md)). |
| `symlink` | `Projection::symlink` — overlay symlink with raw target bytes. |
| `opendir` | Snapshot the full listing once (`Projection::readdir`) on the metadata pool; key it by the returned `fh`. |
| `readdir` | Serve a slice of the `opendir` snapshot, making paged reads O(entries) instead of O(entries²); falls back to a one-shot `readdir` if the client skipped `opendir`. |
| `releasedir` | Drop the snapshot. |
| `access` | `ok()` — permission policy is left to the kernel's `default_permissions`. |
| `statfs` | Synthetic totals with large free space so tools never refuse writes. |

### Not implemented (kernel default = `ENOSYS`)

`link`, `mknod`, every xattr op (`getxattr`/`listxattr`/`setxattr`/
`removexattr`), `fallocate`, `copy_file_range`, `lseek` (`SEEK_DATA`/`SEEK_HOLE`),
file locking (`getlk`/`setlk`/`flock`), `destroy`, and `batch_forget` are **not**
implemented; they fall through to `fuser`'s default and return `ENOSYS`. Standard
editor/build workflows do not require them. Advisory locking and `lseek`
hole-punching are candidate future work (see the note in §5).

`RENAME_EXCHANGE` is explicitly rejected inside `Projection::rename` with
`UnsupportedOperation` (which maps to errno 95, `EOPNOTSUPP`) rather than
attempting an atomic two-node swap; `RENAME_NOREPLACE` is honored.

---

## 2. The inode table

`InodeTable` (`crates/fs-common/src/inode.rs`) is path-keyed: a
`HashMap<RepoPath, u64>` plus per-inode entries. It is the substrate for
FS-INO-1..5 and its unit tests encode them (`repeated_lookup_is_stable`,
`rename_preserves_identity`, `open_unlink_keeps_inode_until_forget`,
`inode_numbers_are_not_reused`, `generation_bumps_for_new_allocations_only`).

`ROOT_INO = 1` is the only pre-allocated inode. The record is intentionally
minimal:

```rust
struct InodeEntry {
    path: Option<RepoPath>,   // None once unlinked while a handle is open
    lookups: u64,             // FUSE lookup/forget reference count
    generation: u64,          // assigned at allocation; never mutated in place
}
```

Identity rules:

- **Allocation.** `lookup(path)` returns an existing `(ino, generation)` or
  allocates the next number. Numbers are never reused (FS-INO-4).
- **Rename.** `rename(old, new)` re-keys the same inode and evicts any prior
  occupant of the destination name by setting its `path = None` (FS-INO-2).
- **Unlink.** `unlink(path)` drops the name→inode mapping and sets `path = None`;
  if no kernel references remain it is removed immediately, otherwise it survives
  as deleted-but-open (FS-INO-3).
- **Free.** `forget(ino, n)` decrements references; an inode is removed only when
  `lookups == 0 && path.is_none()`. `ROOT_INO` is never freed (FS-INO-5).
- **Generation.** `bump_generation` raises only the table-wide counter used at
  the next allocation; existing entries keep their generation, so open handles
  are unaffected. The shipped `Projection` fixes its baseline at open and does
  not advance it, so this is currently exercised only by the unit test.

There is no reserved synthetic-`.git` inode. The synthetic `.git` is protected
in `Projection::child_path`, which rejects any mutating op whose path is the root
`.git` with `ErrorCode::Authentication` ([worktree-model.md](worktree-model.md)).
Reads of `.git` resolve to the gitfile bytes and are served from memory.

---

## 3. File handles

A handle is allocated per successful `open`/`create` from `next_fh: AtomicU64`,
which starts at **1** — the kernel never sees `fh = 0` (FS-FH-2). Handles live in
a `HashMap<u64, Handle>` on the mount, not in the inode table:

```rust
enum Handle {
    Read  { ino: u64, content: Arc<ContentHandle> },   // read-only: pread a cache FD or .git bytes
    Write { ino: u64, file: Arc<std::fs::File> },       // writable overlay FD (also readable)
}
```

Every `read`/`write`/`fsync`/`release` carries the `fh`, so I/O is serviced
strictly by handle — never by re-resolving a path (FS-FH-3). A `Handle::Read`
backs a `ContentHandle` (`crates/worktree/src/lib.rs`), which serves bounded
`read_at` calls by `pread` from a cache-file FD or, for the tiny synthetic
`.git`, from an in-memory byte slice (FS-FH-1). A `Handle::Write` holds the
overlay's native FD; writes are `pwrite` (`FileExt::write_at`) at the requested
offset, with no whole-file read-modify-write.

The handle carries `ino` so a deleted-but-open file keeps working: after
`unlink`, `getattr(ino)` finds no path but `open_size` walks the handle table for
a live FD on that inode and reports its size (FS-FH-4). The writable FD's own
size reflects intervening writes.

### Copy-on-write and `O_TRUNC`

The write path's copy-up policy lives in `Projection`
([worktree-model.md](worktree-model.md)); the FUSE-visible contract is:

- `create` and `open_write(ino, truncate = true)` seed an **empty** overlay file
  and fetch **zero** baseline bytes.
- `open_write(ino, truncate = false)` on a clean baseline file copies the
  baseline up **once** into an overlay file, then writes happen in place.
- Because `init` negotiates `FUSE_ATOMIC_O_TRUNC`, a truncating open arrives as a
  single `open(O_TRUNC)` and never materializes the old blob first (FS-FH-5).

---

## 4. The two bounded worker pools

FUSE callbacks must not block the kernel's request thread, and they must not
spawn one OS thread per request (FS-CB-1). `TransparentFs` runs two bounded pools
(`crates/fuse/src/pool.rs`):

- **`pool` (`POOL_THREADS = 16`)** runs object-IO callbacks that may block on
  `git` or the network: `lookup`, `getattr`, `setattr`, `open`, `create`,
  `read`, `write`, `fsync`, `readlink`, `mkdir`, `unlink`, `rmdir`, `rename`,
  `symlink`.
- **`meta_pool` (`META_THREADS = 4`)** runs the fast, non-faulting structural
  callbacks `opendir`/`readdir`. These read only tree objects (present under the
  partial clone) and the overlay — never a blob fault — so a directory listing
  stays responsive even when all 16 object-IO threads are busy hydrating blobs;
  an `ls` never queues behind a slow `cat`.

There are no separate decompress/filter/network pools and no
backpressure/cancellation machinery. Content reads go through `git-store`'s
long-lived `cat-file` batch session against the native gitdir with
`GIT_NO_LAZY_FETCH` set on the hot path (`crates/git-store/src/batch.rs`,
`crates/git-store/src/proc.rs`); concurrent first-reads of one missing blob are
coalesced single-flight ([object-fetching.md](object-fetching.md)).

---

## 5. Editor save and tests

The canonical atomic-save sequence works end to end on the mount:

```text
open(existing) → write(tmp sibling) → fsync(tmp) → rename(tmp, original)
              → fsync(parent dir) → unlink(backup)
```

Walked through the implementation: `create` tmp (empty overlay file, no fetch),
`write` + `fsync` (pwrite + `sync_*` on the FD), `rename` tmp→original (overlay
re-key + inode rename, identity preserved; the old destination inode is evicted),
`unlink` backup. After it, plain `git status` sees exactly the new content. The
mount integration test `experiment_a_b_c_transparent_mount`
(`crates/fuse/src/mount.rs`) exercises create, baseline edit via truncate+write,
append (`O_APPEND`), `mkdir` + nested create, `rename`, baseline `unlink`, and
`.git` deletion-is-rejected against a real `/dev/fuse` mount.

The projection-level write, rename, unlink, copy-up, and tombstone behaviors have
their own unit tests in `crates/worktree/src/lib.rs` (e.g.
`cow_edit_of_a_baseline_file_reads_back_merged`,
`unlink_baseline_tombstones_and_hides_from_readdir`,
`mkdir_symlink_and_clean_rename_without_fetch`, and
`journal_record_failure_fails_the_mutation_not_silently_succeeds`).
