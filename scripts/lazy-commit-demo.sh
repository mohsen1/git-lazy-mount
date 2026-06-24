#!/usr/bin/env bash
#
# Demo + smoke test: make a commit on a *huge* repository (microsoft/TypeScript,
# 80k+ tracked files) through git-lazy-mount — with NO full clone and NO
# working-tree checkout — and show the commit is near-instant and fetches ~no
# objects. This is the core value proposition: VCS/filesystem operations cost
# O(what you touched), not O(repo size).
#
# Env knobs (all optional):
#   GLM_BIN           path to the git-lazy-mount binary (default target/release)
#   GLM_DEMO_REPO     repo URL (default microsoft/TypeScript)
#   GLM_DEMO_BRANCH   branch (default main)
#   GLM_MAX_CLONE_MS  clone time ceiling, ms (default 60000)
#   GLM_MAX_COMMIT_MS commit time ceiling, ms (default 10000)
#
set -euo pipefail

BIN="${GLM_BIN:-target/release/git-lazy-mount}"
REPO="${GLM_DEMO_REPO:-https://github.com/microsoft/TypeScript}"
BRANCH="${GLM_DEMO_BRANCH:-main}"
MAX_CLONE_MS="${GLM_MAX_CLONE_MS:-60000}"
MAX_COMMIT_MS="${GLM_MAX_COMMIT_MS:-10000}"

BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN" ] || { echo "binary not found/executable: $BIN (build with: cargo build --release -p glm-cli)"; exit 2; }

WORK="$(mktemp -d)"
export HOME="$WORK"                 # isolate lazy-mount data roots AND git config
export GIT_TERMINAL_PROMPT=0
git config --global user.email "ci@git-lazy-mount.example"
git config --global user.name  "lazy-mount CI"
trap 'rm -rf "$WORK"' EXIT

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
MNT="$WORK/ts"
fail=0

echo "::group::1. Lazy-clone ${REPO} (${BRANCH}) — blob:none, no checkout"
t=$(now_ms)
"$BIN" clone "$REPO" "$MNT" --filter blob:none --depth 1 --branch "$BRANCH"
clone_ms=$(( $(now_ms) - t ))
echo "→ clone: ${clone_ms} ms"
echo "::endgroup::"

cd "$MNT"
# How big is this repo? Count tracked files through the lazy store (trees are
# present after a blob:none clone, so this is local + fast — no blob downloads).
files="$("$BIN" git -- ls-tree -r --name-only HEAD 2>/dev/null | wc -l | tr -d ' ')"
echo "→ repository tracked files: ${files}"

echo "::group::2. Edit + stage + COMMIT (the headline)"
printf '# touched by lazy-mount CI at %s\n' "$(date -u +%FT%TZ)" | "$BIN" debug write CI_TOUCH.md
"$BIN" add CI_TOUCH.md
t=$(now_ms)
"$BIN" commit -m "lazy-mount: fast commit on a huge repo (CI demo)"
commit_ms=$(( $(now_ms) - t ))
echo "→ commit: ${commit_ms} ms"
"$BIN" git -- log -1 --oneline
echo "::endgroup::"

echo "::group::3. Proof of laziness (object-fetch stats)"
stats="$("$BIN" stats)"
echo "$stats"
fetched="$(printf '%s' "$stats" | grep -oE '"objects_fetched"[[:space:]]*:[[:space:]]*[0-9]+' | grep -oE '[0-9]+$' || echo '?')"
echo "→ objects fetched over the whole session: ${fetched}"
echo "::endgroup::"

echo
echo "==================== SUMMARY ===================="
printf 'repo            : %s (%s files)\n' "$REPO" "$files"
printf 'clone (lazy)    : %6s ms   (ceiling %s)\n' "$clone_ms" "$MAX_CLONE_MS"
printf 'commit          : %6s ms   (ceiling %s)\n' "$commit_ms" "$MAX_COMMIT_MS"
printf 'objects fetched : %s\n' "$fetched"
echo "A full clone+checkout of this repo would download every blob and write"
echo "${files} files to disk; lazy-mount committed without doing either."
echo "================================================="

[ "$clone_ms"  -le "$MAX_CLONE_MS"  ] || { echo "FAIL: clone exceeded ${MAX_CLONE_MS} ms"; fail=1; }
[ "$commit_ms" -le "$MAX_COMMIT_MS" ] || { echo "FAIL: commit exceeded ${MAX_COMMIT_MS} ms"; fail=1; }
[ "${files:-0}" -ge 10000 ] 2>/dev/null || { echo "WARN: expected a huge repo (>=10k files), got ${files}"; }
exit $fail
