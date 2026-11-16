#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX
#
# Runs one scenario: an agent VM, a trainer VM, or both, each with one GPU
# passed through, then copies their results to out/NAME/<vm>/.
#
# Usage (root): scenario.sh NAME AGENT_CORES TRAINER_CORES [smoke]
#
# An empty core list means that VM doesn't run. Core lists are host CPU
# lists for taskset. Environment:
#   AGENT_GPU, TRAINER_GPU  PCI addresses of the GPUs (bound to vfio-pci)
#   CH        cloud-hypervisor binary (default: target/release in the repo)
#   FW        guest firmware, CLOUDHV.fd (required)
#   NUMA      host NUMA node for guest memory (default 0)
#   WARM, DUR seconds of warm-up and of measurement (240, 300)
#   VCPUS     vCPUs per VM (16)
#   PARAMS    JSON merged into the job's params.json, e.g.
#             '{"agent":{"tools":"heavy"}}'
set -euo pipefail
W=$(cd "$(dirname "$0")" && pwd)
NAME=$1
AC=$2
TC=$3
MODE=${4:-run}
CH=${CH:-$W/../../../target/release/cloud-hypervisor}
FW=${FW:?set FW to the CLOUDHV.fd firmware}
NUMA=${NUMA:-0}
OUT=$W/out/$NAME
rm -rf "$OUT"
mkdir -p "$OUT"
DUR=${DUR:-300}
WARM=${WARM:-240}
VCPUS=${VCPUS:-16}
START=$(($(date +%s) + WARM))
declare -A GPU=([agent]=${AGENT_GPU:-} [trainer]=${TRAINER_GPU:-})
declare -A MEM=([agent]=48G [trainer]=40G)
declare -A CORES=([agent]=$AC [trainer]=$TC)
pids=()
names=()
for vm in agent trainer; do
    [ -n "${CORES[$vm]}" ] || continue
    [ -n "${GPU[$vm]}" ] || {
        echo "set ${vm^^}_GPU" >&2
        exit 1
    }
    JOB=$W/img/job-$vm.raw
    rm -f "$JOB"
    truncate -s 2G "$JOB"
    stage=$(mktemp -d)
    cp -r "$W/job/." "$stage/"
    rm -rf "$stage/__pycache__"
    mkdir -p "$stage/out"
    role=$vm
    [ "$MODE" = smoke ] && role=smoke
    python3 - "$stage/params.json" "$role" "$START" "$DUR" "${PARAMS:-{\}}" <<'PY'
import json, sys
path, role, start, dur, extra = sys.argv[1:]
p = {"role": role, "agent": {"measure_start": float(start), "duration_s": int(dur)},
     "trainer": {"measure_start": float(start), "duration_s": int(dur)}}
for k, v in json.loads(extra).items():
    p.setdefault(k, {}).update(v)
json.dump(p, open(path, "w"), indent=1)
PY
    mkfs.ext4 -q -L JOB -d "$stage" "$JOB"
    rm -rf "$stage"
    taskset -c "${CORES[$vm]}" "$CH" \
        --api-socket "path=$OUT/$vm.sock" \
        --cpus boot="$VCPUS" --memory size=0 \
        --memory-zone "id=m0,size=${MEM[$vm]},host_numa_node=$NUMA" \
        --numa "guest_numa_id=0,cpus=[0-$((VCPUS - 1))],memory_zones=m0" \
        --firmware "$FW" \
        --disk "path=$W/img/$vm.raw,image_type=raw" \
        "path=$W/img/models.raw,readonly=on,image_type=raw" \
        "path=$JOB,image_type=raw" \
        --device "path=/sys/bus/pci/devices/${GPU[$vm]}" \
        --console off --serial "file=$OUT/$vm-serial.log" \
        >"$OUT/$vm-ch.log" 2>&1 &
    pids+=($!)
    names+=("$vm")
    echo "$vm pid $! cores ${CORES[$vm]}"
done
echo "measure window $START + $DUR s"
python3 "$W/hostsample.py" "$OUT/host.jsonl" \
    "$(
        IFS=,
        echo "${pids[*]}"
    )" "$(
        IFS=,
        echo "${names[*]}"
    )" \
    </dev/null >/dev/null 2>&1 &
SP=$!
# A VM that hangs is killed after this long. The timers write to a file,
# not to our stdout, so a caller reading our output isn't held open.
LIMIT=$((WARM + DUR + 900))
timers=()
for i in "${!pids[@]}"; do
    (
        sleep $LIMIT
        kill "${pids[$i]}" 2>/dev/null &&
            echo "killed ${names[$i]} after ${LIMIT}s"
    ) \
        >>"$OUT/timeouts.log" 2>&1 </dev/null &
    timers+=($!)
done
for i in "${!pids[@]}"; do
    wait "${pids[$i]}" || echo "${names[$i]} exited $?"
done
for t in "${timers[@]}"; do
    pkill -P "$t" 2>/dev/null
    kill "$t" 2>/dev/null
done
kill $SP 2>/dev/null || true
for vm in "${names[@]}"; do
    m=$(mktemp -d)
    mount -o ro "$W/img/job-$vm.raw" "$m"
    mkdir -p "$OUT/$vm"
    cp -r "$m/out/." "$OUT/$vm/"
    umount "$m"
    rmdir "$m"
done
[ -n "${SUDO_USER:-}" ] && chown -R "$SUDO_USER:" "$OUT"
echo SCENARIO-DONE
