#!/usr/bin/env bash
# Run one repo's benchmark (both modes) in a privileged FUSE container.
# Usage: ./run.sh <key> <clone owner/name> <push-fork owner/name> <upstream owner/name> <default-branch> "<question>"
# Requires a .benchenv file with ANTHROPIC_API_KEY=... and GH_TOKEN=...
set -u
cd "$(dirname "$0")"
key="$1"; mkdir -p "out/$key"; chmod 777 "out/$key"
docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
  --env-file .benchenv \
  -v "$PWD/bench_repo.sh:/bench/bench_repo.sh:ro" -v "$PWD/out/$key:/out" \
  glm-bench bash /bench/bench_repo.sh "$@"
