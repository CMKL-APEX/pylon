#!/bin/bash
# Usage: sudo run.sh <pylond binary> <out dir> <seconds>. Two churn-load VMs under pylond with
# scx_pylon; gap.sh watches for vCPUs that stay runnable without progress.
set -u; bin=$1; out=$2; secs=$3; cd "$(dirname "$0")"; mkdir -p "$out"
[ "$(cat /sys/kernel/sched_ext/state)" = disabled ] || { echo "sched_ext busy" >&2; exit 1; }
rm -f /run/pylon/vm*.api /dev/shm/vm*-pylon /run/pylon/pylond.sock
for i in 1 2; do cp --sparse=always "${IMG:-churn.raw}" vm$i.raw; done
echo > /sys/kernel/tracing/trace; echo 64000 > /sys/kernel/tracing/buffer_size_kb
echo 1 > /sys/kernel/tracing/events/sched_ext/sched_ext_dump/enable; echo 1 > /proc/sys/kernel/sysrq
RUST_LOG=info "$bin" --socket /run/pylon/pylond.sock --cores 2-5 --engine-cpu 1 --enable-scx > "$out/pylond.log" 2>&1 &
pyl=$!
until [ -S /run/pylon/pylond.sock ]; do kill -0 $pyl 2>/dev/null || { echo "pylond exited" >&2; exit 1; }; sleep 0.5; done
for i in 1 2; do
  PYLON_PARK_IN_KVM=1 "${PYLON_SRC:?}"/target/release/cloud-hypervisor --kernel vmlinuz --initramfs initrd.img \
    --disk path=vm$i.raw,image_type=raw --cmdline "console=ttyS0 root=/dev/vda1 rw p1period=$((3+2*i))" \
    --cpus boot=4 --memory size=2G --serial file="$out/serial$i.log" --console off --api-socket /run/pylon/vm$i.api \
    --pylon shm=/dev/shm/vm$i-pylon,socket=/run/pylon/pylond.sock,vm_id=$i,initial_active_vcpus=1,min_cores=1,max_cores=4 \
    > "$out/vmm$i.log" 2>&1 &
done
sleep 2; ./gap.sh "$secs" > "$out/gap.txt" 2>&1
echo "END $(date +%T) scx=$(cat /sys/kernel/sched_ext/state)" >> "$out/gap.txt"
cat /sys/kernel/tracing/trace > "$out/dump.txt"; dmesg > "$out/dmesg.txt"
pkill -x cloud-hyperviso; sleep 2; kill -TERM $pyl 2>/dev/null; sleep 3; kill -KILL $pyl 2>/dev/null; wait $pyl
for _ in $(seq 20); do [ "$(cat /sys/kernel/sched_ext/state)" = disabled ] && break; sleep 0.5; done
echo 0 > /sys/kernel/tracing/events/sched_ext/sched_ext_dump/enable; echo 1408 > /sys/kernel/tracing/buffer_size_kb
echo "$(cat sysrq.orig)" > /proc/sys/kernel/sysrq
rm -f /run/pylon/vm*.api /dev/shm/vm*-pylon /run/pylon/pylond.sock; chown -R "${SUDO_USER:-root}" "$out"
