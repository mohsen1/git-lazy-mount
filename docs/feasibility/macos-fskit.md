# Feasibility: macOS FSKit

**Question.** Can the projection run on Apple FSKit (with macFUSE as a fallback
for older systems), and what is required (spec §5.5, §41)?

## Status

**Not testable in this environment** (no macOS host) and **not implemented.**
`glm-fs-fskit` is a documented scaffold. macOS is explicitly **not**
production-ready (spec §54); we will not label it supported on the basis of
compilation.

## What can be reused

The backend-independent logic — workspace resolution, the `InodeTable`, neutral
`FileAttr`, the object provider — is shared. Only the OS bridge is
macOS-specific, and it should expose the same operations as `FuseOps`.

## Open questions to resolve on-device (before any support claim)

* FSKit `FSUnaryFileSystem`/`FSVolume` extension lifecycle, signing, and
  entitlements; runtime capability detection and installation diagnostics; an
  isolated macFUSE backend for OS versions lacking FSKit.
* APFS case-sensitive **and** case-insensitive volumes; Unicode normalization
  collisions; case-only rename.
* Resource forks, Finder metadata, extended attributes, and file flags — policy
  must be explicit (`ignored` / `overlay-only` / `rejected`); never silently
  commit them as Git content.
* File coordination (NSFileCoordinator) interaction.
* Mount recovery after a daemon or system-extension restart.

## Decision

Defer FSKit until it can be validated on real hardware with real filesystem
tests (spec §54). The shared logic is ready; the macOS bridge is future work
tracked in `docs/platform-macos.md`.
