#!/usr/bin/env bash
#
# End-to-end scenario suite for the git-lazy-mount native workflow against a real
# (typically huge) upstream repository — no full clone, no working-tree checkout.
# Designed to run across a wide distro × repo × filter matrix in CI.
#
# It drives the CLI exactly as a user would (clone → inspect → edit → stage →
# commit → branch → reset → hydrate → fsck …) and asserts each scenario. Every
# scenario runs even if an earlier one fails, so the log shows the full picture;
# the script exits non-zero if any scenario failed.
#
# Env knobs:
#   GLM_BIN          path to the git-lazy-mount binary (default target/release)
#   GLM_DEMO_REPO    repo URL              (default microsoft/TypeScript)
#   GLM_DEMO_BRANCH  branch               (default main)
#   GLM_FILTER       partial-clone filter (default blob:none)
#   GLM_DEPTH        shallow depth        (default 1; empty = full history)
#   GLM_MAX_COMMIT_S commit ceiling, secs (default 30)
#
set -u

BIN="${GLM_BIN:-target/release/git-lazy-mount}"
REPO="${GLM_DEMO_REPO:-https://github.com/microsoft/TypeScript}"
# Empty branch = let the clone auto-detect the remote's default branch.
BRANCH="${GLM_DEMO_BRANCH:-}"
FILTER="${GLM_FILTER:-blob:none}"
DEPTH="${GLM_DEPTH:-1}"
MAX_COMMIT_S="${GLM_MAX_COMMIT_S:-30}"

BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -x "$BIN" ] || { echo "FATAL: binary not found/executable: $BIN"; exit 2; }
command -v git >/dev/null || { echo "FATAL: git not installed"; exit 2; }

WORK="$(mktemp -d)"
export HOME="$WORK"                 # isolate lazy-mount data roots AND git config
export GIT_TERMINAL_PROMPT=0
git config --global user.email "e2e@git-lazy-mount.example"
git config --global user.name  "lazy-mount e2e"
git config --global init.defaultBranch main
git config --global protocol.version 2 >/dev/null 2>&1 || true
trap 'rm -rf "$WORK"' EXIT

MNT="$WORK/repo"
pass=0; fail=0; skip=0
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; fail=$((fail+1)); }
note() { printf '  \033[33mSKIP\033[0m %s\n' "$*"; skip=$((skip+1)); }
group(){ printf '\n=== %s ===\n' "$*"; }
# assert: <description> <command...>  (passes if the command exits 0; on
# failure, surfaces the command's stderr to make CI logs actionable)
assert(){ local d="$1"; shift; local err; if err="$("$@" 2>&1 >/dev/null)"; then ok "$d"; else bad "$d → ${err:-(no stderr)}"; fi; }

glm(){ "$BIN" "$@"; }
porcelain(){ "$BIN" git -- status --porcelain 2>/dev/null; }

echo "repo=$REPO branch=$BRANCH filter=$FILTER depth=${DEPTH:-full}"
echo "git=$(git --version)  bin=$BIN"

# ----------------------------------------------------------------------------
group "1. clone (lazy: $FILTER, no checkout)"
depth_args=(); [ -n "$DEPTH" ] && depth_args=(--depth "$DEPTH")
branch_args=(); [ -n "$BRANCH" ] && branch_args=(--branch "$BRANCH")
t=$(date +%s)
if glm clone "$REPO" "$MNT" --filter "$FILTER" "${depth_args[@]}" "${branch_args[@]}" 2>&1 | tee "$WORK/clone.log" | tail -2; then
  if grep -qi "no files were checked out" "$WORK/clone.log"; then ok "clone reports no checkout"; else bad "clone missing no-checkout notice"; fi
else
  bad "clone failed"; echo "SUMMARY repo=$REPO PASS=$pass FAIL=$fail (clone failed, aborting)"; exit 1
fi
clone_s=$(( $(date +%s) - t )); echo "  (clone: ${clone_s}s)"
cd "$MNT" || { echo "FATAL cd"; exit 2; }

# Pick concrete paths from the real tree (trees are present after a blob:none
# clone). Use sed line-picks (portable; no bash-4 `mapfile`).
tree_files="$("$BIN" git -- ls-tree -r --name-only HEAD 2>/dev/null)"
nfiles=$(printf '%s\n' "$tree_files" | grep -c . )
FILE1=$(printf '%s\n' "$tree_files" | sed -n '1p')
FILE2=$(printf '%s\n' "$tree_files" | sed -n '2p')
FILE3=$(printf '%s\n' "$tree_files" | sed -n '3p')
TOPDIR=$("$BIN" git -- ls-tree HEAD 2>/dev/null | awk '$2=="tree"{print $4; exit}')
echo "  tracked files: ${nfiles}; sample: ${FILE1}"

# ----------------------------------------------------------------------------
group "2. read-only projection (lazy hydration on access)"
assert "ls root"            bash -c '"$0" ls | grep -q .' "$BIN"
[ -n "$TOPDIR" ] && assert "ls subdir ($TOPDIR)" bash -c '"$0" ls "$1" | grep -q .' "$BIN" "$TOPDIR" || note "no subdir to ls"
if [ -n "$FILE1" ]; then
  if [ -n "$("$BIN" cat "$FILE1" 2>/dev/null)" ]; then ok "cat hydrates a real blob ($FILE1)"; else bad "cat returned empty for $FILE1"; fi
else bad "no file to cat"; fi
# (Resolve HEAD through the interop bridge. `git -- log` traversal can hit the
# shallow boundary on the freshly-cloned base, so use rev-parse for the tip OID;
# `git -- log` is exercised after the first workspace commit below.)
[ -n "$("$BIN" git -- rev-parse HEAD 2>/dev/null)" ] && ok "git -- rev-parse HEAD resolves the tip" || bad "git -- rev-parse HEAD empty"
[ -z "$(porcelain)" ] && ok "fresh workspace is clean" || bad "fresh workspace not clean"

# ----------------------------------------------------------------------------
group "3. add a new file → stage → commit (the headline: O(change), not O(repo))"
echo "e2e new file $(date -u +%FT%TZ)" | glm debug write E2E_NEW.md >/dev/null 2>&1
glm add E2E_NEW.md >/dev/null 2>&1
porcelain | grep -q "E2E_NEW.md" && ok "new file shows staged" || bad "new file not staged"
t=$(date +%s)
if glm commit -m "e2e: add a new file" >/dev/null 2>&1; then
  commit_s=$(( $(date +%s) - t ))
  [ -z "$(porcelain)" ] && ok "clean after commit" || bad "not clean after commit"
  "$BIN" git -- log -1 --pretty=%s 2>/dev/null | grep -q "e2e: add a new file" && ok "commit is at HEAD" || bad "commit not at HEAD"
  [ "$commit_s" -le "$MAX_COMMIT_S" ] && ok "commit fast (${commit_s}s ≤ ${MAX_COMMIT_S}s)" || bad "commit too slow (${commit_s}s)"
else bad "commit failed"; fi

# ----------------------------------------------------------------------------
group "4. modify an existing (lazy) file → commit"
if [ -n "$FILE1" ]; then
  printf 'modified by e2e\n' | glm debug write "$FILE1" >/dev/null 2>&1
  # Unstaged overlay edits surface through the native diff (the interop
  # `git status --porcelain` reflects the *stage*, i.e. staged-vs-HEAD).
  [ -n "$("$BIN" diff 2>/dev/null)" ] && ok "working modification detected (glm diff)" || bad "modification not detected"
  glm add "$FILE1" >/dev/null 2>&1
  porcelain | grep -q . && ok "modification shows staged after add" || bad "staged modification missing"
  assert "commit the modification" glm commit -m "e2e: modify $FILE1"
  [ "$("$BIN" cat "$FILE1" 2>/dev/null)" = "modified by e2e" ] && ok "modified content reads back" || bad "modified content mismatch"
else note "no file to modify"; fi

# ----------------------------------------------------------------------------
group "5. delete + rename"
if [ -n "$FILE2" ]; then
  glm debug rm "$FILE2" >/dev/null 2>&1
  glm add -A >/dev/null 2>&1
  assert "commit a deletion" glm commit -m "e2e: delete $FILE2"
  "$BIN" git -- ls-tree -r --name-only HEAD 2>/dev/null | grep -qx "$FILE2" && bad "deleted file still tracked" || ok "deleted file gone from tree"
else note "no file to delete"; fi
if [ -n "$FILE3" ]; then
  glm debug mv "$FILE3" "${FILE3}.renamed" >/dev/null 2>&1
  glm add -A >/dev/null 2>&1
  assert "commit a rename" glm commit -m "e2e: rename $FILE3"
  "$BIN" git -- ls-tree -r --name-only HEAD 2>/dev/null | grep -qx "${FILE3}.renamed" && ok "renamed path present" || bad "renamed path missing"
else note "no file to rename"; fi

# ----------------------------------------------------------------------------
group "6. branch / diff / restore / reset (smoke)"
assert "branch lists"  bash -c '"$0" branch | grep -q .' "$BIN"
if [ -n "$FILE1" ]; then
  printf 'dirty\n' | glm debug write "$FILE1" >/dev/null 2>&1
  "$BIN" diff 2>/dev/null | grep -q . && ok "diff shows a working change" || bad "diff empty after edit"
  glm restore "$FILE1" >/dev/null 2>&1
  [ -z "$(porcelain)" ] && ok "restore drops the overlay edit" || bad "restore left changes"
fi
assert "reset --soft HEAD"  glm reset --soft HEAD
assert "reset --mixed HEAD" glm reset --mixed HEAD

# ----------------------------------------------------------------------------
group "7. hydration metrics + integrity + diagnostics"
[ -n "$FILE1" ] && assert "hydrate a path" glm hydrate "$FILE1"
stats="$("$BIN" stats 2>/dev/null)"
printf '%s' "$stats" | grep -q '"objects_fetched"' && ok "stats expose object-fetch counters" || bad "stats missing counters"
assert "fsck (workspace consistency)" glm fsck
assert "doctor (environment health)"  glm doctor

# ----------------------------------------------------------------------------
echo
echo "==================== SUMMARY ===================="
printf 'repo   : %s (%s files)\n' "$REPO" "$nfiles"
printf 'config : filter=%s depth=%s  on %s\n' "$FILTER" "${DEPTH:-full}" "$(uname -sr)"
printf 'result : %s passed, %s failed, %s skipped\n' "$pass" "$fail" "$skip"
echo "================================================="
[ "$fail" -eq 0 ]
