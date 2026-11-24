#!/usr/bin/env bash
# Copyright © 2026 CMKL-APEX
#
# SPDX-License-Identifier: Apache-2.0
#
# Runs the overcommit benchmark on a test host over ssh: every scenario,
# with Pylon and with upstream Cloud Hypervisor (sharing the CPUs, and with
# a CPU per VM), REPS times, one run at a
# time, then prints the summary. See README.md.
#
# Environment (defaults in brackets):
#   BENCH_HOST     ssh destination of the test host (needs sudo, KVM,
#                  sched_ext, qemu-img and mtools)
#   PYLON_VMM      Pylon's cloud-hypervisor binary
#   UPSTREAM_VMM   upstream cloud-hypervisor binary
#   PYLOND         pylond binary
#   AGENT          pylon-guest-agent binary
#   IMAGE          guest image, a path on the test host
#   FIRMWARE       firmware, a path on the test host
#   OUT            local directory for the results [./results]
#   REPS           runs of each scenario and system [2]
#   SCENARIOS      comma-separated, from the list below [all]
#   SYSTEMS        comma-separated: pylon, upstream, dedicated (see host.sh)
#                  [pylon,upstream,dedicated]
#   CPUS, VCPUS, MEMORY, PYLON1, PYLON2, PYLOND_ARGS, STOP_SERVICE,
#   WARMUP, SECONDS_RUN  passed to host.sh
set -eu
H=$(dirname "$(readlink -f "$0")")

# name|VM 1's load|VM 2's load
ALL_SCENARIOS=(
    "independent|count --threads 4|count --threads 4"
    "spinlock|spinlock --threads 4|spinlock --threads 4"
    "barrier|barrier --threads 4|barrier --threads 4"
    "pipes|pipes --threads 4|pipes --threads 4"
    "noisy|pingpong|count --threads 4"
)

OUT=${OUT:-results}
REPS=${REPS:-2}
SYSTEMS=${SYSTEMS:-pylon,upstream,dedicated}
: "${BENCH_HOST:?}" "${IMAGE:?}" "${FIRMWARE:?}"
PASS=(CPUS VCPUS MEMORY PYLON1 PYLON2 PYLOND_ARGS STOP_SERVICE WARMUP SECONDS_RUN
    REQUIRE_SCX CORE_SCHED VCPU_SLICE_NS METRICS_PORT HOST_LOAD)

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cc -O2 -static -pthread -o "$stage/pylon-overcommit-load" "$H/workload.c"
cc -O2 -static -o "$stage/set_slice" "$H/../spec11/set_slice.c"

run_one() { # scenario system rep load1 load2
    local dir="$OUT/$1/$2-$3" vmm env=() v remote
    if [ "$2" = pylon ]; then
        vmm=${PYLON_VMM:?}
    else
        vmm=${UPSTREAM_VMM:?}
    fi
    rm -rf "${stage:?}/run" && mkdir "$stage/run"
    cp "$H/host.sh" "$H/guest.sh" "$stage/pylon-overcommit-load" "$stage/set_slice" "$stage/run/"
    cp "$vmm" "$stage/run/cloud-hypervisor"
    [ "$2" = pylon ] && cp "${PYLOND:?}" "$stage/run/pylond" && cp "${AGENT:?}" "$stage/run/pylon-guest-agent"
    env=("SYSTEM=$2" "IMAGE=$IMAGE" "FIRMWARE=$FIRMWARE" "LOAD1=$4" "LOAD2=$5")
    for v in "${PASS[@]}"; do [ -n "${!v:-}" ] && env+=("$v=${!v}"); done
    remote=$(ssh -o BatchMode=yes "$BENCH_HOST" mktemp -d)
    scp -q -o BatchMode=yes "$stage"/run/* "$BENCH_HOST:$remote/"
    echo "[$(date +%T)] $1 $2 run $3"
    # Detached, so a dropped connection doesn't stop the run; host.sh writes `finished` when it ends.
    # shellcheck disable=SC2029 # expanded here on purpose
    ssh -o BatchMode=yes -f "$BENCH_HOST" \
        "cd $remote && nohup $(printf '%q ' env "${env[@]}") ./host.sh >host.out 2>&1 </dev/null &"
    for _ in $(seq 90); do
        sleep 10
        # shellcheck disable=SC2029
        ssh -o BatchMode=yes "$BENCH_HOST" "test -f $remote/finished" && break
    done
    mkdir -p "$dir"
    scp -q -o BatchMode=yes "$BENCH_HOST:$remote"/{result.txt,result.log,host.out,serial1.log,serial2.log,ch1.log,ch2.log} "$dir/" || true
    scp -q -o BatchMode=yes "$BENCH_HOST:$remote/pylond.log" "$dir/" 2>/dev/null || true
    scp -q -o BatchMode=yes "$BENCH_HOST:$remote/metrics.txt" "$dir/" 2>/dev/null || true
    ssh -o BatchMode=yes "$BENCH_HOST" "sudo rm -rf $remote"
    grep -h "restored\|ABORT" "$dir/result.log" || echo "  no log: see the test host"
}

for rep in $(seq "$REPS"); do
    for s in "${ALL_SCENARIOS[@]}"; do
        IFS='|' read -r name load1 load2 <<<"$s"
        [ -n "${SCENARIOS:-}" ] && [[ ",$SCENARIOS," != *",$name,"* ]] && continue
        # Alternate which system goes first, so drift on the host doesn't favour one.
        systems=${SYSTEMS//,/ }
        [ $((rep % 2)) = 0 ] && systems=$(echo "$systems" | tr ' ' '\n' | tac | tr '\n' ' ')
        for system in $systems; do
            run_one "$name" "$system" "$rep" "$load1" "$load2"
        done
    done
done
python3 "$H/summarize.py" "$OUT"
