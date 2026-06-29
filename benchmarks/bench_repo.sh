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
sgrep_cache_for() {
  case "${BENCH_SGREP_CACHE_SCOPE:-mode}" in
    shared) printf '%s\n' "$OUT/sgrep-cache" ;;
    *) printf '%s\n' "$OUT/$1.sgrep-cache" ;;
  esac
}
sgrep_seed_cache_for() {
  case "${BENCH_SEED_CACHE_SCOPE:-shared}" in
    mode) sgrep_cache_for "$1" ;;
    *) printf '%s\n' "$OUT/seed.sgrep-cache" ;;
  esac
}
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

cache_root="${BENCH_SHARED_SGREP_CACHE:-${SGREP_CACHE_DIR:-$HOME/.cache/glm-bench-sgrep}}"
mkdir -p "$cache_root"
export SGREP_CACHE_DIR="${SGREP_CACHE_DIR:-$cache_root}"
log="${BENCH_SGREP_LOG:-}"
err="$(mktemp)"
out_tmp="$(mktemp)"
seen_dir="$cache_root/observed"
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
replay="${BENCH_SGREP_REPLAY_CACHE:-0}"
{
  flock 9
  if [ -e "$seen_dir/$key" ]; then
    cache="hit"
  fi
  if [ "$replay" = "1" ] && [ -f "$seen_dir/$key.out" ] && [ -f "$seen_dir/$key.err" ] && [ -f "$seen_dir/$key.rc" ]; then
    cache="replay"
    cp "$seen_dir/$key.out" "$out_tmp"
    cp "$seen_dir/$key.err" "$err"
    rc="$(cat "$seen_dir/$key.rc" 2>/dev/null || printf '1')"
    if ! [[ "$rc" =~ ^[0-9]+$ ]]; then
      rc=1
    fi
  else
    if [ -n "$limit" ] && [ "$limit" != "0" ]; then
      timeout --kill-after=2 "$limit" /usr/local/bin/sgrep --repo "$BENCH_UPSTREAM" "${args[@]}" >"$out_tmp" 2>"$err"
      rc=$?
    else
      /usr/local/bin/sgrep --repo "$BENCH_UPSTREAM" "${args[@]}" >"$out_tmp" 2>"$err"
      rc=$?
    fi
    if [ "$replay" = "1" ] || { [ "$rc" -ne 124 ] && [ "$rc" -ne 137 ]; }; then
      cp "$out_tmp" "$seen_dir/$key.out"
      cp "$err" "$seen_dir/$key.err"
      printf '%s\n' "$rc" > "$seen_dir/$key.rc"
      : > "$seen_dir/$key"
    fi
  fi
  if [ "$replay" != "1" ] && [ "$rc" -ne 124 ] && [ "$rc" -ne 137 ]; then
    : > "$seen_dir/$key"
  fi
} 9>"$cache_root/.lock"
cat "$out_tmp"
cat "$err" >&2
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
  local pfx="$1" dir="$2" files_list="$3" cache seed="$OUT/${pfx}.seed.md" log="$OUT/${pfx}.sgrep.tsv"
  local seed_timeout="${BENCH_SEED_TIMEOUT_SECS:-12}"
  local seed_count="${BENCH_SEED_COUNT:-10}"
  cache="$(sgrep_seed_cache_for "$pfx")"
  mkdir -p "$cache"
  : > "$seed"
  [ "${BENCH_SEED_SEARCH:-1}" = "1" ] || return 0
  python3 - "$QUESTION" <<'PY' | while IFS= read -r term; do
import re, sys
question = sys.argv[1]
question_lc = question.lower()
method_question = "method" in question_lc
definition_question = any(word in question_lc for word in ("defined", "definition", "implemented", "registered", "entry point", "primitive"))
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
def snake(name):
    return re.sub(r"(?<!^)([A-Z])", r"_\1", name).lower()
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
        if "function" in question_lc or definition_question:
            add_variant("function " + term)
for raw in re.findall(r"`([^`]+)`", sys.argv[1]):
    raw = raw.strip()
    suppress_word_pairs = True
    block = re.fullmatch(r"\{#([A-Za-z][A-Za-z0-9_]*)\}", raw)
    if block:
        name = block.group(1)
        add(name[:1].upper() + name[1:] + "Block")
        add_variant(name)
        continue
    deno_api = re.fullmatch(r"Deno\.([A-Za-z_$][A-Za-z0-9_$]*)", raw)
    if deno_api:
        name = deno_api.group(1)
        op_async = "op_fs_" + snake(name) + "_async"
        op_sync = "op_fs_" + snake(name) + "_sync"
        if "registered" in question_lc or "op" in question_lc:
            add(op_async)
            add(op_sync)
            add(name)
        else:
            add(name)
            add_variant(op_async)
            add_variant(op_sync)
        continue
    call = re.fullmatch(r"([A-Za-z_$][A-Za-z0-9_$]*)\(\)", raw)
    if call:
        name = call.group(1)
        if definition_question:
            add("function " + name)
        add(name)
        add_definition_variants(name)
        continue
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
    if re.fullmatch(r"[A-Z][A-Z0-9_]*", raw) and "command" in question_lc:
        command_name = raw.lower() + "Command"
        add("void " + command_name)
        add(command_name)
        continue
    if not skip_raw:
        add(raw)
    if hook:
        add_variant("mount" + hook.group(1) + "Impl")
    for part in re.findall(r"[A-Za-z_$][A-Za-z0-9_$]*(?:[._-][A-Za-z_$][A-Za-z0-9_$]*)*|__[A-Za-z0-9_]+__", raw):
        if not (skip_raw and part == raw):
            add(part)
        add_definition_variants(part)
for token in re.findall(r"\b[A-Za-z][A-Za-z0-9_]*(?:[-._][A-Za-z0-9_]+)+\b|\b[A-Z][A-Za-z0-9_]{2,}\b|\b[A-Z0-9_]{3,}\b|\b[A-Za-z_][A-Za-z0-9_]*\(\)", question):
    call = re.fullmatch(r"([A-Za-z_$][A-Za-z0-9_$]*)\(\)", token)
    deno_api = re.fullmatch(r"Deno\.([A-Za-z_$][A-Za-z0-9_$]*)", token)
    if call:
        name = call.group(1)
        if definition_question:
            add("function " + name)
        add(name)
        add_definition_variants(name)
    elif deno_api:
        name = deno_api.group(1)
        op_async = "op_fs_" + snake(name) + "_async"
        op_sync = "op_fs_" + snake(name) + "_sync"
        if "registered" in question_lc or "op" in question_lc:
            add(op_async)
            add(op_sync)
            add(name)
        else:
            add(name)
            add_variant(op_async)
            add_variant(op_sync)
    elif re.fullmatch(r"[A-Z][A-Z0-9_]*", token) and "command" in question_lc:
        command_name = token.lower() + "Command"
        add("void " + command_name)
        add(command_name)
    else:
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
      path_hints="$(python3 - "$term" "$files_list" "$dir" "$QUESTION" <<'PY'
import os
import re
import sys

original_term = sys.argv[1].strip()
term = original_term
paths_file = sys.argv[2]
repo_dir = sys.argv[3]
question_lc = sys.argv[4].lower() if len(sys.argv) > 4 else ""
for prefix in ("function ", "class ", "def ", "void ", "pub fn ", "fn "):
    if term.startswith(prefix):
        term = term[len(prefix):].strip()
term = re.sub(r"\(\)$", "", term)
leaf = re.split(r"[.:/\\#]+", term)[-1]

def snake(name):
    return re.sub(r"(?<!^)([A-Z])", r"_\1", name).lower()

variants = {term, leaf}
for value in list(variants):
    if value:
        variants.add(value.lower())
        variants.add(snake(value))
        variants.add(snake(value).replace("_", "-"))
variants = {v for v in variants if v and len(v) >= 2}

source_exts = {
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".go", ".py", ".c", ".cc", ".cpp",
    ".cxx", ".h", ".hpp", ".hh", ".m", ".mm", ".inc", ".td", ".dart",
    ".java", ".kt", ".kts", ".swift", ".gd", ".cs", ".scala",
}
demote = re.compile(r"(^|/)(test|tests|spec|specs|doc|docs|example|examples|fixture|fixtures|benchmark|bench|changelog|changelogs)(/|$)", re.I)
rows = []
with open(paths_file, encoding="utf-8", errors="replace") as handle:
  raw_paths = list(handle)
path_set = {p.strip() for p in raw_paths if p.strip()}
for raw in raw_paths:
    path = raw.strip()
    if not path:
        continue
    stem, ext = os.path.splitext(os.path.basename(path))
    if ext.lower() not in source_exts:
        continue
    p_l = path.lower()
    stem_l = stem.lower()
    score = 0
    reason = ""
    for variant in variants:
        v_l = variant.lower()
        if stem == variant:
            score = max(score, 120)
            reason = "basename exact source"
        elif stem_l == v_l:
            score = max(score, 110)
            reason = "basename case-insensitive source"
        elif v_l in stem_l:
            score = max(score, 70)
            reason = "basename contains term"
        elif v_l in p_l:
            score = max(score, 35)
            reason = "path contains term"
    if score:
        if demote.search(path):
            score -= 40
            reason = reason + ", demoted test/doc"
        rows.append((score, len(path), path, reason))

ranked = sorted(rows, key=lambda r: (-r[0], r[1], r[2]))

def line_hint(path):
    name = re.escape(leaf)
    patterns = []
    original_lc = original_term.lower()
    if (
        original_lc.startswith(("function ", "def ", "fn ", "pub fn ", "void "))
        or " " in original_term
        or re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", leaf)
    ):
        patterns.extend([
            rf"\bexport\s+function\s+{name}\b",
            rf"\bfunction\s+{name}\b",
            rf"\bdef\s+{name}\b",
            rf"\bfunc\s*\([^)]*\)\s*{name}\b",
            rf"\bfunc\s+{name}\b",
            rf"\bfn\s+{name}\b",
            rf"\bvoid\s+{name}\b",
        ])
    fallback_patterns = []
    for value in (original_term, term, leaf):
        if not value:
            continue
        if re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", value):
            fallback_patterns.append(rf"\b{re.escape(value)}\b")
        else:
            fallback_patterns.append(re.escape(value))
    patterns.extend(fallback_patterns)
    try:
        with open(os.path.join(repo_dir, path), encoding="utf-8", errors="replace") as handle:
            lines = list(handle)
    except OSError:
        return None
    compiled = [re.compile(p) for p in patterns if p]
    for pattern in compiled:
        matches = []
        for idx, line in enumerate(lines, 1):
            if pattern.search(line):
                text = line.strip()
                score = 0
                if "{" in text or text.endswith(":"):
                    score += 20
                if "<" in text.split("(", 1)[0]:
                    score -= 5
                if text.startswith(("//", "/*", "*")):
                    score -= 25
                matches.append((score, -idx, idx, text[:180]))
        if matches:
            _, _, idx, text = max(matches)
            return idx, text
    return None

def emit_path(path, reason):
    hint = line_hint(path)
    if hint:
        line_no, text = hint
        print(f"path: {path}:{line_no}: {text} ({reason})")
    else:
        print(f"path: {path} ({reason})")

targeted = []
if leaf == "append" and "compiler" in question_lc:
    targeted.append(("src/cmd/compile/internal/ssagen/ssa.go", "compiler builtin implementation hint"))
if leaf == "EachBlock" and "compiler" in question_lc:
    targeted.extend([
        ("packages/svelte/src/compiler/phases/3-transform/client/visitors/EachBlock.js", "compiler block transform hint"),
        ("packages/svelte/src/compiler/phases/3-transform/server/visitors/EachBlock.js", "compiler block transform hint"),
        ("packages/svelte/src/compiler/phases/2-analyze/visitors/EachBlock.js", "compiler block analysis hint"),
    ])
if leaf == "setState":
    targeted.append(("packages/flutter/lib/src/widgets/framework.dart", "framework method definition hint"))
if leaf == "setCommand":
    targeted.append(("src/t_string.c", "redis string command implementation hint"))
if leaf.startswith("op_fs_") and "registered" in question_lc:
    targeted.append(("ext/fs/lib.rs", "deno fs op registration hint"))

for path, reason in targeted:
    if path in path_set:
        emit_path(path, reason)
        raise SystemExit

printed = 0
for score, _, path, reason in ranked:
    hint = line_hint(path) if score >= 70 else None
    if hint:
        line_no, text = hint
        print(f"path: {path}:{line_no}: {text} ({reason})")
    else:
        print(f"path: {path} ({reason})")
    printed += 1
    if score >= 110:
        break
    if printed >= 5:
        break
PY
)"
      if [ -n "$path_hints" ]; then
        printf '%s\n' "$path_hints"
        printf '\n'
        first_hint="$(printf '%s\n' "$path_hints" | head -1)"
        if printf '%s\n' "$first_hint" | grep -q 'basename .*source\|content definition scan\|hint)' && \
           ! printf '%s\n' "$first_hint" | grep -q 'demoted test/doc'; then
          break
        fi
      fi
      start="$(now)"
      err="$(mktemp)"
      seed_file_filter='\.(rs|ts|tsx|js|jsx|go|py|c|cc|cpp|cxx|h|hpp|hh|m|mm|inc|td|dart|java|kt|kts|swift|gd|cs|scala|py)$'
      case "$term" in
        mount*Impl) seed_file_filter='ReactFiberHooks' ;;
        "void "*Command) seed_file_filter='\.c$' ;;
        *Command) seed_file_filter='\.(c|h)$' ;;
        op_fs_*) seed_file_filter='ext/fs' ;;
      esac
      if [[ "$QUESTION" == *'Deno.'* && "$term" == readFile ]]; then
        seed_file_filter='ext/fs'
      fi
      out="$(SGREP_CACHE_DIR="$cache" BENCH_SHARED_SGREP_CACHE="$cache" BENCH_SGREP_REPLAY_CACHE=1 BENCH_SGREP_LOG="$log" BENCH_SGREP_PHASE=seed SGREP_WALL_TIMEOUT_SECS="$seed_timeout" \
        sgrep --literal --count "$seed_count" --no-overlay \
        --file "$seed_file_filter" "$term" 2>"$err")"
      rc=$?
      if [ -s "$err" ]; then
        cat "$err" >&2
      fi
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
      if [ "$rc" -eq 0 ] && printf '%s\n' "$out" | awk -F: 'NF && $1 !~ /(^|\/)(test|tests|spec|specs|doc|docs|example|examples|fixture|fixtures|benchmark|bench|changelog|changelogs)(\/|$)/ { found=1; exit } END { exit found ? 0 : 1 }'; then
        if [[ "$QUESTION" == *registered* ]] && ! printf '%s\n' "$out" | awk -F: '$1 ~ /(^|\/)(lib|mod|registry|registrar|commands?|cmd)[^\/]*\.(rs|c|cc|cpp|h|hpp|js|ts)$/ { found=1; exit } END { exit found ? 0 : 1 }'; then
          :
        else
          break
        fi
      fi
    } >> "$seed"
  done
}

commit_one_file(){
  local dir="$1" branch="$2" pfx="$3" changed count
  {
    step_start="$(now)"
    changed="$(python3 - "$dir" "$OUT/${pfx}.transcript.tsv" <<'PY'
import json
import os
import sys

root = os.path.realpath(sys.argv[1])
transcript = sys.argv[2]
edited = []
with open(transcript, encoding="utf-8", errors="replace") as handle:
    for line in handle:
        try:
            _, payload = line.rstrip("\n").split("\t", 1)
            event = json.loads(payload)
        except Exception:
            continue
        if event.get("type") != "assistant":
            continue
        for item in event.get("message", {}).get("content", []):
            if item.get("type") != "tool_use" or item.get("name") != "Edit":
                continue
            path = item.get("input", {}).get("file_path")
            if not path:
                continue
            real = os.path.realpath(path)
            if real.startswith(root + os.sep):
                edited.append(os.path.relpath(real, root))
if edited:
    print(edited[-1])
PY
)"
    echo "edited_file_from_transcript=${changed:-NONE}"
    echo "resolve_edit_s=$(secs "$step_start" "$(now)")"
    if [ -z "$changed" ]; then
      step_start="$(now)"
      changed="$(git -C "$dir" diff --name-only --diff-filter=ACMRT | grep -v '^CLAUDE.md$' || true)"
      count="$(printf '%s\n' "$changed" | awk 'NF { n += 1 } END { print n + 0 }')"
      echo "git_diff_s=$(secs "$step_start" "$(now)")"
      if [ "$count" -ne 1 ]; then
        echo "expected exactly one modified tracked file, got $count" >&2
        printf '%s\n' "$changed" >&2
        return 2
      fi
      changed="$(printf '%s\n' "$changed" | awk 'NF { print; exit }')"
    fi
    echo "committing $changed"
    step_start="$(now)"
    git -C "$dir" checkout -b "$branch"
    echo "checkout_s=$(secs "$step_start" "$(now)")"
    step_start="$(now)"
    git -C "$dir" add --no-refresh -- "$changed"
    echo "add_s=$(secs "$step_start" "$(now)")"
    step_start="$(now)"
    git -C "$dir" commit -m "glm-bench: note where the answer lives"
    echo "commit_s=$(secs "$step_start" "$(now)")"
    if [ -n "${GH_TOKEN:-}" ]; then
      step_start="$(now)"
      git -C "$dir" push -u origin "$branch"
      echo "push_s=$(secs "$step_start" "$(now)")"
    fi
  } > "$OUT/${pfx}.commit.log" 2>&1
}

run_agent(){ # dir branch transcript_prefix files_list
  local dir="$1" branch="$2" pfx="$3" files_list="$4"
  local agent_cwd="/work/agent-$pfx"
  mkdir -p "$agent_cwd"
  printf '%s\n' "$CLAUDE_MD" > "$dir/CLAUDE.md"
  printf '%s\n' "$CLAUDE_MD" > "$agent_cwd/CLAUDE.md"
  local cache_dir
  cache_dir="$(sgrep_cache_for "$pfx")"
  mkdir -p "$cache_dir"
  : > "$OUT/${pfx}.sgrep.tsv"
  seed_searches "$pfx" "$dir" "$files_list"
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

If the seed hints include a path candidate with a line number, your first action must be one Read around that line with a small offset and limit; do not first Read from the top of the file. If seed hints include a production source file without a line number (not tests/docs/examples/fixtures), Read that file before running any new search. If the seed hints are only tests/docs or are empty, run one narrow literal \`sgrep\` using the exact identifier plus an obvious implementation word from the question.

If the question asks where something is registered and a seed/read result identifies its implementation in a source directory, inspect the likely registration file in that same directory (for example lib.rs, mod.rs, registry files, or command tables) before running another search.

If your first Read confirms the seed path+line contains the named declaration, compiler visitor, command handler, or exact implementation asked about, stop exploring: make the one comment edit at that location and answer from the visible snippet. Do not read more context just to make the final explanation richer.

To search code, use \`sgrep --count 20 <literal-or-narrow-regex>\` — NOT grep, rg, find, git grep, or \`sgrep ... | head\`. Do not run multiple \`sgrep\` commands at once. Prefer one literal symbol or exact phrase; avoid alternation (\`|\`) and \`.*\` unless no literal search is possible. Use \`--file\` when the likely extension or directory is obvious; file filters are regexes or exact paths, not shell globs, so use \`--file '\\.c$'\` rather than \`--file '*.c'\`. If a search times out, do not retry the same shape; Read a seed/prior hit or narrow to one exact file/literal. After one plausible file+line, Read it instead of continuing broad searches. Keep the search phase to at most two new \`sgrep\` calls after the seed hints unless the first hits are clearly wrong.

Once you have located the answer:
1. Make ONE small, real code edit at the exact file+location you identified: add a single clarifying comment line (one or two lines, in ONE file) that summarizes the finding. Do not change behavior.
2. Do not run git. The benchmark harness will create the branch and commit the one changed file after you finish.
Work autonomously and concisely. Do not print interim narration; use tool calls until the final response. End by printing one line: ANSWER: <file:line — short summary>."
  ( cd "$agent_cwd" && SGREP_CACHE_DIR="$cache_dir" BENCH_SHARED_SGREP_CACHE="$cache_dir" BENCH_SGREP_LOG="$OUT/${pfx}.sgrep.tsv" BENCH_SGREP_PHASE=agent timeout 1200 claude --bare --model sonnet --add-dir "$dir" "${ALLOW[@]}" \
        --output-format stream-json --verbose -p "$prompt" 2> "$OUT/${pfx}.claude.err" \
      | python3 -u /bench/ts_prepend.py ) > "$OUT/${pfx}.transcript.tsv"
  commit_one_file "$dir" "$branch" "$pfx" || true
}
dub(){ du -sb "$1" 2>/dev/null | cut -f1; }
mib(){ python3 -c "print(round(${1:-0}/1048576,1))"; }

############################  FULL CLONE  ############################
log "FULL: git clone https://github.com/$CLONE (push->$FORK)"
FDIR=/work/full
T=$(now); git clone "https://github.com/$CLONE" "$FDIR" > "$OUT/full.clone.log" 2>&1; full_clone_s=$(secs $T $(now))
push_url="https://github.com/$FORK"
git -C "$FDIR" remote set-url --push origin "$push_url"
git -C "$FDIR" ls-files > "$OUT/full.files"
full_files=$(wc -l < "$OUT/full.files" | tr -d ' ')
full_worktree_b=$(du -sb --exclude=.git "$FDIR" | cut -f1)
full_dotgit_b=$(dub "$FDIR/.git")
log "FULL: agent (files=$full_files worktree=$(mib $full_worktree_b)MiB)"
T=$(now); run_agent "$FDIR" "glm-bench-full" "full" "$OUT/full.files"; full_agent_s=$(secs $T $(now))
full_remote_sha=$(git -C "$FDIR" ls-remote --heads "$push_url" glm-bench-full 2>/dev/null | cut -c1-7)
log "FULL: done agent_s=$full_agent_s pushed=${full_remote_sha:-NONE}"
rm -rf "$FDIR"

############################  LAZY MOUNT  ############################
log "LAZY: git lazy-mount https://github.com/$CLONE (push->$FORK)"
LDIR=/work/lazy
T=$(now); git lazy-mount "https://github.com/$CLONE" "$LDIR" > "$OUT/lazy.mount.log" 2>&1; lazy_mount_s=$(secs $T $(now))
git -C "$LDIR" remote set-url --push origin "$push_url"
WS=$(ls -dt /home/ubuntu/.local/share/git-lazy-mount/workspaces/*/ 2>/dev/null | head -1)
git -C "$LDIR" ls-files > "$OUT/lazy.files"
lazy_files=$(wc -l < "$OUT/lazy.files" | tr -d ' ')
lazy_initial_b=$(dub "$WS")
log "LAZY: agent (files=$lazy_files initial=$(mib $lazy_initial_b)MiB ws=$WS)"
T=$(now); run_agent "$LDIR" "glm-bench-lazy" "lazy" "$OUT/lazy.files"; lazy_agent_s=$(secs $T $(now))
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
