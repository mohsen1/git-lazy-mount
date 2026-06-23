# Platform: macOS (FSKit) — spec §41

> **Status — SCAFFOLD ONLY. Not implemented, not production-ready (spec §54).**
> macOS support is **not** claimed to work. `glm-fs-fskit::backend_available()`
> returns `false` and `mount()` returns
> `ErrorCode::FilesystemBackendUnavailable`. The backend logic shared with other
> platforms exists (`glm-fs-common`, `glm-workspace`); none of the macOS-specific
> integration below is built. This document tracks what would be required and is
> the place to record progress. Nothing here should be read as a feature claim.

The intended backend is an **FSKit** file-system extension. On macOS versions
without usable FSKit, an **isolated macFUSE** backend may be offered instead —
as an explicit, separate backend, never by silently changing semantics.

The cross-platform callback surface is the same one
[`FuseOps`](../crates/fs-fuse/src/lib.rs) exposes; the macOS work is the bridge
from FSKit (or macFUSE) callbacks into that engine, plus the platform-specific
concerns below.

## What is required before macOS can be labeled supported

### FSKit extension (or isolated macFUSE)

* An `FSUnaryFileSystem`/`FSVolume` FSKit extension bridging the same callbacks
  `FuseOps` implements (`lookup`, `getattr`, enumerate, `read`, `readlink`,
  `forget`, and the write callbacks).
* For older systems lacking FSKit: an isolated macFUSE backend, kept behind a
  distinct backend boundary.

### Runtime capability detection + diagnostics

Detect at runtime whether a usable FSKit (or macFUSE) backend is present and emit
clear installation diagnostics. `backend_available()` is the seam for this; it
returns `false` today.

### APFS case-sensitivity and Unicode normalization

APFS volumes are normally **case-insensitive** and perform **Unicode
normalization**. Two repo paths that Git treats as distinct byte strings can
**collide** on disk (case-only differences, or NFC/NFD-equivalent names). The
backend must detect and surface such collisions rather than silently merging
entries, and must preserve the exact bytes Git recorded. Repo paths are arbitrary
byte strings (`RepoPath`), which APFS path APIs cannot always represent verbatim.

### Resource forks, Finder metadata, xattrs, file flags

macOS attaches resource forks, Finder metadata (`.DS_Store`, `com.apple.*`
xattrs), and BSD file flags. Policy: these are **never silently committed** as
Git content (spec §41). The backend must decide, explicitly, what to expose,
what to ignore, and what to persist locally without it leaking into commits.

### File coordination

Cooperate with NSFileCoordination so coordinated readers/writers (Finder,
document-based apps) see consistent state and the backend honors coordination
intents.

### Case-only rename

`a.txt` → `A.txt` on a case-insensitive volume is a rename to a path that
"already exists" by the volume's comparison rules. This must be handled
correctly (identity preserved, as the inode table guarantees) and tested
on-device.

### System-extension lifecycle + signing/entitlements

* System-extension activation/deactivation, including user approval flows.
* Code signing and the entitlements an FSKit extension requires.
* Behavior across macOS updates and extension reloads.

### Recovery after extension or daemon restart

If the FSKit extension (or the controlling daemon) restarts, mounts must recover
to a consistent state. This rides on the engine's crash-safe operation log and
the daemon lifecycle states (`Recovering`, etc.; `glm-daemon`, spec §39), but the
macOS-specific re-attach path is unbuilt and must be validated.

## Data root

`glm-platform` ([roots.rs](../crates/platform/src/roots.rs)) places macOS state
under `~/Library/Application Support/git-lazy-mount` and caches under
`~/Library/Caches/git-lazy-mount`.

## Tracking

Real FSKit behavior must be validated **on-device** before macOS is labeled
supported (spec §54). Record findings and progress in this file.
