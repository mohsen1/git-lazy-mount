#!/usr/bin/env bash
# Run one repo's benchmark (both modes) in a privileged FUSE container.
#
# Usage:
#   ./run.sh [--push] [--out-dir DIR] [--image IMAGE] [--sgrep-timeout SECONDS] [--sgrep-broad-timeout SECONDS] [--sgrep-filtered-timeout SECONDS] [--sgrep-filtered-regex-timeout SECONDS] [--seed-timeout SECONDS] \
#     <key> <clone owner/name> <push-fork owner/name> <upstream owner/name> \
#     <default-branch> "<question>"
#
# Requires ANTHROPIC_API_KEY in .benchenv or the environment. GH_TOKEN is not
# forwarded by default, even if .benchenv contains it; use --push to let the
# agent push benchmark branches to the configured fork.
set -euo pipefail
cd "$(dirname "$0")"

usage() {
  cat >&2 <<'EOF'
Run one repo's benchmark (both modes) in a privileged FUSE container.

Usage:
  ./run.sh [--push] [--out-dir DIR] [--image IMAGE] [--sgrep-timeout SECONDS] [--sgrep-broad-timeout SECONDS] [--sgrep-filtered-timeout SECONDS] [--sgrep-filtered-regex-timeout SECONDS] [--seed-timeout SECONDS] \
    <key> <clone owner/name> <push-fork owner/name> <upstream owner/name> \
    <default-branch> "<question>"

Requires ANTHROPIC_API_KEY in .benchenv or the environment. GH_TOKEN is not
forwarded by default, even if .benchenv contains it; use --push to let the
agent push benchmark branches to the configured fork.
EOF
}

push=0
out_base="out"
image="glm-bench"
sgrep_timeout="${SGREP_TIMEOUT_SECS:-0}"
sgrep_broad_timeout="${SGREP_BROAD_TIMEOUT_SECS:-12}"
sgrep_filtered_timeout="${SGREP_FILTERED_TIMEOUT_SECS:-12}"
sgrep_filtered_regex_timeout="${SGREP_FILTERED_REGEX_TIMEOUT_SECS:-20}"
seed_timeout="${BENCH_SEED_TIMEOUT_SECS:-12}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --push)
      push=1
      shift
      ;;
    --out-dir)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      out_base="$2"
      shift 2
      ;;
    --image)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      image="$2"
      shift 2
      ;;
    --sgrep-broad-timeout)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      sgrep_broad_timeout="$2"
      shift 2
      ;;
    --sgrep-timeout)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      sgrep_timeout="$2"
      shift 2
      ;;
    --sgrep-filtered-timeout)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      sgrep_filtered_timeout="$2"
      shift 2
      ;;
    --sgrep-filtered-regex-timeout)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      sgrep_filtered_regex_timeout="$2"
      shift 2
      ;;
    --seed-timeout)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      seed_timeout="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      usage
      exit 2
      ;;
    *)
      break
      ;;
  esac
done

[ "$#" -eq 6 ] || { usage; exit 2; }
key="$1"

out_dir="$out_base/$key"
mkdir -p "$out_dir"
chmod 777 "$out_dir"
case "$out_dir" in
  /*) host_out_dir="$out_dir" ;;
  *) host_out_dir="$PWD/$out_dir" ;;
esac

env_args=()
tmp_env=""
cleanup() {
  [ -z "$tmp_env" ] || rm -f "$tmp_env"
}
trap cleanup EXIT

add_env_file_line() {
  local name="$1"
  [ -f .benchenv ] || return 0
  local line
  line="$(grep -E "^${name}=" .benchenv 2>/dev/null | tail -1 || true)"
  [ -n "$line" ] || return 0
  if [ -z "$tmp_env" ]; then
    tmp_env="$(mktemp)"
    chmod 600 "$tmp_env"
    env_args+=(--env-file "$tmp_env")
  fi
  printf '%s\n' "$line" >> "$tmp_env"
}

if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  env_args+=(-e ANTHROPIC_API_KEY)
else
  add_env_file_line ANTHROPIC_API_KEY
fi

if [ "$push" -eq 1 ]; then
  if [ -n "${GH_TOKEN:-}" ]; then
    env_args+=(-e GH_TOKEN)
  else
    add_env_file_line GH_TOKEN
  fi
fi

if [ -n "$sgrep_broad_timeout" ]; then
  env_args+=(-e "SGREP_BROAD_TIMEOUT_SECS=$sgrep_broad_timeout")
fi
if [ -n "$sgrep_filtered_timeout" ]; then
  env_args+=(-e "SGREP_FILTERED_TIMEOUT_SECS=$sgrep_filtered_timeout")
fi
if [ -n "$sgrep_timeout" ] && [ "$sgrep_timeout" != "0" ]; then
  env_args+=(-e "SGREP_TIMEOUT_SECS=$sgrep_timeout")
fi
if [ -n "$sgrep_filtered_regex_timeout" ]; then
  env_args+=(-e "SGREP_FILTERED_REGEX_TIMEOUT_SECS=$sgrep_filtered_regex_timeout")
fi
if [ -n "$seed_timeout" ]; then
  env_args+=(-e "BENCH_SEED_TIMEOUT_SECS=$seed_timeout")
fi

docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  "${env_args[@]}" \
  -v "$PWD/bench_repo.sh:/bench/bench_repo.sh:ro" \
  -v "$PWD/ts_prepend.py:/bench/ts_prepend.py:ro" \
  -v "$host_out_dir:/out" \
  "$image" bash /bench/bench_repo.sh "$@"
