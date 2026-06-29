#!/usr/bin/env bash
set -euo pipefail

cd /opt/fcbench

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY is required" >&2
  exit 2
fi

mkdir -p rmnt
if mount -o loop rootfs.base.ext4 rmnt; then
  cp bench_lazy_fc.sh bench_repo.sh guest_init.sh ts_prepend.py rmnt/bench/
  chmod +x rmnt/bench/*
  umount rmnt
fi

for l in $(losetup -a 2>/dev/null | cut -d: -f1); do
  losetup -d "$l" 2>/dev/null || true
done
umount run/*/rmnt 2>/dev/null || true
rm -rf run
mkdir -p run

: > /tmp/agent-seq.log
echo "START $(date +%Y-%m-%dT%H:%M:%S%z)" >> /tmp/agent-seq.log
i=0
while IFS=$'\t' read -r key clone prompt <&3; do
  [ -n "$key" ] || continue
  echo "[$key] start $(date +%Y-%m-%dT%H:%M:%S%z)" >> /tmp/agent-seq.log
  timeout --kill-after=60 2400 bash run_vm.sh "$i" "$key" "$clone" "$prompt" </dev/null > "run_${key}.log" 2>&1
  echo "[$key] done $(date +%Y-%m-%dT%H:%M:%S%z) $(tr -d '\n ' < "run/$key/metrics.json" 2>/dev/null | head -c 200)" >> /tmp/agent-seq.log
  i=$((i+1))
done 3< repos.tsv
echo "ALL_DONE $(date +%Y-%m-%dT%H:%M:%S%z)" >> /tmp/agent-seq.log
