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

### APFS case-sensitivity and Unicode normalization — issue #7

* **Built:** `glm_platform::validate::macos_collision_key` folds names per a
  concrete volume (`AppleVolume::CaseInsensitive` / `CaseSensitive`); both APFS
  variants are normalization-insensitive, so NFC/NFD-equivalent names fold
  together on either.
* **Built:** `crates/fs-fskit/src/collision.rs` + the bridge use it:
  * `enumerate` returns every entry's **exact recorded bytes**;
    `directory_collisions` reports the sets that fold together, so a directory is
    never silently merged;
  * `lookup` fuzzy-resolves to the single matching entry's exact bytes (NFC↔NFD,
    case-insensitive), and surfaces `PlatformPathCollision` when **two distinct**
    Git entries fold together rather than picking one;
  * `create` / `symlink` reject a new name that would collide with a sibling.
* **Built:** real-FS tests on the macOS host assert the resolver agrees with the
  volume's actual case/normalization behavior (in addition to the existing
  `validate.rs` NFC/NFD real-FS test).
* **On-device (issue #12):** end-to-end validation through a real FSKit mount on
  both case-insensitive and case-sensitive APFS volumes.

### Resource forks, Finder metadata, xattrs, file flags — issue #8

* **Built:** `glm_platform::metadata` is the single, documented **policy table**
  (`.DS_Store` / `._*` → `Ignored`; xattrs incl. resource forks / Finder info /
  quarantine → `OverlayOnly`; BSD file flags → `OverlayOnly`).
* **Enforced:** the workspace staging path screens `Ignored` paths
  (`is_never_committed_path`), so `.DS_Store` / `._*` can never reach a staged
  tree or commit — on `add`, `add -A`, **or** the git-interop bridge. xattrs /
  resource forks / file flags have no Git commit channel at all, so they are
  structurally never committed. A workspace integration test verifies this
  directly against the committed tree (root and nested).

### File coordination — issue #9

Cooperate with NSFileCoordination so coordinated readers/writers (Finder,
document-based apps) see consistent state and the backend honors coordination
intents.

* **Built:** `crates/fs-fskit/src/coordination.rs` — a per-path reader/writer
  `Coordinator` modeling the `NSFileCoordinator` intents the adapter receives:
  coordinated writes to a path are mutually exclusive and no coordinated read
  overlaps an in-flight write (concurrency tests assert both). The on-device
  adapter wraps each `FskitOps` callback in `coordinate(path, intent, …)`.
* **On-device (issue #12):** wiring to the real `NSFileCoordinator` and
  validation with Finder + a document-based app.

### Case-only rename — issue #7

* **Built:** `a.txt` → `A.txt` on a case-insensitive volume targets a name that
  "already exists" by the volume's comparison, but the bridge recognizes the
  folding-only rename (`collision::is_case_only_rename`) and performs it,
  preserving identity via the inode table (spec §19). A bridge test covers
  identity + content preservation.
* **On-device (issue #12):** validation through a real FSKit mount.

### System-extension lifecycle + signing/entitlements — issue #10

* **Built:** `crates/fs-fskit/extension/` carries the packaging — `Info.plist`
  (FSKit `FSModuleType` declaration), `git-lazy-mount.entitlements` (notably
  `com.apple.developer.fskit.fsmodule`), and `README.md` (the reproducible
  signed-build steps + the activation/deactivation/approval lifecycle, incl.
  behavior across OS updates).
* **Built:** `crates/fs-fskit/src/lifecycle.rs` derives an `ExtensionState`
  (`unsupported` / `not_installed` / `awaiting_approval` / `activated`) from the
  capability probe and the concrete next step; `git lazy-mount doctor` surfaces
  it (`fskit_extension_state`, `fskit_next_step`).
* **On-device (issue #12):** a reproducible signed build and the live
  activation/approval flow on Apple hardware with a Developer identity.

### Recovery after extension or daemon restart — issue #11

If the FSKit extension (or the controlling daemon) restarts, mounts must recover
to a consistent state. This rides on the engine's crash-safe operation log and
the daemon lifecycle states (`Recovering`, etc.; `glm-daemon`, spec §39).

* **Built:** `crates/fs-fskit/src/recovery.rs` — `reattach(ws, volume)` replays
  the operation log and drives the FSKit re-attach through `Recovering → Mounted`
  (or `Failed`), returning a fresh `FskitOps` (the kernel re-issues `lookup`, so
  inode identity is rebuilt on demand; numbers are never reused). Tests simulate
  an extension/daemon restart by re-opening from the same on-disk state and
  assert **no data loss**: an uncommitted overlay edit survives, and a committed
  base is preserved.
* **On-device (issue #12):** induced extension/daemon restarts against a live
  FSKit mount.

## Data root

`glm-platform` ([roots.rs](../crates/platform/src/roots.rs)) places macOS state
under `~/Library/Application Support/git-lazy-mount` and caches under
`~/Library/Caches/git-lazy-mount`.

## Tracking

Real FSKit behavior must be validated **on-device** before macOS is labeled
supported (spec §54). Record findings and progress in this file.
