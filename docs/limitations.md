# Limitations and honest status

This document states plainly what is implemented, what is partial, and what is
not done. Correctness, durability, and explicit limitations take priority over
superficial transparency. **We do not claim transparency or platform support
that the test suite has not demonstrated.**

## Implemented and verified by tests (against real Git)

* Lazy bare clone with `blob:none`; trees present, blobs fetched on demand.
* Object provider: residency tracking, request coalescing (100 reads → 1 fetch),
  fetch batching, `FetchPolicy` enforcement, metrics.
* Copy-on-write overlay: full write, partial overwrite (preserves untouched
  bytes), truncate-without-fetch, tombstones, base-refs (clean rename without
  fetching the blob), atomic crash-safe publication, survives reopen.
* Persistent staged delta; three-tree `O(changed)` status that never fetches or
  writes objects.
* Ordinary Git commits (subtrees reused, passes `git fsck`); private workspace
  head ref; attached-branch compare-and-swap that detects divergence; push to a
  normal bare remote with `--force-with-lease`.
* Faithful working-tree filtering via Git plumbing with `--attr-source`
  (CRLF/`eol` verified); symlinks; executable bit.
* Append-only operation log with crash injection proven at every persistence
  boundary; structured recovery report.
* Stable inode model (numbers never reused, rename preserves identity,
  open-unlink survives until forget).
* A working `git-lazy-mount` CLI driving the whole flow end-to-end.

## Partial / designed but not complete

* **`switch`/`reset`/`merge`/`rebase`/`clean`/`stash`** — the native command set
  is partial: `switch` clean-case is designed; reset/merge/rebase/clean/stash and
  the structured-conflict materialization (§34) are not implemented.
* **Content `diff`** — only name-status is exposed; byte/hunk diff is future.
* **Manifest-assisted metadata** (§5.1) — designed, not implemented.
* **Streaming large blobs** — the provider buffers blob bytes in memory rather
  than streaming to a verified temp file; per-remote concurrency limits, negative
  caching, and circuit breaking are designed but not all implemented.
* **Generated-output redirections** (§32), **FSMonitor endpoint** (§38 — the
  journal exists; the Git-facing endpoint does not), and the **socketed daemon**
  (§39 — the in-process controller/registry exists; the Unix-socket/named-pipe
  control server and its auth do not).
* **Operation-log undo/restore** commands (§13) — the log + recovery exist; the
  `op undo`/`op restore` commands are not implemented.

## Not implemented (explicit non-goals for now)

* **Kernel mounting on any platform.** The FUSE backend logic (`FuseOps`) is
  implemented and tested, but the libfuse FFI adapter that performs a real mount
  is not built (this environment has no libfuse). **macOS FSKit and Windows
  ProjFS are documented scaffolds only** and are not production-ready (§54).
* **Git LFS** (§26) — policy modes designed; not implemented. A local Git blob
  being present does not imply its LFS object is present.
* **Submodules** (§33) — gitlink mode modeled; nested lazy mounts not
  implemented.
* **Wrapped/arbitrary stock Git** (Levels C/D, §37) — not exposed.
* First-milestone non-goals from the spec remain out of scope: perfect porcelain
  compatibility, transparent lazy commit-graph history, LFS locking,
  cross-repository object dedup, device/socket/FIFO under version control,
  hard-link identity preservation, distributed operation-log sync, a custom
  hosting service.

## Behavioral notes (correct, but worth knowing)

* **Exact `stat` may fetch + filter content**, and the size is
  platform-dependent under `autocrlf` (see [metadata-limitations.md](metadata-limitations.md)).
* **`git lazy-mount stats` is per-process**: each CLI invocation opens a fresh
  provider, so metrics reflect only that invocation. A persistent daemon would
  accumulate them.
* **A `cat-file --batch` session with `GIT_NO_LAZY_FETCH` dies on a missing
  promisor object** — handled by making the provider the residency authority
  (see [git-object-fetching.md](git-object-fetching.md)).
* **Path arguments to the CLI are repo-root-relative**, not resolved against a
  subdirectory cwd (a convenience to add later).

## Platform path representation

Invalid/colliding platform paths are handled per a configurable policy
(`fail-on-discovery` / `preflight` / `hide` / `escape`; §30). The path layer
(`glm-core::RepoPath`) already rejects NUL/absolute/traversal/empty components
and preserves arbitrary non-UTF-8 bytes with reversible escaping; the
platform-collision *policies* themselves are designed and partially enforced at
the backends (which are scaffolds). `escape` is never the default.
