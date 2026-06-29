#!/usr/bin/env bash
# Run the 20-repo agent benchmark locally in Docker and keep transcript
# summaries up to date as each repo finishes.
set -uo pipefail
cd "$(dirname "$0")"

usage() {
  cat >&2 <<'EOF'
Usage: ./run_all.sh [--push] [--resume] [--only a,b,c] [--run-id ID] [--out-dir DIR] [--image IMAGE] [--sgrep-timeout SECONDS] [--sgrep-broad-timeout SECONDS] [--sgrep-filtered-timeout SECONDS] [--sgrep-filtered-regex-timeout SECONDS] [--seed-timeout SECONDS]

Defaults:
  - local commits only; GH_TOKEN is forwarded only with --push
  - full sgrep timeout is off by default; set --sgrep-timeout to cap every query
  - unfiltered sgrep calls time out after 12s; set 0 to disable
  - file-filtered sgrep calls time out after 12s; set 0 to disable
  - filtered regex sgrep calls time out after 20s; set 0 to disable
  - identifier seed searches time out after 12s; set --seed-timeout to change
  - repos come from firecracker/repos.tsv
  - outputs go to out/<run-id>/<repo>
EOF
}

stamp() { date '+%Y-%m-%dT%H:%M:%S%z'; }

push=0
resume=0
only=""
run_id="agent-$(date '+%Y%m%d-%H%M%S')"
out_base=""
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
    --resume)
      resume=1
      shift
      ;;
    --only)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      only="$2"
      shift 2
      ;;
    --run-id)
      [ "$#" -ge 2 ] || { usage; exit 2; }
      run_id="$2"
      shift 2
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
    *)
      usage
      exit 2
      ;;
  esac
done

[ -n "$out_base" ] || out_base="out/$run_id"
mkdir -p "$out_base"
log="$out_base/batch.log"

include_key() {
  local key="$1"
  [ -z "$only" ] && return 0
  case ",$only," in
    *,"$key",*) return 0 ;;
    *) return 1 ;;
  esac
}

run_args=(--out-dir "$out_base" --image "$image" --sgrep-timeout "$sgrep_timeout" --sgrep-broad-timeout "$sgrep_broad_timeout" --sgrep-filtered-timeout "$sgrep_filtered_timeout" --sgrep-filtered-regex-timeout "$sgrep_filtered_regex_timeout" --seed-timeout "$seed_timeout")
[ "$push" -eq 0 ] || run_args+=(--push)

{
  echo "RUNID $run_id"
  echo "OUT $out_base"
  echo "START $(stamp)"
} | tee "$log"

overall_rc=0
while IFS=$'\t' read -r key clone prompt; do
  [ -n "$key" ] || continue
  include_key "$key" || continue
  if [ "$resume" -eq 1 ] && [ -s "$out_base/$key/metrics.json" ]; then
    echo "[$key] skip existing $(stamp)" | tee -a "$log"
    continue
  fi

  echo "[$key] start $(stamp) clone=$clone" | tee -a "$log"
  ./run.sh "${run_args[@]}" "$key" "$clone" "$clone" "$clone" main "$prompt"
  rc=$?
  [ "$rc" -eq 0 ] || overall_rc="$rc"
  echo "[$key] done $(stamp) rc=$rc" | tee -a "$log"

  if [ -s "$out_base/$key/metrics.json" ]; then
    python3 - "$out_base/$key/metrics.json" <<'PY' | tee -a "$log"
import json, sys
d = json.load(open(sys.argv[1]))
full = d["full"]["clone_s"] + d["full"]["agent_s"]
lazy = d["lazy"]["mount_s"] + d["lazy"]["agent_s"]
print(
    "  totals full={:.1f}s lazy={:.1f}s delta={:.1f}s "
    "full_agent={:.1f}s lazy_agent={:.1f}s".format(
        full, lazy, lazy - full, d["full"]["agent_s"], d["lazy"]["agent_s"]
    )
)
PY
  fi
  python3 format_transcripts.py "$out_base" >/dev/null 2>&1 || true
done < firecracker/repos.tsv

python3 format_transcripts.py "$out_base" | tee -a "$log"
echo "END $(stamp) rc=$overall_rc" | tee -a "$log"
exit "$overall_rc"
