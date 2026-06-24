# Feasibility: Windows ProjFS

**Question.** Can the projection run on Windows Projected File System, and what
is required (spec §5.5, §42)?

## Status

**Not implemented**, and real ProjFS behavior is **not testable in this
environment** (no Windows host with ProjFS). `glm-fs-projfs` is a documented
scaffold. Windows is explicitly **not** production-ready (spec §54). ProjFS is a
**distinct architecture** — not "FUSE with Windows callbacks" — and is treated
as such.

## What CI already told us about Windows

The cross-platform CI runs the full test suite on `windows-latest`. It surfaced
a real, correct behavior: **Git for Windows ships `core.autocrlf=true` in system
config**, so faithful filtering projects `hello\n` as `hello\r\n` (7 bytes) and
exact `stat` size is platform-dependent. This is expected behavior; tests pin
`core.autocrlf=false` for determinism (see `feasibility/file-metadata.md`). It
confirms the projection logic and Git interop work on Windows at the
backend-logic level even though no kernel projection is mounted.

## Open questions to resolve on Windows (before any support claim)

* Placeholder creation and directory enumeration sessions; a **ContentID** that
  identifies the logical content **and filter context**, not merely a path.
* Required file-size metadata at placeholder time (ties to the exact-size
  problem); async hydration; callback cancellation.
* Post-operation notifications that may arrive **after** an operation and out of
  transactional order ⇒ a reconciliation journal + startup FSCK.
* Reconciling files modified while the provider was not running.
* Reserved device names, forbidden characters, trailing dots/spaces, long paths,
  reparse points, junctions, symlink policy (native/text/error), alternate data
  streams, case-insensitive lookup.
* Antivirus and indexer access patterns.
* WinFsp only as an explicit, separately-maintained fallback backend — never
  hiding semantic differences.

## Decision

Defer ProjFS until it can be validated on Windows with real filesystem tests
(spec §54). The shared logic (and Git interop) is exercised on Windows CI today;
the ProjFS bridge is future work tracked in `docs/platform-windows.md`.
