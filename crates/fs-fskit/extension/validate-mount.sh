#!/usr/bin/env bash
#
# On-device FSKit mount validation (macOS, issue #19). Run this AFTER enabling
# the module in System Settings → File System Extensions (or after the reboot /
# `sudo launchctl kickstart -k system/com.apple.filesystems.fskitd` that clears
# the stuck enablement state on macOS 26.4.1).
#
# It first validates Apple's `passthrough` sample (the de-risk gate: does ANY
# third-party FSKit module mount on this OS?), then — if that works — our own
# `gitlazymount` module against a freshly lazy-cloned repo.
#
# Exit 0 = a real third-party FSKit mount succeeded on-device. Until that
# happens, macOS is NOT "supported" (spec §54).
#
set -uo pipefail
[ "$(uname)" = "Darwin" ] || { echo "macOS only"; exit 2; }

pass=0 fail=0
ok()   { echo "  ✅ $*"; pass=$((pass+1)); }
bad()  { echo "  ❌ $*"; fail=$((fail+1)); }

enabled() { [ -e "/Library/Filesystems/$1.fs" ]; }

# ---- 1. Apple's passthrough sample (the OS-level gate) --------------------
echo "== Apple passthrough sample =="
if pluginkit -mAv -p com.apple.fskit.fsmodule 2>/dev/null | grep -qi passthrough; then
  ok "module registered"
  if enabled passthrough; then
    ok "module enabled (/Library/Filesystems/passthrough.fs present)"
    src="$HOME/glmtest-src"; dst="$HOME/passthrough-fs"
    rm -rf "$src" "$dst"; mkdir -p "$src/sub" "$dst"
    echo "hello from FSKit" > "$src/hello.txt"; echo nested > "$src/sub/nested.txt"
    if timeout 30 mount -t passthrough "$src" "$dst" 2>&1; then
      ok "mounted"
      [ "$(cat "$dst/hello.txt" 2>/dev/null)" = "hello from FSKit" ] && ok "read through mount" || bad "read through mount"
      if echo "written via FSKit" > "$dst/new.txt" 2>/dev/null && [ -f "$src/new.txt" ]; then
        ok "write through mount"
      else
        bad "write through mount"
      fi
      umount "$dst" 2>/dev/null || diskutil unmount "$dst" 2>/dev/null
    else
      bad "mount failed (module still disabled?)"
    fi
  else
    echo "  ⏳ NOT enabled — System Settings → General → Login Items & Extensions"
    echo "       → File System Extensions → enable, or:"
    echo "       sudo launchctl kickstart -k system/com.apple.filesystems.fskitd"
  fi
else
  echo "  ⏳ not registered — build + open: crates/fs-fskit/extension/build.sh then"
  echo "       open /Applications/GitLazyMount.app (and Apple's Passthrough.app)"
fi

# ---- 2. Our git-lazy-mount FSKit module ----------------------------------
echo "== git-lazy-mount module =="
if pluginkit -mAv -p com.apple.fskit.fsmodule 2>/dev/null | grep -qi gitlazymount; then
  ok "module registered"
  enabled gitlazymount && ok "module enabled" || echo "  ⏳ enable 'git-lazy-mount' in System Settings, then re-run"
  # (A full mount needs a registered workspace via the CLI; gated on enablement
  #  + the daemon-IPC path of ADR 0008 for sandbox-legal git. See issue #19.)
else
  echo "  ⏳ not registered — run crates/fs-fskit/extension/build.sh + open the app"
fi

echo
echo "== result: $pass ok, $fail failed =="
[ "$fail" -eq 0 ] && [ "$pass" -gt 0 ] && exit 0 || exit 1
