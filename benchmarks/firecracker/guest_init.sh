#!/bin/bash
set +e
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
printf 'nameserver 8.8.8.8\nnameserver 1.1.1.1\n' > /etc/resolv.conf
mkdir -p /results && mount /dev/vdb /results && chmod 777 /results
chmod 666 /dev/fuse 2>/dev/null
chmod u+s /usr/bin/fusermount3 /bin/fusermount3 /usr/bin/fusermount /bin/fusermount 2>/dev/null
grep -q user_allow_other /etc/fuse.conf 2>/dev/null || echo user_allow_other >> /etc/fuse.conf
export HOME=/home/ubuntu
[ -f /results/job.env ] && . /results/job.env
runuser -u ubuntu -- env HOME=/home/ubuntu ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  bash /bench/bench_lazy_fc.sh "$REPO_KEY" "$CLONE" "$PROMPT" /results > /results/run.log 2>&1
sync
poweroff -f 2>/dev/null; reboot -f 2>/dev/null; echo o > /proc/sysrq-trigger 2>/dev/null
