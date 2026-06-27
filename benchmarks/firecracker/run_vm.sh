#!/usr/bin/env bash
# Host-side: launch ONE Firecracker microVM for one repo. Args: IDX REPO_KEY CLONE "PROMPT"
set -xuo pipefail
cd /opt/fcbench; IDX="$1"; KEY="$2"; CLONE="$3"; PROMPT="$4"
VD=run/$KEY; mkdir -p "$VD"
# per-VM rootfs (reflink copy = instant where supported) + a results drive
cp --reflink=auto rootfs.base.ext4 "$VD/rootfs.ext4"
truncate -s 256M "$VD/results.ext4"; mkfs.ext4 -qF "$VD/results.ext4"
mkdir -p "$VD/rmnt"; mount -o loop "$VD/results.ext4" "$VD/rmnt"
printf 'REPO_KEY=%q\nCLONE=%q\nPROMPT=%q\nANTHROPIC_API_KEY=%q\n' "$KEY" "$CLONE" "$PROMPT" "${ANTHROPIC_API_KEY}" > "$VD/rmnt/job.env"
umount "$VD/rmnt"
# networking: one /30 per VM, NAT out the host's main iface
SUB=$((IDX*4)); TAP="fc${IDX}"; GUESTIP="172.16.$((SUB/256)).$((SUB%256+2))"; GW="172.16.$((SUB/256)).$((SUB%256+1))"
ip tuntap add "$TAP" mode tap 2>/dev/null || true; ip addr add "$GW/30" dev "$TAP" 2>/dev/null || true; ip link set "$TAP" up
HOSTIF=$(ip route get 1.1.1.1 | grep -oP 'dev \K\S+'); sysctl -qw net.ipv4.ip_forward=1
iptables -t nat -C POSTROUTING -o "$HOSTIF" -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -o "$HOSTIF" -j MASQUERADE
iptables -C FORWARD -i "$TAP" -o "$HOSTIF" -j ACCEPT 2>/dev/null || iptables -A FORWARD -i "$TAP" -o "$HOSTIF" -j ACCEPT
iptables -C FORWARD -i "$HOSTIF" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || iptables -A FORWARD -i "$HOSTIF" -o "$TAP" -m state --state RELATED,ESTABLISHED -j ACCEPT
MAC="06:00:AC:10:$(printf '%02x' $((SUB/256))):$(printf '%02x' $((SUB%256+2)))"
cat > "$VD/vm.json" <<JSON
{ "boot-source": { "kernel_image_path": "/opt/fcbench/vmlinux",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off init=/bench/guest_init.sh ip=${GUESTIP}::${GW}:255.255.255.252::eth0:off" },
  "drives": [ {"drive_id":"rootfs","path_on_host":"/opt/fcbench/$VD/rootfs.ext4","is_root_device":true,"is_read_only":false},
              {"drive_id":"results","path_on_host":"/opt/fcbench/$VD/results.ext4","is_root_device":false,"is_read_only":false} ],
  "network-interfaces": [ {"iface_id":"eth0","guest_mac":"$MAC","host_dev_name":"$TAP"} ],
  "machine-config": { "vcpu_count": 4, "mem_size_mib": 8192 } }
JSON
install -m0755 guest_init.sh "$VD/guest_init.sh" 2>/dev/null || true  # (also baked into rootfs at /bench)
timeout --kill-after=60 1500 firecracker --no-api --config-file "$VD/vm.json" >"$VD/fc.log" 2>&1 || true
# extract results
mount -o loop "$VD/results.ext4" "$VD/rmnt"; cp "$VD/rmnt/metrics.json" "$VD/metrics.json" 2>/dev/null || echo '{}' > "$VD/metrics.json"
cp "$VD/rmnt/"*.tsv "$VD/" 2>/dev/null || true; umount "$VD/rmnt"
ip link del "$TAP" 2>/dev/null || true
echo "[$KEY] $(cat $VD/metrics.json)"
