#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX
#
# Builds the guest OS image for the agentic-inference / fine-tuning
# benchmark: Ubuntu 24.04 cloud image + NVIDIA 580 (open) driver + a
# Python venv with vLLM and PEFT, plus the CPU-tool corpora. Run as root.
set -euxo pipefail
W=$(cd "$(dirname "$0")" && pwd)/img
mkdir -p "$W"
cd "$W"
MNT=$W/mnt
[ -f noble.img ] || curl -fsSLo noble.img \
    https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img
rm -f base.raw
qemu-img convert -O raw noble.img base.raw
truncate -s 60G base.raw
LOOP=$(losetup -fP --show base.raw)
cleanup() {
    set +e
    for m in dev/pts dev proc sys run boot/efi boot ""; do
        mountpoint -q "$MNT/$m" && umount -l "$MNT/$m"
    done
    losetup -d "$LOOP"
}
trap cleanup EXIT
sgdisk -e "$LOOP"
parted -s "$LOOP" resizepart 1 100%
partprobe "$LOOP"
e2fsck -fy "${LOOP}p1" || true
resize2fs "${LOOP}p1"
mkdir -p "$MNT"
mount "${LOOP}p1" "$MNT"
[ -b "${LOOP}p16" ] && mount "${LOOP}p16" "$MNT/boot"
mount "${LOOP}p15" "$MNT/boot/efi"
for m in dev dev/pts proc sys; do mount --bind /$m "$MNT/$m"; done
mount -t tmpfs tmpfs "$MNT/run"
mv "$MNT/etc/resolv.conf" "$MNT/etc/resolv.conf.orig"
cp -L /etc/resolv.conf "$MNT/etc/resolv.conf"

cat >"$MNT/tmp/inside.sh" <<'EOF'
set -euxo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y linux-generic
KVER=$(ls /lib/modules | sort -V | tail -1)
apt-get install -y "linux-modules-nvidia-580-server-open-$KVER" \
    nvidia-utils-580-server libnvidia-compute-580-server \
    python3-venv python3-dev build-essential git ripgrep pkg-config \
    sysstat numactl linux-tools-common "linux-tools-$KVER" unzip
python3 -m venv /opt/venv
/opt/venv/bin/pip install --upgrade pip
/opt/venv/bin/pip install vllm
/opt/venv/bin/pip install peft trl datasets accelerate pandas pytest \
    openai aiohttp
/opt/venv/bin/pip freeze > /opt/venv-freeze.txt
# CPU tool corpora (the agent's tools work on these).
mkdir -p /corpus
git clone --depth 1 https://github.com/pallets/click /corpus/click
/opt/venv/bin/pip install -e /corpus/click
git clone --depth 1 https://github.com/python/cpython /corpus/cpython
curl -fsSLo /tmp/sqlite.zip https://www.sqlite.org/2024/sqlite-amalgamation-3460100.zip
unzip -o /tmp/sqlite.zip -d /corpus && mv /corpus/sqlite-amalgamation-3460100 /corpus/sqlite
# Guest setup: no cloud-init, root autologin on the serial console,
# and a unit that runs /job/run.sh from the job disk, then powers off
# unless /job/keep exists.
touch /etc/cloud/cloud-init.disabled
echo bench > /etc/hostname
mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d /job /models
cat > /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf <<'U'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear %I 115200 linux
U
cat >> /etc/fstab <<'U'
LABEL=JOB /job ext4 defaults,nofail 0 2
LABEL=MODELS /models ext4 ro,nofail 0 2
U
cat > /etc/systemd/system/bench.service <<'U'
[Unit]
Description=Benchmark job from the job disk
After=local-fs.target
ConditionPathExists=/job/run.sh
[Service]
Type=oneshot
ExecStart=/bin/bash -c '/bin/bash /job/run.sh > /job/out/run.log 2>&1; sync; [ -e /job/keep ] || systemctl poweroff'
StandardOutput=journal+console
[Install]
WantedBy=multi-user.target
U
systemctl enable bench.service
systemctl disable snapd.service snapd.socket unattended-upgrades.service apt-daily.timer apt-daily-upgrade.timer || true
sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=.*/GRUB_CMDLINE_LINUX_DEFAULT="console=ttyS0"/' /etc/default/grub
rm -f /etc/default/grub.d/50-cloudimg-settings.cfg
update-grub
# Without HMM off, cuInit fails in the VM (CUDA_ERROR_NOT_INITIALIZED)
# on passed-through A100s.
echo "options nvidia_uvm uvm_disable_hmm=1" > /etc/modprobe.d/nvidia-uvm-no-hmm.conf
update-initramfs -u -k all
apt-get clean
EOF
chroot "$MNT" /bin/bash /tmp/inside.sh
mv "$MNT/etc/resolv.conf.orig" "$MNT/etc/resolv.conf"
echo IMAGE-OK
