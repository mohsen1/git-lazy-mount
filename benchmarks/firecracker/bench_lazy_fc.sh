#!/usr/bin/env bash
set -euo pipefail

KEY="$1"
CLONE="$2"
PROMPT="$3"
export OUT="${4:-/results}"

exec bash /bench/bench_repo.sh "$KEY" "$CLONE" "$CLONE" "$CLONE" main "$PROMPT"
