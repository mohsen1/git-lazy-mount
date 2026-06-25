# macOS FSKit on-device build & validation (issue #19)

How to build, sign, install, and register the macOS FSKit backend on real Apple
hardware, plus the current OS-level blocker. Validated on macOS 26.4.1
(25E253), Xcode 26.5 SDK, Apple silicon.

## Result summary

The de-risk approach was to validate Apple's official `Passthrough` FSKit sample
on-device before finishing our own. Both Apple's sample and our `GitLazyMount`
extension hit the same wall:

| Step | Apple sample | Our extension |
|------|:---:|:---:|
| Build (Release, automatic signing) | ✅ | ✅ |
| Sign (team + FSKit entitlement) | ✅ | ✅ |
| Install to /Applications + launch | ✅ | ✅ |
| `pluginkit` registration (`com.apple.fskit.fsmodule`) | ✅ | ✅ |
| **Enable** (System Settings → File System Extensions) | ❌ | ❌ |
| `mount -t …` | ❌ | ❌ |

**Blocker: a confirmed upstream Apple bug, not our code and not SIP.** The
System Settings enablement toggle for File System Extensions is inert for
every third-party FSKit module on macOS 26. Clicking it produces zero system
response, so `/Library/Filesystems/<name>.fs` is never created and `mount`
fails with `Module … is disabled!`. This reproduces on Apple's own
`FSKitSample`. The root cause is documented in
[andrewgazelka/loaf#1](https://github.com/andrewgazelka/loaf/issues/1):
`fskitd` rejects the client connection (`Hello FSClient! entitlement no` →
`Failed to start instance … extensionKit Code=2 … RBSRequestErrorDomain Code=5`)
on 26.1 / 26.2 (and our 26.4.1). Others tried the same things we did, and more:
Developer ID signing, notarization, hardened runtime, FSKit entitlements,
library-validation disabled, embedded dylib, manual plist enablement, even an
`fskitd` restart. None of it worked. Apple DTS confirmed (Jul 2025) it awaits
"more fixes."

* **SIP is not the issue.** FSKit is a user-space app-extension model and is
  designed to run with SIP enabled; no SIP change is ever required. The
  `Operation not permitted while System Integrity Protection is engaged` we saw
  was only from `launchctl kickstart` of a system daemon (always SIP-blocked).
  The SIP-compatible restart is `sudo killall fskit`, which still does not fix
  this bug.
* **A reboot will not fix it either.** An `fskitd` restart was already tried and
  reproduces on Apple's sample. Re-validate when Apple ships an FSKit fix.

## Signing runbook (the hard-won part)

No SIP changes, no notarization, no Developer ID, no `OSSystemExtension`. Just a
plain app extension provisioned by Xcode automatic signing. The blockers, in the
order they bite:

1. **Program License Agreement.** Automatic signing fails with *"Unable to
   process request – PLA Update available"* until the account holder accepts the
   updated agreement at <https://developer.apple.com/account>.
2. **The team ID is the cert's `OU`, not the name's parenthetical.** A keychain
   identity `Apple Development: … (698SEVL7YQ)` has `OU=HESNS6JK33`, and
   `HESNS6JK33` is the team (confirmed by the Xcode account `teamID` and the
   embedded `.provisionprofile`). Passing the wrong team gives you *"No Account for
   Team"*.
3. **CLI `xcodebuild` works with the correct team** (`-allowProvisioningUpdates`).
   No GUI build required once the team/PLA/cert are right. A wrong team is
   what produced the misleading "No Account" errors.
4. **Duplicate "Apple Development" certs** make Xcode sign the app and the appex
   with different identities, which gives you *"Embedded binary is not signed with the same
   certificate as the parent app."* Delete the orphaned one
   (`security delete-identity -Z <sha1>`).
5. **Apple's sample ships an ad-hoc override** on the app target only
   (`"CODE_SIGN_IDENTITY[sdk=macosx*]" = "-"`); remove it so app + appex both
   real-sign.
6. **Build Release**, not Debug. The Debug "debug dylib" indirection
   (`*.debug.dylib`, `__preview.dylib`) is a poor fit for a system-validated
   extension.

### Working invocation
```sh
xcodebuild -project GitLazyMount.xcodeproj -scheme GitLazyMount -configuration Release \
  -derivedDataPath ./DerivedData DEVELOPMENT_TEAM=HESNS6JK33 -allowProvisioningUpdates build
cp -R DerivedData/Build/Products/Release/GitLazyMount.app /Applications/
open /Applications/GitLazyMount.app
pluginkit -mAv -p com.apple.fskit.fsmodule | grep gitlazymount   # → "+ …GitLazyMountFS"
```
(`crates/fs-fskit/extension/build.sh` automates the Rust staticlib + xcodegen +
this build.)

## Architecture note: sandbox vs. `git`

FSKit extensions are sandboxed (`com.apple.security.app-sandbox` is mandatory).
The current `glm-fskit-ffi` opens the workspace and runs `git` in-process,
which the sandbox restricts. Production should proxy FSKit callbacks to
`glm-daemon` over XPC/IPC so `git` runs outside the sandbox. The engine split
(`FskitOps` vs. daemon) already supports this. The in-process FFI is the
validation shim that let us prove build/sign/registration end-to-end.
