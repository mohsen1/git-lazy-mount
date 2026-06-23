# Platform: Linux (FUSE) — spec §40

The Linux backend projects a workspace as a FUSE filesystem. Its design
deliberately splits into two parts:

1. **Backend logic** (`glm-fs-fuse`, the [`FuseOps`](../crates/fs-fuse/src/lib.rs)
   type) — maps the low-level FUSE callbacks onto the transactional workspace
   engine (`glm-workspace`) and the stable inode table (`glm-fs-common`). This
   contains no libfuse FFI, builds on every platform, and is **unit-tested
   without libfuse**.
2. **The kernel adapter** — a thin `fuser::Filesystem` implementation (gated
   behind a `fuse` feature, requiring libfuse3 and a privileged/loopback-capable
   runner) whose methods call straight into `FuseOps`.

> **Status — real kernel mounting is NOT enabled in the default build or CI.**
> The callback logic is implemented and tested; the libfuse-backed adapter that
> performs an actual kernel mount is **not compiled in this environment** (no
> libfuse). `glm_fs_fuse::mount()` returns a structured
> `ErrorCode::FilesystemBackendUnavailable` rather than pretending to mount, so
> callers degrade to the headless CLI. Do not read this document as a claim of
> transparent FUSE support.

## Why the split

The risky, platform-specific part of any FUSE backend is the thin layer that
translates kernel requests into engine calls and back. By keeping `FuseOps`
free of FFI, the entire callback surface — path resolution, inode allocation,
attribute synthesis, ranged reads, lazy hydration, `forget` accounting — is
exercised by ordinary `cargo test` on Linux, macOS, and Windows. The remaining
adapter is small and is the only code that needs a real kernel and libfuse.

Platform FFI and `unsafe` are isolated to the per-backend crate; `glm-fs-fuse`
itself is `#![forbid(unsafe_code)]`.

## Implemented callback behaviors (`FuseOps`)

`FuseOps` today exposes `lookup`, `getattr`, `readdir`, `read`, `readlink`, and
`forget`. The properties below are enforced and tested.

### Stable inode identity and generations

The [`InodeTable`](../crates/fs-common/src/inode.rs) (spec §19) gives every
logical path a stable inode number for the lifetime of the workspace:

* Repeated `lookup` of the same path returns the same `(ino, generation)`.
* **Inode numbers are never reused.** A freed number is never handed out again,
  so a stale kernel reference can never be mistaken for a newly created file.
* A per-inode **generation** is carried for backends that surface it. A view
  switch (`bump_generation`) only affects inodes allocated *afterwards*; existing
  inodes keep their generation so open handles are unaffected (spec §35).

### Lookup refcounting and `forget`

Each `lookup` increments a kernel reference count; `forget(ino, nlookup)` drops
`nlookup` references. The root inode is never forgotten. Memory for an inode is
released only when its references reach zero *and* it is unlinked — releasing the
entry, never the number.

### Open-unlinked survival

`unlink` removes a path from the namespace but keeps the inode alive (its number
reserved) until the final `forget`. An open handle to an unlinked file therefore
remains valid, matching POSIX open-unlink semantics. A rename preserves identity:
the same inode answers to the new path and open handles stay valid (spec §22).

### Reads hydrate non-interactively

`read`/`readlink` resolve content through the workspace, which goes through the
object provider. The fetch policy permits network I/O (`FetchPolicy::AllowNetwork`)
so a read can lazily hydrate a missing blob — but **a filesystem callback never
prompts for credentials** (spec §3.13). Every `git` subprocess in `glm-git-store`
runs with `GIT_TERMINAL_PROMPT=0`, so a hydration that needs credentials fails
cleanly instead of blocking the kernel on an interactive prompt. Reads never mark
a file modified.

### Exact attribute sizes

`getattr`/`lookup` report the **exact** byte size: for files and symlinks via
`Workspace::file_size`, and `0` for directories and gitlinks. Backend-neutral
attributes ([`FileAttr`](../crates/fs-common/src/attr.rs), spec §28) carry only
what Git tracks — file type and the executable bit — plus stable synthetic
`st_mode`. A synthetic timestamp difference is never treated as a modification.

### No network I/O under inode-map locks

The inode table's mutex is only ever held for in-memory map updates. Object
fetching in the provider happens with **no lock held** (spec §3.19): the
in-flight set is updated under a short critical section, the lock is dropped, the
fetch runs, and the lock is retaken to publish results. A slow or hung fetch can
never block unrelated `lookup`/`forget` traffic.

### Bounded attribute caching + explicit invalidation (design requirement)

The adapter must request a **bounded** attribute/entry TTL from the kernel and
**explicitly invalidate** cached inodes/attributes when a view switch or external
mutation changes them, rather than relying on TTL expiry. This is the FUSE-side
expression of the engine's generation model: when `bump_generation` advances the
desired generation, affected entries must be invalidated so the kernel re-`lookup`s
them. (Cache-key/TTL policy lives with the engine; `glm-metadata` is a placeholder
today and the stat policy is computed inline by `FuseOps::file_size`.)

## FUSE operations to implement in the kernel adapter

The `fuser::Filesystem` adapter forwards to `FuseOps`. Beyond the already-tested
core (`lookup`, `getattr`, `readdir`, `read`, `readlink`, `forget`), the adapter
must implement and the manual job must evaluate:

* `init`/`destroy` — negotiate kernel capabilities; clean detach.
* `open`/`opendir`/`release`/`releasedir` — handle lifecycle; surface
  open-unlinked correctly.
* `write`/`create`/`mkdir`/`unlink`/`rmdir`/`rename`/`symlink`/`setattr`
  (truncate) — copy-on-write into the overlay via `FuseOps::workspace()`; these
  mutate the overlay and must update the inode table (`rename`/`unlink`).
* `readdirplus` — combined enumerate-plus-attributes, to avoid a `lookup`
  storm; evaluate whether the per-entry attribute it returns also takes a kernel
  reference (refcount accounting must match `forget`).
* `forget`/`batch_forget` — already backed by `InodeTable::forget`.
* `statfs`, `access`, `flush`, `fsync`, `lookup` of `.`/`..`.

### Behaviors to evaluate on a real kernel

* **READDIRPLUS** refcount accounting (above).
* **Writeback cache** (`FUSE_WRITEBACK_CACHE`): interaction with copy-on-write
  and exact-size reporting; whether the kernel may buffer writes past what the
  overlay has durably recorded.
* **Attribute/entry TTL** values vs. explicit invalidation on generation change.
* **`max_read`/`max_write`** and large ranged reads against lazily hydrated blobs.
* **Direct I/O vs. page cache** for freshly hydrated content.
* **Unmount with busy handles** — a documented concern: detaching while handles
  are open (or while a hydration is in flight) must drain via the
  `Quiescing`→`Unmounting` lifecycle states (`glm-daemon`, spec §39) rather than
  ripping the mount out. Lazy/forced unmount semantics must be chosen
  deliberately and tested.

## CI

The default `check` matrix runs `fmt` + `clippy` + `test` on
`ubuntu-latest`, `macos-latest`, and `windows-latest`
([.github/workflows/ci.yml](../.github/workflows/ci.yml)). A **green matrix does
NOT imply the kernel backend was exercised** — it runs the backend-independent
suite only.

A separate, **manually-triggered** `linux-mount` job (`workflow_dispatch`)
installs `fuse3`/`libfuse3-dev` and runs the FUSE backend-logic tests
(`cargo test -p glm-fs-fuse -- --include-ignored`). When the `fuser` adapter
lands, this job will additionally perform a real loopback mount. Until then,
real kernel mounting is not part of any automated build.

## Data root

Per-OS roots are resolved by `glm-platform`
([roots.rs](../crates/platform/src/roots.rs)): on Linux the XDG base directories
(`XDG_CACHE_HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`).
