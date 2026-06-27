#!/bin/bash
exec > /var/log/glm-bench.log 2>&1
cd /opt/fcbench || exit 0
export ANTHROPIC_API_KEY=""
cat > bench_fc.sh <<'BENCH'
#!/usr/bin/env bash
set -uo pipefail
KEY="$1"; UPSTREAM="$2"; OUT="${4:-/results}"; export HOME=/home/ubuntu PATH=/usr/local/bin:/usr/bin:/bin; mkdir -p "$OUT" /work
now(){ date +%s.%N; }; secs(){ python3 -c "print(round($2-$1,1))"; }
dub(){ du -sb "$1" 2>/dev/null|cut -f1; }
git config --global user.name b; git config --global user.email b@b.co; git config --global --add safe.directory '*'
T=$(now); timeout 900 git clone --depth 1 "https://github.com/$UPSTREAM" /work/c >/dev/null 2>&1; crc=$?; cs=$(secs $T $(now))
cb=$(dub /work/c); files=$(cd /work/c 2>/dev/null && git ls-files|wc -l|tr -d ' '||echo 0); rm -rf /work/c
T=$(now); timeout 400 /usr/local/bin/git-lazy-mount "https://github.com/$UPSTREAM" /work/lazy > "$OUT/mount.err" 2>&1; mrc=$?; ms=$(secs $T $(now))
WS=$(ls -dt /home/ubuntu/.local/share/git-lazy-mount/workspaces/*/ 2>/dev/null|head -1); mb=$(dub "$WS")
fusermount3 -u /work/lazy 2>/dev/null
python3 -c "import json;print(json.dumps({'repo':'$KEY','upstream':'$UPSTREAM','files':int('$files' or 0),'clone_mb':round(int('$cb' or 0)/1048576),'clone_s':float('$cs'),'clone_rc':$crc,'mount_mb':round(int('$mb' or 0)/1048576),'mount_s':float('$ms'),'mount_rc':$mrc}))" > "$OUT/metrics.json"
echo DONE
BENCH
chmod +x bench_fc.sh
mkdir -p rmnt
if mount -o loop rootfs.base.ext4 rmnt; then cp bench_fc.sh rmnt/bench/bench_lazy_fc.sh; chmod +x rmnt/bench/bench_lazy_fc.sh; umount rmnt; fi
for l in $(losetup -a 2>/dev/null|cut -d: -f1); do losetup -d "$l" 2>/dev/null; done
umount run/*/rmnt 2>/dev/null; rm -rf run; mkdir -p run
: > /tmp/seq.log; echo "START $(date +%H:%M:%S)" >> /tmp/seq.log
i=0
while IFS=$'\t' read -r key clone prompt <&3; do
  [ -z "$key" ] && continue
  echo "[$key] start $(date +%H:%M:%S)" >> /tmp/seq.log
  timeout --kill-after=60 600 bash run_vm.sh "$i" "$key" "$clone" noagent </dev/null > "run_${key}.log" 2>&1
  echo "[$key] done $(date +%H:%M:%S) $(tr -d '\n ' < run/$key/metrics.json 2>/dev/null|head -c 90)" >> /tmp/seq.log
  i=$((i+1))
done 3< repos.tsv
echo "ALL_DONE $(date +%H:%M:%S)" >> /tmp/seq.log
