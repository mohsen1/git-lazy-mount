# Platform: Windows (ProjFS)

> **Status: SCAFFOLD ONLY. Not implemented, not production-ready.**
> Windows support is **not** claimed to work. `glm-fs-projfs::backend_available()`
> returns `false` and `mount()` returns
> `ErrorCode::FilesystemBackendUnavailable`. This document tracks what would be
> required and is the place to record progress.

## ProjFS is a distinct architecture

The Windows backend is built on Windows Projected File System (ProjFS). It is
intentionally a separate backend (`glm-fs-projfs`), not "FUSE with Windows
callbacks." ProjFS has a fundamentally different model from FUSE, and the design
has to respect those differences instead of papering over them:

* The projection is materialized as placeholders that NTFS persists. ProjFS is a
  filter driver layered on a real volume, not a synthetic FS answering every
  `stat`.
* Hydration is asynchronous and cancellable, and many notifications are
  post-operation and may arrive out of transactional order.

The ProjFS backend therefore cannot reuse the FUSE callback shape. Its
reconciliation and metadata requirements are genuinely different.

## What is required before Windows can be labeled supported

### Placeholders and ContentID

ProjFS placeholders carry a provider-defined **ContentID**. It must identify the
logical content and the filter context, not just the path. The same path can
project different bytes depending on the active filter/attribute context (see
CRLF below), so a path alone is an insufficient identity. A stale ContentID after
a context change must be detectable so ProjFS re-requests content.

### Required file-size metadata at placeholder time

When a placeholder is created (during directory enumeration), ProjFS requires the
file size up front, before any content is hydrated. The provider must supply the
exact projected size at enumeration time. This is the Windows analogue of the
engine's exact-size stat policy, and it has to account for filter-dependent size
(CRLF).

### Async hydration and callback cancellation

* `GetFileDataCallback` hydration is asynchronous and may be cancelled (the app
  aborted, the handle closed). Cancellation must abort cleanly without leaving a
  partially hydrated placeholder marked complete.
* As with FUSE, hydration goes through the object provider. A projection callback
  must never trigger an interactive credential prompt
  (`GIT_TERMINAL_PROMPT=0` throughout `glm-git-store`).

### Post-op notifications, reconciliation journal, startup FSCK

ProjFS delivers post-operation notifications (file opened/created/modified/
renamed/deleted) that can arrive out of order relative to the engine's
transactional view. The backend must:

* Record them in a **reconciliation journal**, then fold them into the
  transactional state in a defined order.
* Run a **startup FSCK** that reconciles the on-disk projection (placeholders,
  full files, tombstones) against the workspace state before serving. This
  repairs drift from notifications lost across a crash or restart.

This mirrors the engine's append-only operation log discipline (CURRENT advanced
last; see the durable change journal), but it is a distinct, Windows-specific
journal because the ordering hazard is ProjFS-specific.

### Offline-modification reconciliation

If files were modified while the provider was not running (ProjFS allows the
volume to be touched without the provider attached), the startup pass must detect
and reconcile those changes rather than assuming the projection only ever changes
through live callbacks.

### Windows path and filesystem semantics

The backend must handle the full set of NTFS/Win32 naming and metadata hazards,
which differ sharply from POSIX:

* **Reserved device names** (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`).
* **Invalid characters** (`< > : " / \ | ? *`) and control characters.
* **Trailing dots and spaces**, which Win32 silently strips.
* **Long paths** (the `MAX_PATH` limit and `\\?\` long-path handling).
* **Reparse points** and a deliberate **symlink policy** (symlink creation is
  privileged on Windows; decide expose/deny/emulate).
* **Alternate Data Streams (ADS):** what to project, ignore, or persist. Never
  silently commit them as Git content.

A Git path that is a perfectly valid byte string may be unrepresentable or
ambiguous on NTFS. Such paths must be surfaced, not silently mangled.

### Antivirus and indexer interaction

Real-time antivirus and the Windows Search indexer open and read files,
triggering hydration and notifications. Their interaction with placeholders,
cancellation, and the reconciliation journal has to be evaluated. They can
hydrate content the user never touched, and they can race with provider
operations.

### WinFsp as an explicit, separate fallback

If ProjFS cannot provide required semantics on a supported Windows version, a
WinFsp backend would be added as an explicit, separate backend that never hides
the semantic differences between the two. WinFsp is a distinct option, not a
drop-in for ProjFS.

## The one Windows behavior already handled and tested: `core.autocrlf=true`

Git for Windows ships `core.autocrlf=true` in its system config by default.
git-lazy-mount performs faithful filtering: it applies Git's own working-tree
(smudge) filters via `cat-file --filters` with the correct `--attr-source` (the
workspace base commit; see ADR 0007). On a host configured like Git for Windows,
faithful filtering produces CRLF line endings for affected files, exactly as a
real `git checkout` would.

So the exact `stat` size is platform-dependent. The same blob can project a
different byte length depending on the active EOL/filter context. That is
expected behavior, not a bug, and it is exactly why a placeholder's ContentID and
size must encode the filter context rather than just the path.

git-lazy-mount pins this deterministically in its tests so behavior is
reproducible on any host:

* `glm-workspace`, `glm-object-provider`, and `glm-fs-fuse` integration tests set
  `core.autocrlf=false` on the test store, so faithful filtering does not inject
  host-dependent CRLF
  ([fs-fuse/src/lib.rs](../crates/fs-fuse/src/lib.rs) notes the Git-for-Windows
  default explicitly).
* The CLI end-to-end test injects `core.autocrlf=false` via `GIT_CONFIG_*` /
  `GIT_CONFIG_NOSYSTEM` so the binary's filtering is host-independent
  ([cli/tests/cli_e2e.rs](../crates/cli/tests/cli_e2e.rs)).
* `workspace_integration::crlf_filter_applied_faithfully` asserts the opposite
  direction: with `.gitattributes` forcing `*.txt text eol=crlf`, a faithful read
  of an LF-stored blob yields CRLF (`line1\r\nline2\r\n`), while the raw blob
  stays LF. Filtering is applied on read, never baked into the object.

So the CRLF/size-is-platform-dependent fact is understood, deterministically
pinned, and surfaced as expected behavior. The rest of the ProjFS backend above
is unbuilt.

## Data root

`glm-platform` ([roots.rs](../crates/platform/src/roots.rs)) places Windows state
under `%LOCALAPPDATA%\git-lazy-mount` (`cache`, `state`, `config`, `data`).

## Tracking

Real ProjFS behavior must be validated on Windows before it is labeled supported.
Record findings and progress in this file.
