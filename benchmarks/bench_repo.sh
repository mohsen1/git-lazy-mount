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
export SGREP_FILTERED_TIMEOUT_SECS="${SGREP_FILTERED_TIMEOUT_SECS:-12}"
export SGREP_FILTERED_REGEX_TIMEOUT_SECS="${SGREP_FILTERED_REGEX_TIMEOUT_SECS:-20}"
OUT="${OUT:-/out}"
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
REAL_GIT="$(command -v git)"
export BENCH_UPSTREAM="$UPSTREAM"
export BENCH_SHARED_SGREP_CACHE="$OUT/sgrep-cache"
mkdir -p "$BENCH_SHARED_SGREP_CACHE"
cat > "$HOME/bin/sgrep" <<'EOF'
#!/usr/bin/env bash
set -uo pipefail
args=("$@")
has_file=0
count_seen=0
count=20
prev=""
pattern=""
skip_value=0
for i in "${!args[@]}"; do
  arg="${args[$i]}"
  if [ "$prev" = "--file" ] || [ "$arg" = "--file" ] || [[ "$arg" == --file=* ]]; then
    has_file=1
  fi
  if [ "$prev" = "--count" ]; then
    count_seen=1
    if [[ "$arg" =~ ^[0-9]+$ ]]; then
      count="$arg"
      if [ "$arg" -gt 20 ]; then
        args[$i]=20
        count=20
      fi
    fi
  elif [[ "$arg" == --count=* ]]; then
    count_seen=1
    value="${arg#--count=}"
    if [[ "$value" =~ ^[0-9]+$ ]]; then
      count="$value"
      if [ "$value" -gt 20 ]; then
        args[$i]="--count=20"
        count=20
      fi
    fi
  fi

  if [ "$skip_value" -eq 1 ]; then
    skip_value=0
  else
    case "$arg" in
      --repo|--rev|--file|--provider|--count|--cache-ttl-secs|--timeout-secs|--broad-timeout-secs|--changed-from)
        skip_value=1
        ;;
      --*) ;;
      *) pattern="$arg" ;;
    esac
  fi
  prev="$arg"
done
if [ "$count_seen" -eq 0 ]; then
  args=(--count 20 "${args[@]}")
fi

regex_intent=0
if [[ "$pattern" == *'\'* || "$pattern" == *'|'* || "$pattern" == *'*'* || "$pattern" == *'+'* || "$pattern" == *'?'* || "$pattern" == *'['* || "$pattern" == *']'* || "$pattern" == *'{'* || "$pattern" == *'}'* || "$pattern" == *'^'* || "$pattern" == *'$'* ]]; then
  regex_intent=1
fi

limit="${SGREP_WALL_TIMEOUT_SECS:-}"
if [ -z "$limit" ] && [ -n "${SGREP_TIMEOUT_SECS:-}" ]; then
  limit="$SGREP_TIMEOUT_SECS"
fi
if [ -z "$limit" ] && [ "$has_file" -eq 0 ] && [ -n "${SGREP_BROAD_TIMEOUT_SECS:-}" ]; then
  limit="$SGREP_BROAD_TIMEOUT_SECS"
fi
if [ -z "$limit" ] && [ "$has_file" -eq 1 ] && [ "$regex_intent" -eq 1 ] && [ -n "${SGREP_FILTERED_REGEX_TIMEOUT_SECS:-}" ]; then
  limit="$SGREP_FILTERED_REGEX_TIMEOUT_SECS"
fi
if [ -z "$limit" ] && [ "$has_file" -eq 1 ] && [ -n "${SGREP_FILTERED_TIMEOUT_SECS:-}" ]; then
  limit="$SGREP_FILTERED_TIMEOUT_SECS"
fi

export SGREP_CACHE_DIR="${SGREP_CACHE_DIR:-$BENCH_SHARED_SGREP_CACHE}"
log="${BENCH_SGREP_LOG:-}"
err="$(mktemp)"
out_tmp="$(mktemp)"
seen_dir="$BENCH_SHARED_SGREP_CACHE/observed"
mkdir -p "$seen_dir"
key="$(python3 - "$BENCH_UPSTREAM" "${args[@]}" <<'PY'
import hashlib
import sys

h = hashlib.sha256()
for arg in sys.argv[1:]:
    h.update(arg.encode("utf-8", "surrogateescape"))
    h.update(b"\0")
print(h.hexdigest())
PY
)"
cache="miss"
start="$(date +%s.%N)"
rc=0
{
  flock 9
  if [ -e "$seen_dir/$key" ]; then
    cache="hit"
  fi
  if [ -n "$limit" ] && [ "$limit" != "0" ]; then
    timeout --kill-after=2 "$limit" /usr/local/bin/sgrep --repo "$BENCH_UPSTREAM" "${args[@]}" > >(tee "$out_tmp") 2> >(tee "$err" >&2)
    rc=$?
  else
    /usr/local/bin/sgrep --repo "$BENCH_UPSTREAM" "${args[@]}" > >(tee "$out_tmp") 2> >(tee "$err" >&2)
    rc=$?
  fi
  if [ "$rc" -ne 124 ] && [ "$rc" -ne 137 ]; then
    : > "$seen_dir/$key"
  fi
} 9>"$BENCH_SHARED_SGREP_CACHE/.lock"
end="$(date +%s.%N)"
dur="$(python3 - "$start" "$end" <<'PY'
import sys
print(round(float(sys.argv[2]) - float(sys.argv[1]), 3))
PY
)"
if [ "$rc" -eq 124 ] || [ "$rc" -eq 137 ]; then
  echo "sgrep: timed out after ${limit}s; read a seed hit, reuse prior hits, or narrow to an exact file/literal" >&2
fi
if [ -n "$log" ]; then
  hits="$(sed -n 's/.*\[sgrep\] \([0-9][0-9]*\) hits.*/\1/p' "$err" | tail -1)"
  if [ -z "$hits" ]; then
    hits="$(awk 'NF { n += 1 } END { print n + 0 }' "$out_tmp")"
  fi
  cmd="$(printf '%q ' "${args[@]}")"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "${BENCH_SGREP_PHASE:-agent}" "$start" "$dur" "$rc" "${limit:-0}" "$count" "$has_file" "${cache:-unknown}" "${hits:-}" "$cmd" >> "$log"
fi
rm -f "$err" "$out_tmp"
exit "$rc"
EOF
chmod +x "$HOME/bin/sgrep"

cat > "$HOME/bin/git" <<EOF
#!/usr/bin/env bash
set -u
case "\${1:-}" in
  grep)
    echo "git grep is disabled in this benchmark; use sgrep so lazy trees are not materialized" >&2
    exit 2
    ;;
  add)
    for arg in "\$@"; do
      case "\$arg" in
        -A|--all|.)
          echo "broad git add is disabled; stage only the one file you edited" >&2
          exit 2
          ;;
      esac
    done
    ;;
esac
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$HOME/bin/git"
export PATH="$HOME/bin:$PATH"

CLAUDE_MD='# Searching this repository

This is a lazily-materialized working tree. Do NOT use grep, ripgrep (rg), `git grep`, or `find` for content search — they read every file and defeat lazy mounting.

Use `sgrep --count 20 <literal-or-narrow-regex>` for ALL code search: it queries a cloud code index and returns matching files+lines without reading local files, and reflects your uncommitted edits.

Use `--file` when the likely extension or directory is obvious (for example `.ts`, `.rs`, `.go`, `.dart`, `.cpp`, `lib/`, `src/`). File filters are regexes or exact paths, not shell globs: use `\.c$`, not `*.c`. Prefer one literal symbol or exact phrase over alternation (`|`) or `.*`; file-filtered searches still have a timeout. Do not run multiple `sgrep` commands at once. Do not use `| head` as a substitute for `--count`; it still makes the remote search do extra work.
Examples: `sgrep --count 20 useState --file "\.(js|jsx|ts|tsx)$"`   `sgrep --count 20 -l --file "\.ts$" "class \w+"`'

ALLOW=(--allowedTools "Read" "Glob" "Edit" "Write" "Bash(git:*)" "Bash(sgrep:*)" "Bash(ls:*)" "Bash(cat:*)" "Bash(head:*)" "Bash(sed:*)" "Bash(mkdir:*)"
       --disallowedTools "Grep" "Bash(rg:*)" "Bash(grep:*)" "Bash(find:*)")

seed_searches(){
  local pfx="$1" cache="$OUT/sgrep-cache" seed="$OUT/${pfx}.seed.md" log="$OUT/${pfx}.sgrep.tsv"
  local seed_timeout="${BENCH_SEED_TIMEOUT_SECS:-12}"
  : > "$seed"
  [ "${BENCH_SEED_SEARCH:-1}" = "1" ] || return 0
  python3 - "$QUESTION" <<'PY' | while IFS= read -r term; do
import re, sys
question = sys.argv[1]
question_lc = question.lower()
method_question = "method" in question_lc
seen = set()
variant_seen = set()
stop = {
    "where", "what", "does", "the", "is", "in", "a", "an", "of", "to",
    "return", "returns", "implemented", "defined", "registered", "entry",
    "point", "command", "method", "object", "lifecycle", "standard", "library",
    "resolve", "resolves", "its",
}
terms = []
variants = []
suppress_word_pairs = False
def add(term):
    term = term.strip("`'\" ")
    if not term or len(term) > 80 or term in seen:
        return
    seen.add(term)
    terms.append(term)
def add_variant(term):
    term = term.strip("`'\" ")
    if not term or len(term) > 80 or term in seen or term in variant_seen:
        return
    variant_seen.add(term)
    variants.append(term)
def add_definition_variants(term):
    if re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", term) and not term.isupper():
        if method_question:
            add_variant("void " + term)
        if "function" in question_lc:
            add_variant("function " + term)
for raw in re.findall(r"`([^`]+)`", sys.argv[1]):
    raw = raw.strip()
    raw_is_simple = re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", raw)
    hook = re.fullmatch(r"use([A-Z][A-Za-z0-9_$]*)", raw)
    if hook:
        suppress_word_pairs = True
    skip_raw = bool(
        (
            method_question
            and raw_is_simple
            and raw[:1].islower()
            and any(ch.isupper() for ch in raw[1:])
        )
        or hook
    )
    if not skip_raw:
        add(raw)
    if re.fullmatch(r"[A-Z][A-Z0-9_]*", raw) and "command" in question_lc:
        command_name = raw.lower() + "Command"
        add_variant(command_name)
    if hook:
        add_variant("mount" + hook.group(1) + "Impl")
    for part in re.findall(r"[A-Za-z_$][A-Za-z0-9_$]*(?:[._-][A-Za-z_$][A-Za-z0-9_$]*)*|__[A-Za-z0-9_]+__", raw):
        if not (skip_raw and part == raw):
            add(part)
        add_definition_variants(part)
for token in re.findall(r"\b[A-Za-z][A-Za-z0-9_]*(?:[-._][A-Za-z0-9_]+)+\b|\b[A-Z][A-Za-z0-9_]{2,}\b|\b[A-Z0-9_]{3,}\b|\b[A-Za-z_][A-Za-z0-9_]*\(\)", question):
    add(token)
if not suppress_word_pairs:
    words = [w.lower() for w in re.findall(r"[A-Za-z][A-Za-z0-9]+", question)]
    for a, b in zip(words, words[1:]):
        if a in stop or b in stop:
            continue
        add(a + b[:1].upper() + b[1:])
emitted = set()
for term in terms + variants:
    if term in emitted:
        continue
    emitted.add(term)
    print(term)
    if len(emitted) >= 6:
        break
PY
    [ -n "$term" ] || continue
    {
      printf '### `%s`\n' "$term"
      start="$(now)"
      err="$(mktemp)"
      seed_file_filter='\.(rs|ts|tsx|js|jsx|go|py|c|cc|cpp|cxx|h|hpp|hh|m|mm|inc|td|dart|java|kt|kts|swift|gd|cs|scala|py)$'
      case "$term" in
        mount*Impl) seed_file_filter='ReactFiberHooks' ;;
      esac
      out="$(SGREP_CACHE_DIR="$cache" BENCH_SGREP_LOG="$log" BENCH_SGREP_PHASE=seed SGREP_WALL_TIMEOUT_SECS="$seed_timeout" \
        sgrep --literal --count 20 --no-overlay \
        --file "$seed_file_filter" "$term" 2> >(tee "$err" >&2))"
      rc=$?
      dur="$(secs "$start" "$(now)")"
      cache_status="$(tail -n 1 "$log" | awk -F '\t' '{print $8}')"
      hits="$(tail -n 1 "$log" | awk -F '\t' '{print $9}')"
      printf 'seed: duration=%ss rc=%s cache=%s hits=%s\n' "$dur" "$rc" "${cache_status:-unknown}" "${hits:-}"
      printf '%s\n' "$out" | sed -n '1,10p'
      rm -f "$err"
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
  local agent_cwd="/work/agent-$pfx"
  mkdir -p "$agent_cwd"
  printf '%s\n' "$CLAUDE_MD" > "$dir/CLAUDE.md"
  printf '%s\n' "$CLAUDE_MD" > "$agent_cwd/CLAUDE.md"
  mkdir -p "$OUT/sgrep-cache"
  : > "$OUT/${pfx}.sgrep.tsv"
  seed_searches "$pfx"
  # Push only when a token (and so a fork) is configured; otherwise commit locally.
  local commit_step
  if [ -n "${GH_TOKEN:-}" ]; then
    commit_step="2. Create a branch and push it:
   git -C $dir checkout -b $branch && git -C $dir add path/to/the-one-file-you-edited && git -C $dir commit -m \"glm-bench: note where the answer lives\" && git -C $dir push -u origin $branch
   Replace path/to/the-one-file-you-edited with the actual edited file relative to $dir. Do not use git add -A."
  else
    commit_step="2. Create a branch and commit it:
   git -C $dir checkout -b $branch && git -C $dir add path/to/the-one-file-you-edited && git -C $dir commit -m \"glm-bench: note where the answer lives\"
   Replace path/to/the-one-file-you-edited with the actual edited file relative to $dir. Do not use git add -A."
  fi
  local seed_block=""
  if [ -s "$OUT/${pfx}.seed.md" ]; then
    seed_block="Initial literal search hints from identifiers and code-like words in the question (prefer implementation/registration/source files over tests, docs, and call sites):
$(cat "$OUT/${pfx}.seed.md")"
  fi
  local prompt="You are working on the git repository rooted at:

  $dir

Use absolute paths under $dir for Read and Edit. Seed hint paths are relative to $dir. For git commands, use \`git -C $dir ...\`.

Answer this question by exploring the code:

  \"$QUESTION\"

$seed_block

If the seed hints include a production source file (not tests/docs/examples/fixtures), your first action must be Read that file around the best line before running any new search. If the seed hints are only tests/docs or are empty, run one narrow literal \`sgrep\` using the exact identifier plus an obvious implementation word from the question.

To search code, use \`sgrep --count 20 <literal-or-narrow-regex>\` — NOT grep, rg, find, git grep, or \`sgrep ... | head\`. Do not run multiple \`sgrep\` commands at once. Prefer one literal symbol or exact phrase; avoid alternation (\`|\`) and \`.*\` unless no literal search is possible. Use \`--file\` when the likely extension or directory is obvious; file filters are regexes or exact paths, not shell globs, so use \`--file '\\.c$'\` rather than \`--file '*.c'\`. If a search times out, do not retry the same shape; Read a seed/prior hit or narrow to one exact file/literal. After one plausible file+line, Read it instead of continuing broad searches. Keep the search phase to at most two new \`sgrep\` calls after the seed hints unless the first hits are clearly wrong.

Once you have located the answer:
1. Make ONE small, real code edit at the exact file+location you identified: add a single clarifying comment line (one or two lines, in ONE file) that summarizes the finding. Do not change behavior.
$commit_step
Work autonomously and concisely. End by printing one line: ANSWER: <file:line — short summary>."
  ( cd "$agent_cwd" && SGREP_CACHE_DIR="$OUT/sgrep-cache" BENCH_SGREP_LOG="$OUT/${pfx}.sgrep.tsv" BENCH_SGREP_PHASE=agent timeout 1200 claude --bare --model sonnet --add-dir "$dir" "${ALLOW[@]}" \
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
