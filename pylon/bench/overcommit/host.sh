#!/usr/bin/env bash
# Copyright © 2026 CMKL-APEX
#
# SPDX-License-Identifier: Apache-2.0
#
# One run of the overcommit benchmark, on the test host, from a directory
# that holds this script, guest.sh, pylon-overcommit-load, the VMM and, for
# Pylon, pylond and pylon-guest-agent. Boots two VMs on the same host CPUs,
# runs LOAD1 in VM 1 and LOAD2 in VM 2, measures the host CPU each VM's
# threads use during the window the loads measure, then stops everything.
# Results go to result.txt and serial1.log and serial2.log.
#
# Environment (defaults in brackets):
#   SYSTEM          pylon: pylond with scx_pylon manages CPUS and the VMs
#                   use --pylon. upstream: the VMs are pinned to CPUS and
#                   share them under the host's scheduler. dedicated: VM 1
#                   is pinned to the first of CPUS and VM 2 to the rest, the
#                   split Pylon gives two VMs that both want every CPU. [pylon]
#   LOAD1, LOAD2    the load and its options, as for pylon-overcommit-load
#   CPUS            host CPUs the VMs run on [2-4]
#   IMAGE           guest disk image (a cloud image cloud-init boots)
#   FIRMWARE        firmware to boot it with (rust-hypervisor-firmware)
#   VCPUS, MEMORY   per VM [4, 2G]
#   PYLON1, PYLON2  --pylon policy per VM
#                   [initial_active_vcpus=1,min_cores=1,max_cores=3,priority_weight=100]
#   PYLOND_ARGS     more pylond options, such as --engine-cpu
#   STOP_SERVICE    a systemd unit to stop for the run and start after it,
#                   such as another pylond that manages the same CPUs
#   WARMUP, SECONDS_RUN  passed to the load [40, 150]
set -u
D=$(dirname "$(readlink -f "$0")")
cd "$D" || exit 1

SYSTEM=${SYSTEM:-pylon}
CPUS=${CPUS:-2-4}
VCPUS=${VCPUS:-4}
MEMORY=${MEMORY:-2G}
POLICY=initial_active_vcpus=1,min_cores=1,max_cores=3,priority_weight=100
PYLON1=${PYLON1:-$POLICY}
PYLON2=${PYLON2:-$POLICY}
WARMUP=${WARMUP:-40}
SECONDS_RUN=${SECONDS_RUN:-150}
VM_PIDS=()
PYLOND_PID=

# CPUS as a list: 2-4,8 is 2 3 4 8.
cpu_list() {
    local r
    for r in ${1//,/ }; do seq "${r%-*}" "${r#*-}"; done
}
read -r -a ALL_CPUS <<<"$(cpu_list "$CPUS" | tr '\n' ' ')"
VM_CPUS=("$CPUS" "$CPUS")
if [ "$SYSTEM" = dedicated ]; then
    rest=("${ALL_CPUS[@]:1}")
    VM_CPUS=("${ALL_CPUS[0]}" "$(
        IFS=,
        echo "${rest[*]}"
    )")
fi

log() { echo "[$(date +%T)] $*" | tee -a result.log; }
result() { echo "$*" >>result.txt; }

cleanup() {
    for p in "${VM_PIDS[@]}"; do sudo kill "$p" 2>/dev/null; done
    sleep 2
    [ -n "$PYLOND_PID" ] && sudo kill -TERM "$PYLOND_PID" 2>/dev/null && sleep 2
    [ -n "${STOP_SERVICE:-}" ] && sudo systemctl start "$STOP_SERVICE"
    log "restored: sched_ext $(cat /sys/kernel/sched_ext/state 2>/dev/null || echo absent)" \
        "${STOP_SERVICE:+, $STOP_SERVICE $(systemctl is-active "$STOP_SERVICE")}"
    sudo chmod a+r serial*.log 2>/dev/null
    sudo rm -f disk1.raw disk2.raw
    touch finished
}
trap cleanup EXIT

# The CPU time of every thread of the given processes, in nanoseconds.
cpu_ns() {
    for p in "$@"; do sudo cat /proc/"$p"/task/*/schedstat; done | awk '{s += $1} END {printf "%d", s}'
}

rm -f result.txt result.log finished
result "system=$SYSTEM cpus=${VM_CPUS[0]}/${VM_CPUS[1]} vcpus=$VCPUS load1=${LOAD1// /_} load2=${LOAD2// /_}"
for v in 1 2; do
    qemu-img convert -O raw "$IMAGE" disk$v.raw && truncate -s 8G disk$v.raw
    load=LOAD$v
    echo "${!load} --warmup $WARMUP --seconds $SECONDS_RUN" >load
    printf '#cloud-config\nruncmd:\n  - [sh, -c, "mkdir -p /mnt/seed && mount -L CIDATA /mnt/seed && sh /mnt/seed/guest.sh"]\n' >user-data
    printf 'instance-id: pylon-bench-%s\nlocal-hostname: pylon-bench-%s\n' $v $v >meta-data
    files=(user-data meta-data load guest.sh pylon-overcommit-load)
    [ "$SYSTEM" = pylon ] && files+=(pylon-guest-agent)
    rm -f seed$v.img
    mkfs.vfat -n CIDATA -C seed$v.img 16384 >/dev/null
    mcopy -i seed$v.img "${files[@]}" ::
done

[ -n "${STOP_SERVICE:-}" ] && sudo systemctl stop "$STOP_SERVICE"
if [ "$SYSTEM" = pylon ]; then
    # The logs belong to the user, so the redirects are meant to run without sudo.
    # shellcheck disable=SC2086,SC2024 # PYLOND_ARGS is a list of options
    sudo env RUST_LOG=info "$D/pylond" --socket "$D/pylond.sock" --cores "$CPUS" ${PYLOND_ARGS:-} \
        >pylond.log 2>&1 &
    for _ in $(seq 30); do
        grep -qE "scx_pylon attached|not attached|Error" pylond.log && break
        sleep 1
    done
    grep -q "scx_pylon attached successfully" pylond.log || {
        log "ABORT: scx_pylon not attached"
        exit 1
    }
    PYLOND_PID=$(pgrep -f "^$D/pylond")
fi
for v in 1 2; do
    # No core scheduling, as in the earlier real-guest runs: it would keep the
    # other VM off a CPU's SMT sibling, which is outside CPUS anyway.
    # shellcheck disable=SC2054 # the commas belong to the options
    vm=(--firmware "$FIRMWARE" --memory "size=$MEMORY" --cpus "boot=$VCPUS,core_scheduling=off"
        --disk "path=disk$v.raw,image_type=raw" "path=seed$v.img,image_type=raw"
        --serial file="$D/serial$v.log" --console off)
    if [ "$SYSTEM" = pylon ]; then
        policy=PYLON$v
        # shellcheck disable=SC2024
        sudo "$D/cloud-hypervisor" "${vm[@]}" \
            --pylon socket="$D/pylond.sock",vm_id=$v,"${!policy}" >ch$v.log 2>&1 &
    else
        # shellcheck disable=SC2024
        sudo taskset -c "${VM_CPUS[v - 1]}" "$D/cloud-hypervisor" "${vm[@]}" >ch$v.log 2>&1 &
    fi
done
sleep 1
for v in 1 2; do VM_PIDS+=("$(pgrep -f "^$D/cloud-hypervisor.*disk$v.raw")"); done
log "$SYSTEM: VMs ${VM_PIDS[*]}, pylond ${PYLOND_PID:-none}"

started() { sudo grep -ac "PYLONBENCH load start" serial1.log serial2.log 2>/dev/null | grep -c ':1$'; }
for _ in $(seq 150); do
    [ "$(started)" = 2 ] && break
    sleep 1
done
[ "$(started)" = 2 ] || {
    log "ABORT: the loads didn't start"
    exit 1
}
log "both loads started"

# Host CPU over the loads' window, less 10 s at the end for a late start.
sleep "$WARMUP"
t0=$(date +%s%N)
c0=()
for p in "${VM_PIDS[@]}" $PYLOND_PID; do c0+=("$(cpu_ns "$p")"); done
sleep $((SECONDS_RUN - WARMUP - 10))
t1=$(date +%s%N)
c1=()
for p in "${VM_PIDS[@]}" $PYLOND_PID; do c1+=("$(cpu_ns "$p")"); done
cpus() { awk -v a="$1" -v b="$2" -v t=$((t1 - t0)) 'BEGIN {printf "%.2f", (b - a) / t}'; }
result "cpu_vm1=$(cpus "${c0[0]}" "${c1[0]}") cpu_vm2=$(cpus "${c0[1]}" "${c1[1]}")" \
    "${PYLOND_PID:+cpu_pylond=$(cpus "${c0[2]}" "${c1[2]}")}"

for _ in $(seq 60); do
    sudo grep -aq "PYLONBENCH done" serial1.log && sudo grep -aq "PYLONBENCH done" serial2.log && break
    sleep 2
done
result "lockups=$(sudo cat serial1.log serial2.log | grep -aciE 'soft lockup|rcu.*stall|hung task')"
log "finished: $(tail -2 result.txt | tr '\n' ' ')"
