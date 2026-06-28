#!/usr/bin/env bash
# Run one repo's benchmark (both modes) in a privileged FUSE container.
# Usage: ./run.sh <key> <clone owner/name> <push-fork owner/name> <upstream owner/name> <default-branch> "<question>"
# Requires ANTHROPIC_API_KEY in .benchenv or the environment. GH_TOKEN is
# optional; without it the agent benchmark commits locally instead of pushing.
set -u
cd "$(dirname "$0")"
key="$1"; mkdir -p "out/$key"; chmod 777 "out/$key"
env_args=()
if [ -f .benchenv ]; then
  env_args=(--env-file .benchenv)
fi
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  env_args+=(-e ANTHROPIC_API_KEY)
fi
if [ -n "${GH_TOKEN:-}" ]; then
  env_args+=(-e GH_TOKEN)
fi
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  "${env_args[@]}" \
  -v "$PWD/bench_repo.sh:/bench/bench_repo.sh:ro" \
  -v "$PWD/ts_prepend.py:/bench/ts_prepend.py:ro" \
  -v "$PWD/out/$key:/out" \
  glm-bench bash /bench/bench_repo.sh "$@"
