# Platform: macOS (FSKit) — spec §41

> **Status — BACKEND LOGIC BUILT; ON-DEVICE MOUNT NOT YET VALIDATED (spec §54).**
> The backend-independent macOS logic is implemented and unit-tested on every
> platform: the FSKit `FSVolume` callback bridge (`FskitOps`), runtime capability
> detection + diagnostics, APFS collision handling, the macOS metadata commit
> policy, the coordination/recovery models, and the on-device validation harness.
> `glm-fs-fskit::backend_available()` now *probes* the host instead of returning a
> hardcoded `false`. **What remains is on-device:** the signed FSKit system
> extension + the Swift `FSVolume` adapter, validated on real Apple hardware via
> the manual CI job (issue #12). Until that lands and is run, macOS is **not**
> labeled supported — a green default CI never implies a working macOS mount.

The intended backend is an **FSKit** file-system extension. On macOS versions
without usable FSKit, an **isolated macFUSE** backend may be offered instead —
as an explicit, separate backend, never by silently changing semantics.

The cross-platform callback surface is the same one
[`FuseOps`](../crates/fs-fuse/src/lib.rs) exposes; the macOS work is the bridge
from FSKit (or macFUSE) callbacks into that engine, plus the platform-specific
concerns below.

## What is required before macOS can be labeled supported

### FSKit extension (or isolated macFUSE) — issue #5

* **Built:** `FskitOps` (`crates/fs-fskit/src/bridge.rs`) is the FSKit `FSVolume`
  callback logic — the macOS analog of `FuseOps` — over the same `Workspace` and
  `InodeTable`: `lookup`, `getattr`, `enumerate`, `read`, `readlink`, `forget`,
  and the write callbacks (`create`, `write`, `truncate`, `set_executable`,
  `remove`, `rename`, `symlink`). Every write routes through the shared overlay →
  stage → operation-log path; there are **no macOS-only write semantics**.
* **Built:** `MacBackend` is the explicit FSKit-vs-macFUSE selection. The two are
  distinct backend boundaries; macFUSE is only ever chosen explicitly, never by
  silently changing semantics.
* **On-device (issue #12):** the Swift `FSUnaryFileSystem`/`FSVolume` adapter that
  calls into `FskitOps`, plus its signed system extension.

### Runtime capability detection + diagnostics — issue #6

* **Built:** `Capability::detect` (`crates/fs-fskit/src/capability.rs`) probes the
  host — macOS version (third-party FSKit needs ≥ 15.4), whether our system
  extension is installed and *approved*, and whether macFUSE is present — and
  selects a backend (or none). `backend_available()` is now this probe, not a
  hardcoded `false`.
* **Built:** when no backend is available, `mount()` and `git lazy-mount doctor`
  emit concrete, ordered install/approval steps (and point at the headless
  fallback) instead of "not implemented yet".

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
