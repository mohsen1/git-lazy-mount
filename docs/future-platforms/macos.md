# Road not taken: macOS (FSKit)

> **Not implemented. The project is Linux-only.**
> macOS (FSKit) was investigated as a backend and is **not pursued**. The
> platform-specific code was prototyped and then **removed** after an Apple 26.x
> FSKit OS bug blocked enabling a third-party file-system extension. There is no
> `fs-fskit` / `platform` crate, no `FskitOps` / `MacBackend` / `Capability` type,
> and `git lazy-mount` ships for Linux only (see the README). This note records
> what a macOS port *would* need and the design choices that were considered, so
> the work can be picked up later. Nothing described here is present-tense built.

The intended backend was an **FSKit** file-system extension. On macOS versions
without usable FSKit, an isolated macFUSE backend could be offered as a separate,
explicitly chosen backend — never substituted silently by changing semantics.

The shipped filesystem surface is `impl Filesystem for TransparentFs` in
[`crates/fuse/src/mount.rs`](../../crates/fuse/src/mount.rs). It is FUSE-specific,
not a cross-platform engine abstraction. A macOS port would need to bridge FSKit
(or macFUSE) callbacks onto the same projection/overlay/journal that
`TransparentFs` drives — the projection, overlay, and durable change journal all
live in [`crates/worktree`](../../crates/worktree) and are backend-agnostic.

## What a macOS port would need

The notes below are **speculative design** — "would need to" / "was considered",
not status. For the concrete build/sign/install/registration runbook and the
exact OS-level blocker that stopped the prototype, see
[`macos-fskit-ondevice.md`](macos-fskit-ondevice.md).

### FSKit extension (or isolated macFUSE)

A Swift `FSUnaryFileSystem`/`FSVolume` adapter would translate FSKit callbacks
(`lookup`, `getattr`, `enumerate`, `read`, `readlink`, plus the write callbacks
`create`, `write`, `truncate`, `remove`, `rename`, `symlink`) onto the projection.
Writes would route through the same overlay + durable `ChangeJournal`
(`crates/worktree`) that the FUSE path uses; there should be **no macOS-only write
semantics**. The FSKit-vs-macFUSE choice would be an explicit backend selection —
macFUSE only ever chosen deliberately, never by silently changing behavior.

### Runtime capability detection + diagnostics

A host probe would check the macOS version (third-party FSKit needs ≥ 15.4),
whether the system extension is installed and **approved**, and whether macFUSE is
present, then select a backend or none. When no backend is available, `mount` and
`git lazy-mount doctor` should emit concrete, ordered install/approval steps. (The
shipped `doctor` reports only `mountpoint` / `mounted` / `show_toplevel` —
[`crates/cli/src/main.rs`](../../crates/cli/src/main.rs) `cmd_doctor` — and has no
FSKit fields.)

### APFS case-sensitivity and Unicode normalization

This is the most macOS-specific concern. Both APFS variants are
normalization-insensitive, so NFC/NFD-equivalent names fold together on either,
and a case-insensitive volume also folds case. A collision-aware resolver would
need to:

* return every directory entry's **exact recorded bytes** on `enumerate`, and
  report sets of names that fold together rather than silently merging a
  directory;
* on `lookup`, fuzzy-resolve (NFC↔NFD, case-insensitive) to the single matching
  entry's exact bytes, and surface a path-collision error when **two distinct**
  Git entries fold together rather than picking one;
* on `create` / `symlink`, reject a new name that would collide with a sibling.

A **case-only rename** (`a.txt` → `A.txt`) on a case-insensitive volume targets a
name that "already exists" by the volume's comparison; the bridge would need to
recognize the folding-only rename and perform it, preserving identity via the
inode table. The shipped projection already keeps raw byte-exact paths
(`crates/core` `RepoPath`), which is the foundation this would build on.

### Resource forks, Finder metadata, xattrs, file flags

A macOS port needs an explicit, documented policy table:

* `.DS_Store` / `._*` → ignored (never reach a staged tree or commit);
* xattrs (resource forks, Finder info, quarantine) and BSD file flags →
  overlay-only.

xattrs / resource forks / file flags have no Git commit channel at all, so they
are structurally never committed regardless. The shipped overlay does **not**
implement this classification today — note that the FUSE build does not implement
any xattr ops (they fall through to `ENOSYS`), so this is greenfield work.

### File coordination (NSFileCoordinator)

To cooperate with coordinated readers/writers (Finder, document-based apps), the
adapter would wrap each callback in an `NSFileCoordinator` intent so coordinated
writes to a path are mutually exclusive and no coordinated read overlaps an
in-flight write. This was considered as a per-path coordinator model; it was not
built.

### System-extension lifecycle, signing, recovery

The Swift FSKit module would need to build, sign (`com.apple.developer.fskit.fsmodule`
entitlement), install, and register with FSKit, then survive the
activation/approval flow on Apple hardware with a Developer identity. After an
extension restart, mounts would recover by replaying the durable journal; the
kernel re-issues `lookup`, so inode identity is rebuilt on demand and numbers are
never reused. The shipped mount lifecycle is a detached hidden `__serve` child
plus a monotonic `MountGeneration` counter
([`crates/core/src/ids.rs`](../../crates/core/src/ids.rs)) — there is no daemon and
no daemon state machine.

**This is where the prototype died.** Both Apple's official `Passthrough` FSKit
sample and the prototype extension built, signed, installed, and registered, but
neither could be **enabled** in System Settings on macOS 26.4.1 — an Apple OS-level
bug. The full runbook and findings are in
[`macos-fskit-ondevice.md`](macos-fskit-ondevice.md). Because the OS itself
blocked enabling the extension, the macOS backend code was retired and the project
is Linux-only.

## Data root

If revived, macOS state would live under the same XDG-style data directory the CLI
already computes: `$XDG_DATA_HOME/git-lazy-mount`, else
`$HOME/.local/share/git-lazy-mount` (`data_dir` in
[`crates/cli/src/main.rs`](../../crates/cli/src/main.rs)). The earlier prototype's
`~/Library/Application Support` placement is not what the shipped CLI uses.
