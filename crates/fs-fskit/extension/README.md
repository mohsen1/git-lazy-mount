# FSKit system extension — signing, entitlements, and lifecycle (issue #10)

> **On-device.** Everything in this directory describes the *signed, on-device*
> packaging of the FSKit module. None of it is compiled by the cross-platform
> Rust build, and producing/validating a signed build requires an Apple Developer
> identity on real Apple hardware (gated by the on-device validation harness,
> issue #12). A green default CI never implies any of this was exercised
> (spec §54).

The FSKit module is the macOS-specific bridge from FSKit `FSUnaryFileSystem` /
`FSVolume` callbacks into the shared, tested Rust logic
([`FskitOps`](../src/bridge.rs)). The Rust side is platform-independent and
unit-tested; this directory is the Swift/codesign packaging around it.

## Files

| File | Purpose |
|------|---------|
| `Info.plist` | Declares the system extension as an FSKit module (`FSModuleType = FSUnaryFileSystem`) under the app-extension point `com.apple.fskit.fsmodule`. |
| `git-lazy-mount.entitlements` | The entitlements the module is signed with — notably `com.apple.developer.fskit.fsmodule`. |

## Required entitlements

* `com.apple.developer.fskit.fsmodule` — **required**; an App ID / provisioning
  profile must carry it, and codesign must apply it.
* `com.apple.security.app-sandbox` — the extension runs sandboxed.
* `com.apple.security.network.client` — lazy object hydration may fetch from the
  remote (still **non-interactive**: a filesystem callback never prompts for
  credentials, spec §3.13).
* `com.apple.security.files.user-selected.read-write` — for the user-chosen
  mountpoint.

## Reproducible signed build (outline)

1. Build the Rust logic as a static library (`cargo build -p glm-fs-fskit
   --release`) and link it into the Swift `FSUnaryFileSystem` extension target.
2. Embed the extension in the host app bundle (`Contents/Library/SystemExtensions`).
3. Sign **inside-out** with a hardened runtime and the entitlements above:
   ```sh
   codesign --force --options runtime --timestamp \
     --entitlements git-lazy-mount.entitlements \
     --sign "Developer ID Application: <TEAM>" \
     "<App>.app/Contents/Library/SystemExtensions/com.git-lazy-mount.fskit.fsmodule.appex"
   codesign --force --options runtime --timestamp \
     --sign "Developer ID Application: <TEAM>" "<App>.app"
   ```
4. Notarize the app (`notarytool submit … --wait`) and staple.
5. Verify: `codesign --verify --deep --strict --verbose=2 "<App>.app"` and
   `spctl -a -vv "<App>.app"`.

Pin tool versions (Xcode, the macOS SDK) so the build is reproducible.

## Activation / approval lifecycle

The lifecycle states are modeled in [`lifecycle.rs`](../src/lifecycle.rs) and
surfaced by `git lazy-mount doctor` (`fskit_extension_state` + `fskit_next_step`):

| State | Meaning | Next step |
|-------|---------|-----------|
| `unsupported` | not macOS, or macOS < 15.4 | upgrade macOS, or use macFUSE |
| `not_installed` | OS supports FSKit; our extension isn't registered | install the app bundle |
| `awaiting_approval` | registered, not yet approved | approve in System Settings → General → Login Items & Extensions → File System Extensions |
| `activated` | approved & active | ready to mount |

* **Activation:** the host app calls `OSSystemExtensionRequest.activationRequest`;
  the user approves it once in System Settings.
* **Deactivation:** `OSSystemExtensionRequest.deactivationRequest`.
* **Across OS updates / reloads:** a major macOS update can require re-approval;
  the probe (`Capability::detect`) re-reads the live state each run, so `doctor`
  always reflects reality rather than a cached assumption.

## On-device acceptance (issue #10)

- [ ] A reproducible **signed** build with the entitlements above.
- [ ] Activation / deactivation + the user-approval flow, surfaced via
      `doctor` diagnostics (the lifecycle state ties into capability detection,
      issue #6).
- [ ] Verified behavior across an OS update / extension reload.
