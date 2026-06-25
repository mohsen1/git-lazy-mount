# Feasibility: macOS FSKit

**Question.** Can the projection run on Apple FSKit (with macFUSE as a fallback
for older systems), and what is required?

## Status

Backend logic is implemented and tested. The on-device mount is not yet validated.
`glm-fs-fskit` now carries the full backend-independent macOS implementation: the
`FskitOps` `FSVolume` bridge, runtime capability detection plus diagnostics, APFS
case/normalization collision handling, the macOS metadata commit policy, the
NSFileCoordination cooperation model, the system-extension lifecycle, and the
mount-recovery re-attach path. All of it is unit-tested on every platform, with
extra real-FS tests on macOS hosts. macOS is still explicitly **not**
production-ready. The signed FSKit system extension and Swift `FSVolume` adapter
have to be validated on real Apple hardware via the manual CI job (issue #12)
before macOS is labeled supported. See [`platform-macos.md`](macos.md) for the
per-sub-issue status table.

## What can be reused

The backend-independent logic is shared: workspace resolution, the `InodeTable`,
neutral `FileAttr`, the object provider. Only the OS bridge is macOS-specific,
and it should expose the same operations as `FuseOps`.

## Open questions to resolve on-device (before any support claim)

* FSKit `FSUnaryFileSystem`/`FSVolume` extension lifecycle, signing, and
  entitlements; runtime capability detection and installation diagnostics; an
  isolated macFUSE backend for OS versions lacking FSKit.
* APFS case-sensitive and case-insensitive volumes; Unicode normalization
  collisions; case-only rename.
* Resource forks, Finder metadata, extended attributes, and file flags. Policy
  must be explicit (`ignored` / `overlay-only` / `rejected`), and we never
  silently commit them as Git content.
* File coordination (NSFileCoordinator) interaction.
* Mount recovery after a daemon or system-extension restart.

## Decision

The backend-independent macOS implementation has landed: the bridge, capability
detection, collision handling, metadata policy, coordination, lifecycle, and
recovery. The open questions above are resolved in software and covered by
tests. The remaining gate is on-device. We need a signed FSKit extension and the
Swift `FSVolume` adapter, validated on real Apple hardware with real filesystem
tests via the manual CI job (issue #12). macOS is **not** labeled supported until
that run lands. Progress is tracked in `docs/platform-macos.md`.
