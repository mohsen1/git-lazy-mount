#!/usr/bin/env bash
set -euo pipefail
ARCH=x86_64
sudo mkdir -p /opt/fcbench && sudo chown "$USER" /opt/fcbench
cd /opt/fcbench; tar xzf /tmp/fcharness.tgz -C /opt/fcbench
if ! command -v firecracker >/dev/null; then
  ver=v1.10.1
  curl -sSL "https://github.com/firecracker-microvm/firecracker/releases/download/${ver}/firecracker-${ver}-${ARCH}.tgz" | tar xz
  sudo install -m0755 release-${ver}-${ARCH}/firecracker-${ver}-${ARCH} /usr/local/bin/firecracker
fi
sudo apt-get update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq docker.io e2fsprogs jq git build-essential bc bison flex libelf-dev libssl-dev xz-utils >/dev/null
sudo systemctl enable --now docker >/dev/null 2>&1 || true
# FUSE-enabled guest kernel
if [ ! -f vmlinux ]; then
  KV=5.10.225
  [ -d linux-$KV ] || curl -sSL "https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-$KV.tar.xz" | tar xJ
  pushd linux-$KV >/dev/null
  curl -fsSL -o .config https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-5.10.config
  ./scripts/config --enable CONFIG_FUSE_FS --enable CONFIG_VIRTIO_FS
  make olddefconfig >/dev/null 2>&1
  make -j"$(nproc)" vmlinux >/dev/null 2>&1
  cp vmlinux /opt/fcbench/vmlinux
  popd >/dev/null
fi
# rootfs
if [ ! -f rootfs.base.ext4 ]; then
  [ -d git-lazy-mount ] || git clone --depth 1 https://github.com/mohsen1/git-lazy-mount
  sudo docker build -t glm-bench-fc -f git-lazy-mount/benchmarks/Dockerfile git-lazy-mount >/dev/null 2>&1
  cid=$(sudo docker create glm-bench-fc)
  truncate -s 40G rootfs.base.ext4; mkfs.ext4 -qF rootfs.base.ext4
  mkdir -p rootmnt; sudo mount -o loop rootfs.base.ext4 rootmnt
  sudo docker export "$cid" | sudo tar -C rootmnt -xf - ; sudo docker rm "$cid" >/dev/null
  sudo mkdir -p rootmnt/bench; sudo cp bench_lazy_fc.sh guest_init.sh ts_prepend.py rootmnt/bench/; sudo chmod +x rootmnt/bench/*
  sudo umount rootmnt
fi
echo "BOOTSTRAP_OK fuse=$(grep -c CONFIG_FUSE_FS=y linux-*/.config 2>/dev/null) kernel=$(stat -c%s vmlinux) rootfs=$(stat -c%s rootfs.base.ext4)"
