#!/usr/bin/env bash
# Runs INSIDE the glm-bench container (as user ubuntu). Benchmarks ONE repo in
# both modes (full clone vs git lazy-mount): real claude (Sonnet) answers the
# question via sgrep, makes a small code edit, commits, and pushes to the fork.
# Args: REPO_KEY  FORK(owner/name)  UPSTREAM(owner/name)  DEFAULT_BRANCH  "QUESTION"
set -uo pipefail
REPO_KEY="$1"; CLONE="$2"; FORK="$3"; UPSTREAM="$4"; DBRANCH="$5"; QUESTION="$6"
export HOME=/home/ubuntu
OUT=/out
mkdir -p "$OUT"
now(){ date +%s.%N; }
secs(){ python3 -c "print(round($2-$1,1))"; }
log(){ echo "[$(date +%H:%M:%S)] [$REPO_KEY] $*"; }

git config --global user.name  "glm-bench"
git config --global user.email "glm-bench@users.noreply.github.com"
git config --global --add safe.directory '*'
git config --global credential.helper "!f(){ echo username=x-access-token; echo password=${GH_TOKEN}; };f"

# sgrep -> query the UPSTREAM index (the fork is not indexed on Sourcegraph)
if [ -f /usr/local/bin/sgrep ] && [ ! -f /usr/local/bin/sgrep-bin ]; then
  cp /usr/local/bin/sgrep /usr/local/bin/sgrep-bin 2>/dev/null || sudo cp /usr/local/bin/sgrep /usr/local/bin/sgrep-bin 2>/dev/null || true
fi
mkdir -p "$HOME/bin"
cat > "$HOME/bin/sgrep" <<EOF
#!/usr/bin/env bash
exec /usr/local/bin/sgrep-bin --repo "$UPSTREAM" "\$@"
EOF
chmod +x "$HOME/bin/sgrep"
export PATH="$HOME/bin:$PATH"

CLAUDE_MD='# Searching this repository

This is a lazily-materialized working tree. Do NOT use grep, ripgrep (rg), `git grep`, or `find` for content search — they read every file and defeat lazy mounting.

Use `sgrep <regex>` for ALL code search: it queries a cloud code index and returns matching files+lines without reading local files, and reflects your uncommitted edits.
Examples: `sgrep useState`   `sgrep -l --file "\.ts$" "class \w+"`'

ALLOW=(--allowedTools "Read" "Glob" "Edit" "Write" "Bash(git:*)" "Bash(sgrep:*)" "Bash(ls:*)" "Bash(cat:*)" "Bash(head:*)" "Bash(sed:*)" "Bash(mkdir:*)"
       --disallowedTools "Grep" "Bash(rg:*)" "Bash(grep:*)" "Bash(find:*)")

run_agent(){ # dir branch transcript_prefix
  local dir="$1" branch="$2" pfx="$3"
  printf '%s\n' "$CLAUDE_MD" > "$dir/CLAUDE.md"
  local prompt="You are working in a git repository. Answer this question by exploring the code:

  \"$QUESTION\"

To search code, use the \`sgrep <regex>\` command (a fast cloud-index search) — NOT grep, rg, or find. This is a lazily-materialized tree; see CLAUDE.md.

Once you have located the answer:
1. Make ONE small, real code edit at the exact file+location you identified: add a single clarifying comment line (one or two lines, in ONE file) that summarizes the finding. Do not change behavior.
2. Create a branch and push it:
   git checkout -b $branch && git add -A && git commit -m \"glm-bench: note where the answer lives\" && git push -u origin $branch
Work autonomously and concisely. End by printing one line: ANSWER: <file:line — short summary>."
  ( cd "$dir" && timeout 1200 claude --model sonnet "${ALLOW[@]}" \
        --output-format stream-json --verbose -p "$prompt" ) \
        > "$OUT/${pfx}.transcript.jsonl" 2> "$OUT/${pfx}.claude.err"
}
dub(){ du -sb "$1" 2>/dev/null | cut -f1; }
mib(){ python3 -c "print(round(${1:-0}/1048576,1))"; }

############################  FULL CLONE  ############################
log "FULL: git clone https://github.com/$CLONE (push->$FORK)"
FDIR=/work/full
T=$(now); git clone "https://github.com/$CLONE" "$FDIR" > "$OUT/full.clone.log" 2>&1; full_clone_s=$(secs $T $(now))
git -C "$FDIR" remote set-url origin "https://github.com/$FORK"
full_files=$(cd "$FDIR" && git ls-files | wc -l | tr -d ' ')
full_worktree_b=$(du -sb --exclude=.git "$FDIR" | cut -f1)
full_dotgit_b=$(dub "$FDIR/.git")
log "FULL: agent (files=$full_files worktree=$(mib $full_worktree_b)MiB)"
T=$(now); run_agent "$FDIR" "glm-bench-full" "full"; full_agent_s=$(secs $T $(now))
full_remote_sha=$(git -C "$FDIR" ls-remote --heads origin glm-bench-full 2>/dev/null | cut -c1-7)
log "FULL: done agent_s=$full_agent_s pushed=${full_remote_sha:-NONE}"
rm -rf "$FDIR"

############################  LAZY MOUNT  ############################
log "LAZY: git lazy-mount https://github.com/$CLONE (push->$FORK)"
LDIR=/work/lazy
T=$(now); git lazy-mount "https://github.com/$CLONE" "$LDIR" > "$OUT/lazy.mount.log" 2>&1; lazy_mount_s=$(secs $T $(now))
git -C "$LDIR" remote set-url origin "https://github.com/$FORK"
WS=$(ls -dt /home/ubuntu/.local/share/git-lazy-mount/workspaces/*/ 2>/dev/null | head -1)
lazy_files=$(cd "$LDIR" && git ls-files | wc -l | tr -d ' ')
lazy_initial_b=$(dub "$WS")
log "LAZY: agent (files=$lazy_files initial=$(mib $lazy_initial_b)MiB ws=$WS)"
T=$(now); run_agent "$LDIR" "glm-bench-lazy" "lazy"; lazy_agent_s=$(secs $T $(now))
lazy_final_b=$(dub "$WS"); lazy_cache_b=$(dub "$WS/cache"); lazy_git_b=$(dub "$WS/git"); lazy_overlay_b=$(dub "$WS/overlay")
lazy_remote_sha=$(git -C "$LDIR" ls-remote --heads origin glm-bench-lazy 2>/dev/null | cut -c1-7)
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
