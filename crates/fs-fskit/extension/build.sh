#!/usr/bin/env bash
#
# Build the git-lazy-mount FSKit module: the Rust FFI static library + the Swift
# host app/extension, signed with the developer team. Produces a signed .app
# under ./DerivedData ready to install + register.
#
# Prereqs: Rust (with the aarch64-apple-darwin target), xcodegen, Xcode, and a
# paid Apple developer team signed into Xcode (see ../../../docs for the runbook).
#
set -euo pipefail
cd "$(dirname "$0")"

REPO_ROOT="$(cd ../../.. && pwd)"
TEAM="${DEVELOPMENT_TEAM:-HESNS6JK33}"
RUST_TARGET="aarch64-apple-darwin"

echo "==> 1/3  Rust FFI static library ($RUST_TARGET, release)"
( cd "$REPO_ROOT" && cargo build -p glm-fskit-ffi --release --target "$RUST_TARGET" )
ls -la "$REPO_ROOT/target/$RUST_TARGET/release/libglm_fskit_ffi.a"

echo "==> 2/3  generate Xcode project (xcodegen)"
xcodegen generate

echo "==> 3/3  build + sign (Release, team $TEAM)"
xcodebuild -project GitLazyMount.xcodeproj -scheme GitLazyMount -configuration Release \
  -derivedDataPath ./DerivedData \
  DEVELOPMENT_TEAM="$TEAM" -allowProvisioningUpdates build

APP="./DerivedData/Build/Products/Release/GitLazyMount.app"
echo "==> built: $APP"
echo "    install:  cp -R '$APP' /Applications/ && open /Applications/GitLazyMount.app"
echo "    enable:   System Settings → General → Login Items & Extensions → File System Extensions"
echo "    mount:    mount -t gitlazymount <registered-mountpoint> <dir>"
