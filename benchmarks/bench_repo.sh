#!/usr/bin/env bash
# Runs INSIDE the glm-bench container (as user ubuntu). Benchmarks ONE repo in
# both modes (full clone vs git lazy-mount): real claude (Sonnet) answers the
# question via sgrep, makes a small code edit, commits locally, and optionally
# pushes to the fork when GH_TOKEN is configured.
# Args: REPO_KEY  FORK(owner/name)  UPSTREAM(owner/name)  DEFAULT_BRANCH  "QUESTION"
set -uo pipefail
REPO_KEY="$1"; CLONE="$2"; FORK="$3"; UPSTREAM="$4"; DBRANCH="$5"; QUESTION="$6"
export HOME=/home/ubuntu
export SGREP_BROAD_TIMEOUT_SECS="${SGREP_BROAD_TIMEOUT_SECS:-12}"
OUT=/out
mkdir -p "$OUT"
now(){ date +%s.%N; }
secs(){ python3 -c "print(round($2-$1,1))"; }
log(){ echo "[$(date +%H:%M:%S)] [$REPO_KEY] $*"; }

git config --global user.name  "glm-bench"
git config --global user.email "glm-bench@users.noreply.github.com"
git config --global --add safe.directory '*'
if [ -n "${GH_TOKEN:-}" ]; then
  git config --global credential.helper "!f(){ echo username=x-access-token; echo password=${GH_TOKEN}; };f"
fi

# sgrep -> query the UPSTREAM index; wrapper calls the real binary by ABSOLUTE path.
mkdir -p "$HOME/bin"
cat > "$HOME/bin/sgrep" <<EOF
#!/usr/bin/env bash
has_file=0
prev=""
for arg in "\$@"; do
  if [ "\$prev" = "--file" ] || [ "\$arg" = "--file" ] || [[ "\$arg" == --file=* ]]; then
    has_file=1
  fi
  prev="\$arg"
done
limit="\${SGREP_WALL_TIMEOUT_SECS:-}"
if [ -z "\$limit" ] && [ -n "\${SGREP_TIMEOUT_SECS:-}" ]; then
  limit="\$SGREP_TIMEOUT_SECS"
fi
if [ -z "\$limit" ] && [ "\$has_file" -eq 0 ] && [ -n "\${SGREP_BROAD_TIMEOUT_SECS:-}" ]; then
  limit="\$SGREP_BROAD_TIMEOUT_SECS"
fi
if [ -n "\$limit" ] && [ "\$limit" != "0" ]; then
  timeout --kill-after=2 "\$limit" /usr/local/bin/sgrep --repo "$UPSTREAM" "\$@"
  rc=\$?
  if [ "\$rc" -eq 124 ] || [ "\$rc" -eq 137 ]; then
    echo "sgrep: timed out after \${limit}s; use the seed hints, Read a plausible hit, or narrow with --file" >&2
  fi
  exit "\$rc"
fi
exec /usr/local/bin/sgrep --repo "$UPSTREAM" "\$@"
EOF
chmod +x "$HOME/bin/sgrep"
export PATH="$HOME/bin:$PATH"

CLAUDE_MD='# Searching this repository

This is a lazily-materialized working tree. Do NOT use grep, ripgrep (rg), `git grep`, or `find` for content search — they read every file and defeat lazy mounting.

Use `sgrep --count 20 <regex>` for ALL code search: it queries a cloud code index and returns matching files+lines without reading local files, and reflects your uncommitted edits.

Use `--file` when the likely extension or directory is obvious (for example `.ts`, `.rs`, `.go`, `.dart`, `.cpp`, `lib/`, `src/`). Unfiltered searches have a short timeout; if one times out or returns irrelevant matches, rerun with `--file` and a narrower literal. Do not use `| head` as a substitute for `--count`; it still makes the remote search do extra work.
Examples: `sgrep --count 20 useState --file "\.(js|jsx|ts|tsx)$"`   `sgrep --count 20 -l --file "\.ts$" "class \w+"`'

ALLOW=(--allowedTools "Read" "Glob" "Edit" "Write" "Bash(git:*)" "Bash(sgrep:*)" "Bash(ls:*)" "Bash(cat:*)" "Bash(head:*)" "Bash(sed:*)" "Bash(mkdir:*)"
       --disallowedTools "Grep" "Bash(rg:*)" "Bash(grep:*)" "Bash(find:*)")

seed_searches(){
  local pfx="$1" cache="$OUT/${pfx}.sgrep-cache" seed="$OUT/${pfx}.seed.md"
  local seed_timeout="${BENCH_SEED_TIMEOUT_SECS:-30}"
  : > "$seed"
  [ "${BENCH_SEED_SEARCH:-1}" = "1" ] || return 0
  python3 - "$QUESTION" <<'PY' | while IFS= read -r term; do
import re, sys
seen = set()
for raw in re.findall(r"`([^`]+)`", sys.argv[1]):
    term = raw.strip()
    if not term or len(term) > 80 or term in seen:
        continue
    seen.add(term)
    print(term)
PY
    [ -n "$term" ] || continue
    {
      printf '### `%s`\n' "$term"
      out="$(SGREP_CACHE_DIR="$cache" timeout --kill-after=2 "$seed_timeout" \
        /usr/local/bin/sgrep --repo "$UPSTREAM" --literal --count 12 --no-overlay \
        --file '\.(rs|ts|tsx|js|jsx|go|py|c|cc|cpp|h|hpp|dart)$' "$term" 2>&1)"
      rc=$?
      printf '%s\n' "$out" | sed -n '1,14p'
      if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
        printf 'seed search timed out after %ss; continue with a narrower --file search if needed\n' "$seed_timeout"
      elif [ "$rc" -ne 0 ]; then
        printf 'seed search exited rc=%s\n' "$rc"
      fi
      printf '\n'
    } >> "$seed"
  done
}

run_agent(){ # dir branch transcript_prefix
  local dir="$1" branch="$2" pfx="$3"
  printf '%s\n' "$CLAUDE_MD" > "$dir/CLAUDE.md"
  rm -rf "$OUT/${pfx}.sgrep-cache"
  mkdir -p "$OUT/${pfx}.sgrep-cache"
  seed_searches "$pfx"
  # Push only when a token (and so a fork) is configured; otherwise commit locally.
  local commit_step
  if [ -n "${GH_TOKEN:-}" ]; then
    commit_step="2. Create a branch and push it:
   git checkout -b $branch && git add path/to/the-one-file-you-edited && git commit -m \"glm-bench: note where the answer lives\" && git push -u origin $branch
   Replace path/to/the-one-file-you-edited with the actual edited file. Do not use git add -A."
  else
    commit_step="2. Create a branch and commit it:
   git checkout -b $branch && git add path/to/the-one-file-you-edited && git commit -m \"glm-bench: note where the answer lives\"
   Replace path/to/the-one-file-you-edited with the actual edited file. Do not use git add -A."
  fi
  local seed_block=""
  if [ -s "$OUT/${pfx}.seed.md" ]; then
    seed_block="Initial literal search hints from identifiers in the question (prefer implementation/registration/source files over tests, docs, and call sites):
$(cat "$OUT/${pfx}.seed.md")"
  fi
  local prompt="You are working in a git repository. Answer this question by exploring the code:

  \"$QUESTION\"

$seed_block

To search code, use the \`sgrep --count 20 <regex>\` command (a fast cloud-index search) — NOT grep, rg, or find, and not \`sgrep ... | head\`. First use the exact backticked identifier from the question or the best seed hit; do not guess renamed symbols until you have Read the best seed candidate. Use \`--file\` when the likely extension or directory is obvious. Unfiltered searches have a short timeout; if one times out, rerun with \`--file\` and a narrower literal. After one plausible file+line, Read it instead of continuing broad searches. Keep the search phase to at most three \`sgrep\` calls unless the first hits are clearly wrong.

Once you have located the answer:
1. Make ONE small, real code edit at the exact file+location you identified: add a single clarifying comment line (one or two lines, in ONE file) that summarizes the finding. Do not change behavior.
$commit_step
Work autonomously and concisely. End by printing one line: ANSWER: <file:line — short summary>."
  ( cd "$dir" && SGREP_CACHE_DIR="$OUT/${pfx}.sgrep-cache" timeout 1200 claude --bare --model sonnet "${ALLOW[@]}" \
        --output-format stream-json --verbose -p "$prompt" 2> "$OUT/${pfx}.claude.err" \
      | python3 -u /bench/ts_prepend.py ) > "$OUT/${pfx}.transcript.tsv"
}
dub(){ du -sb "$1" 2>/dev/null | cut -f1; }
mib(){ python3 -c "print(round(${1:-0}/1048576,1))"; }

############################  FULL CLONE  ############################
log "FULL: git clone https://github.com/$CLONE (push->$FORK)"
FDIR=/work/full
T=$(now); git clone "https://github.com/$CLONE" "$FDIR" > "$OUT/full.clone.log" 2>&1; full_clone_s=$(secs $T $(now))
push_url="https://github.com/$FORK"
git -C "$FDIR" remote set-url --push origin "$push_url"
full_files=$(cd "$FDIR" && git ls-files | wc -l | tr -d ' ')
full_worktree_b=$(du -sb --exclude=.git "$FDIR" | cut -f1)
full_dotgit_b=$(dub "$FDIR/.git")
log "FULL: agent (files=$full_files worktree=$(mib $full_worktree_b)MiB)"
T=$(now); run_agent "$FDIR" "glm-bench-full" "full"; full_agent_s=$(secs $T $(now))
full_remote_sha=$(git -C "$FDIR" ls-remote --heads "$push_url" glm-bench-full 2>/dev/null | cut -c1-7)
log "FULL: done agent_s=$full_agent_s pushed=${full_remote_sha:-NONE}"
rm -rf "$FDIR"

############################  LAZY MOUNT  ############################
log "LAZY: git lazy-mount https://github.com/$CLONE (push->$FORK)"
LDIR=/work/lazy
T=$(now); git lazy-mount "https://github.com/$CLONE" "$LDIR" > "$OUT/lazy.mount.log" 2>&1; lazy_mount_s=$(secs $T $(now))
git -C "$LDIR" remote set-url --push origin "$push_url"
WS=$(ls -dt /home/ubuntu/.local/share/git-lazy-mount/workspaces/*/ 2>/dev/null | head -1)
lazy_files=$(cd "$LDIR" && git ls-files | wc -l | tr -d ' ')
lazy_initial_b=$(dub "$WS")
log "LAZY: agent (files=$lazy_files initial=$(mib $lazy_initial_b)MiB ws=$WS)"
T=$(now); run_agent "$LDIR" "glm-bench-lazy" "lazy"; lazy_agent_s=$(secs $T $(now))
lazy_final_b=$(dub "$WS"); lazy_cache_b=$(dub "$WS/cache"); lazy_git_b=$(dub "$WS/git"); lazy_overlay_b=$(dub "$WS/overlay")
lazy_remote_sha=$(git -C "$LDIR" ls-remote --heads "$push_url" glm-bench-lazy 2>/dev/null | cut -c1-7)
log "LAZY: done agent_s=$lazy_agent_s final=$(mib $lazy_final_b)MiB pushed=${lazy_remote_sha:-NONE}"
fusermount3 -u "$LDIR" 2>/dev/null || true

############################  METRICS  ############################
python3 - <<PYEOF > "$OUT/metrics.json"
import json
def i(x):
  try: return int(x)
  except: return 0
def f(x):
  try: return float(x)
  except: return 0.0
print(json.dumps({
 "repo":"$REPO_KEY","clone":"$CLONE","fork":"$FORK","upstream":"$UPSTREAM","default_branch":"$DBRANCH","question":"""$QUESTION""",
 "files": i("$full_files"),
 "full": {"clone_s": f("$full_clone_s"), "agent_s": f("$full_agent_s"),
          "worktree_bytes": i("$full_worktree_b"), "dotgit_bytes": i("$full_dotgit_b"),
          "pushed": "$full_remote_sha"},
 "lazy": {"mount_s": f("$lazy_mount_s"), "agent_s": f("$lazy_agent_s"),
          "initial_bytes": i("$lazy_initial_b"), "final_bytes": i("$lazy_final_b"),
          "git_bytes": i("$lazy_git_b"), "cache_bytes": i("$lazy_cache_b"), "overlay_bytes": i("$lazy_overlay_b"),
          "pushed": "$lazy_remote_sha"},
}, indent=2))
PYEOF
cat "$OUT/metrics.json"
log "ALL DONE"
