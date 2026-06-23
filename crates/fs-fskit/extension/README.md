# git-lazy-mount FSKit module (macOS) — issues #5/#10

The on-device macOS backend: a Swift FSKit **app extension** whose `FSVolume`
operations are served by the shared git-lazy-mount engine through the
[`glm-fskit-ffi`](../../fskit-ffi) C ABI. This is the macOS counterpart to the
Linux FUSE adapter (`glm-fs-fuse`) — the same tested engine, a platform shell.

> **On-device.** None of this is compiled by the cross-platform Rust build, and a
> signed build requires an Apple developer identity on real Apple hardware. A
> green default CI never implies any of this was exercised (spec §54).

```
GitLazyMount/            SwiftUI host app (exists so the system can discover +
                         enable the embedded extension)
GitLazyMountFS/          the FSKit module (app extension)
  GitLazyMountFSExtension.swift   @main UnaryFileSystemExtension
  GlmFileSystem.swift             FSUnaryFileSystem: loadResource → open workspace
  GlmVolume.swift                 FSVolume.Operations/ReadWrite/OpenClose → FFI
  GlmItem.swift                   FSItem addressed by engine inode
  GlmSupport.swift                errno/attr/byte-name bridging
  glm_fskit.h                     C ABI (matches crates/fskit-ffi)
  *-Bridging-Header.h             exposes the C ABI to Swift
  Info.plist                      EXAppExtensionAttributes (FSShortName=gitlazymount)
  GitLazyMountFS.entitlements     app-sandbox + com.apple.developer.fskit.fsmodule
project.yml              xcodegen spec (host app + appex, links the Rust .a)
build.sh                 build the Rust staticlib + generate + build/sign the app
```

## Build · install · enable · mount

```sh
./build.sh                          # Rust staticlib → xcodegen → xcodebuild (signed)
cp -R DerivedData/Build/Products/Release/GitLazyMount.app /Applications/
open /Applications/GitLazyMount.app                               # registers the module
pluginkit -mAv -p com.apple.fskit.fsmodule | grep gitlazymount   # verify "+" registration
# System Settings → General → Login Items & Extensions → File System Extensions → enable
mount -t gitlazymount <registered-mountpoint> <dir>
```

`build.sh` needs Rust (+ the `aarch64-apple-darwin` target), `xcodegen`, Xcode,
and a paid Apple team signed into Xcode (`DEVELOPMENT_TEAM`, default
`HESNS6JK33`).

## Signing facts (validated on macOS 26.4.1)

No SIP changes, **no notarization, no Developer ID, no `OSSystemExtension`** — it
is a plain app extension that Xcode automatic-signing provisions:

* Entitlement **`com.apple.developer.fskit.fsmodule`** self-serves under any paid
  team via automatic signing once the **Program License Agreement** is accepted.
* `DEVELOPMENT_TEAM` is the team ID = the signing cert's **`OU`** (not the
  parenthetical in the cert's common name — that bit us repeatedly).
* The host app and the embedded appex must sign with the **same** identity (one
  "Apple Development" cert; a duplicate causes "embedded binary is not signed
  with the same certificate as the parent app").
* Build **Release** (the Debug "debug dylib" indirection is a poor fit for an
  extension the system must validate).

Full runbook + the current blocker:
[`docs/platform-macos-fskit-ondevice.md`](../../../docs/platform-macos-fskit-ondevice.md).

**Open blocker (issue #19):** on macOS 26.4.1 the System Settings *enablement*
toggle for File System Extensions is inert for **every** third-party FSKit module
(Apple's own `Passthrough` sample included), so the final `mount` can't proceed.
The module builds, signs, links the engine, and **registers** — verified — but
cannot be enabled on this OS build.

## doctor / activation lifecycle

Surfaced by `git lazy-mount doctor` (`fskit_extension_state` + `fskit_next_step`),
modeled in [`lifecycle.rs`](../src/lifecycle.rs):

| State | Meaning | Next step |
|-------|---------|-----------|
| `unsupported` | not macOS, or macOS < 15.4 | upgrade macOS, or use macFUSE |
| `not_installed` | FSKit present; our module not registered | install + launch the app |
| `awaiting_approval` | registered, not yet enabled | enable in System Settings → File System Extensions |
| `activated` | enabled & active | ready to mount |

A major macOS update can require re-enabling; `Capability::detect` re-reads the
live state each run so `doctor` reflects reality, not a cached assumption.

## Known gaps (this validation build)

* **Sandbox vs. `git`.** FSKit extensions are sandboxed; the FFI currently opens
  the workspace and shells out to `git` in-process, which the sandbox restricts.
  Production should proxy FS callbacks to `glm-daemon` over XPC/IPC (the daemon
  runs `git` outside the sandbox). The FFI is the validation shim.
* **`mkdir`** returns `ENOTSUP` (Git has no empty trees; directories materialize
  on first child write).
