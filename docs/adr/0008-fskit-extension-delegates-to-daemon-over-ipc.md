# 0008 — The FSKit extension delegates filesystem callbacks to the daemon over IPC

**Status:** Proposed (validated up to module registration on macOS 26.4.1; the
end-to-end mount is blocked by an OS enablement bug — see issue #19 and
[../platform-macos-fskit-ondevice.md](../platform-macos-fskit-ondevice.md)).

## Context

The macOS backend is a Swift FSKit **app extension**
([`crates/fs-fskit/extension/`](../../crates/fs-fskit/extension/)) whose
`FSVolume` operations are served by the shared engine. Today it links the
[`glm-fskit-ffi`](../../crates/fskit-ffi) static library, which opens the
workspace and serves callbacks **in-process** — including running `git` (lazy
hydration, smudge filters) via [`glm-git-store`](../../crates/git-store).

But FSKit modules are mandatorily **sandboxed**
(`com.apple.security.app-sandbox`; the entitlement is required to load). A
sandboxed process cannot freely `posix_spawn` the `git` binary, nor reach the
network for lazy fetches, nor open arbitrary paths in the shared store. So the
in-process FFI — while it proved build/sign/registration end-to-end — cannot be
the production data path: the first lazy `read` that needs `git` would be denied.

This is the same engine/transport split the project already uses elsewhere: the
backend-neutral engine (`FskitOps`, `FuseOps`) is shared, and a per-platform
shell adapts it. On Linux, FUSE runs in the **same** unsandboxed process as the
engine, so in-process `git` is fine (see
[../platform-linux.md](../platform-linux.md)). macOS is different precisely
because FSKit forces the sandbox.

## Decision

The FSKit extension **does not run the engine in-process**. It is a thin XPC/IPC
**client** of the per-user [`glm-daemon`](../../crates/daemon), which runs
**outside** the sandbox and owns the workspace, the bare store, and all `git`
subprocesses. Each `FSVolume` callback (`lookup`/`getAttributes`/`read`/
`enumerate`/`write`/…) is marshalled to the daemon and served there.

* The wire shapes reuse [`glm-ipc`](../../crates/ipc) (already the versioned,
  `serde` daemon control protocol over a Unix domain socket), extended with the
  per-inode filesystem operations the extension needs.
* The extension is signed with `com.apple.security.network.client` (a local
  socket connection) in addition to the FSKit entitlement and the sandbox.
* `glm-fskit-ffi` keeps the **same C ABI** but its `glm_fskit_open` connects to
  the daemon socket instead of opening the workspace locally; the operation
  functions become IPC round-trips. The Swift side (`GlmVolume`) is unchanged —
  it already addresses everything by inode through the FFI.
* The daemon never blocks on the network from inside a callback (spec
  §16/§3.13): residency/fetch policy stays the daemon's responsibility, exactly
  as for the other backends.

The current in-process FFI remains as a **validation shim** behind a build flag —
it is what let us confirm signing/registration without the daemon, and it is
usable for unsandboxed harness testing.

## Consequences

* The macOS data path becomes sandbox-legal: `git` and the network live in the
  daemon, never in the extension.
* One engine, two transports: Linux FUSE calls `FskitOps`/`FuseOps` in-process;
  macOS FSKit calls the daemon over IPC. The neutral engine and its tests are
  shared and unchanged.
* New surface to test: the IPC operation set and its serialization (unit-testable
  in Rust without a mount), plus a daemon-side handler that maps requests onto
  `FskitOps`. The Swift extension stays a thin marshaller.
* This is **not yet exercised end-to-end**: third-party FSKit module
  *enablement* is currently broken on macOS 26.4.1 (reproduces on Apple's own
  sample), so no mount — and therefore no live IPC round-trip — has run on-device
  yet. macOS stays **not "supported"** (spec §54) until that OS gate clears and
  this path is validated through a real mount.
