# Road not taken: Windows (ProjFS)

> **git-lazy-mount is Linux-only.** A Windows backend on the Projected File
> System (ProjFS) was investigated and **not pursued**. There is no
> `fs-projfs` crate, no Windows backend, and no `backend_available()` symbol in
> the tree. This note records why ProjFS would have been a distinct architecture
> and what a hypothetical future Windows backend would have to solve, so the
> analysis is not lost. Nothing here is built. For why future platforms are out
> of scope, see [limitations.md](../limitations.md).

## Why ProjFS would be a separate architecture

ProjFS is not "FUSE with Windows callbacks." It is a filter driver layered on a
real NTFS volume, not a synthetic filesystem answering every `stat`. A Windows
backend could not reuse the FUSE callback shape; its reconciliation and metadata
requirements are genuinely different:

* The projection materializes as **placeholders that NTFS persists**, rather
  than living entirely in a provider that owns every lookup.
* Hydration is **asynchronous and cancellable**, and many notifications are
  post-operation and may arrive out of transactional order.

The FUSE engine in `crates/fuse` assumes it is the source of truth for every
operation. ProjFS does not give a provider that guarantee, so the two cannot
share a callback layer.

## What a future Windows backend would have to solve

These are open design problems, not implemented features.

### Placeholders and a content identity

ProjFS placeholders carry a provider-defined **ContentID**. It would need to
identify the logical content *and* the filter context, not just the path. The
same path can project different bytes depending on the active filter/attribute
context (see CRLF below), so a path alone is insufficient identity, and a stale
ContentID after a context change must be detectable so ProjFS re-requests
content.

### File size at placeholder time

When a placeholder is created during directory enumeration, ProjFS requires the
file size up front, before any content is hydrated. The provider would have to
supply the exact projected size at enumeration time — the Windows analogue of
the engine's exact-size stat policy (see [object-fetching.md](../object-fetching.md)),
and it would have to account for filter-dependent size (CRLF).

### Async hydration and callback cancellation

* `GetFileDataCallback` hydration is asynchronous and may be cancelled (the app
  aborted, the handle closed). Cancellation must abort cleanly without leaving a
  partially hydrated placeholder marked complete.
* As on Linux, hydration would go through git object access, and a projection
  callback must never trigger an interactive credential prompt
  (`git-store` sets `GIT_TERMINAL_PROMPT=0` throughout).

### Post-op notifications, reconciliation journal, startup reconcile

ProjFS delivers post-operation notifications (file opened/created/modified/
renamed/deleted) that can arrive out of order relative to a transactional view.
A backend would have to:

* Record them in a **reconciliation journal**, then fold them into the
  transactional state in a defined order.
* Run a **startup reconcile** that compares the on-disk projection
  (placeholders, full files, tombstones) against workspace state before serving,
  to repair drift from notifications lost across a crash or restart.

This is conceptually similar to the Linux change journal (see
[fsmonitor.md](../fsmonitor.md)), but it would be a distinct, Windows-specific
journal because the ordering hazard is ProjFS-specific.

### Offline modification

ProjFS allows the volume to be touched without the provider attached. The
startup pass would have to detect and reconcile changes made while the provider
was not running, rather than assuming the projection only ever changes through
live callbacks.

### NTFS / Win32 naming and metadata hazards

NTFS and Win32 differ sharply from POSIX. A Git path that is a valid byte string
may be unrepresentable or ambiguous on NTFS, and such paths must be surfaced,
not silently mangled:

* **Reserved device names** (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`).
* **Invalid characters** (`< > : " / \ | ? *`) and control characters.
* **Trailing dots and spaces**, which Win32 silently strips.
* **Long paths** (the `MAX_PATH` limit and `\\?\` long-path handling).
* **Reparse points** and a deliberate **symlink policy** (symlink creation is
  privileged on Windows; decide expose/deny/emulate).
* **Alternate Data Streams (ADS):** what to project, ignore, or persist — never
  silently committed as Git content.

### Antivirus and indexer interaction

Real-time antivirus and the Windows Search indexer open and read files,
triggering hydration and notifications. They can hydrate content the user never
touched and race with provider operations, so their interaction with
placeholders, cancellation, and the reconciliation journal would need
evaluation.

### WinFsp as an explicit, separate fallback

If ProjFS could not provide required semantics on a supported Windows version, a
WinFsp backend would be a separate, explicitly chosen backend that never hides
the semantic differences between the two — not a drop-in for ProjFS.

## A past finding: `core.autocrlf=true` and platform-dependent size

Git for Windows ships `core.autocrlf=true` in its system config by default. The
shipped engine performs faithful filtering: it applies Git's own working-tree
(smudge) filters via `cat-file --filters` with the correct `--attr-source` (the
workspace base commit), in
[`GitStore::smudge_blob`](../../crates/git-store/src/store.rs). On a host
configured like Git for Windows, faithful filtering would produce CRLF line
endings for affected files, exactly as a real `git checkout` would.

That makes the exact `stat` size **platform-dependent**: the same blob can
project a different byte length depending on the active EOL/filter context. This
is expected behavior, not a bug, and it is precisely why a placeholder's
ContentID and size would need to encode the filter context rather than just the
path.

The Linux tests pin this deterministically so behavior is reproducible on any
host: the integration tests set `core.autocrlf=false` on the test store
(`crates/git-store/tests/store_integration.rs:21`) so faithful filtering does
not inject host-dependent CRLF. The faithful-smudge mechanism and the
`--attr-source` rationale are owned by [object-fetching.md](../object-fetching.md)
and [worktree-model.md](../worktree-model.md).

## Data root

There is no Windows data-root logic. Data-dir selection is Linux-only, in
`crates/cli/src/main.rs` (`data_dir`): `$XDG_DATA_HOME/git-lazy-mount` else
`$HOME/.local/share/git-lazy-mount`. A future Windows backend would have placed
state under `%LOCALAPPDATA%\git-lazy-mount`, but no such code exists.
