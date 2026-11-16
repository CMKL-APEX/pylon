#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX
#
# Runs the scenario matrix in turn (root). Log: out/matrix.log.
# FIRST is the first of 16 host CPUs on the GPUs' NUMA node (default
# 112, for 112-127). scenario.sh's environment variables apply.
set -u
cd "$(dirname "$0")" || exit 1
mkdir -p out
F=${FIRST:-112}
c() { echo "$((F + $1))-$((F + $2))"; }
H='{"agent":{"tools":"heavy"}}'
L='{"agent":{"tools":"light"}}'
run() {
    echo "=== $1 $(date -Is)"
    PARAMS="$4" WARM=${WARM:-480} DUR=${DUR:-300} VCPUS=16 \
        bash scenario.sh "$1" "$2" "$3" 2>&1 | tail -2
}
{
    run R1-light-16c "$(c 0 15)" "" "$L"
    run R2-heavy-16c "$(c 0 15)" "" "$H"
    run R3-heavy-8c "$(c 0 7)" "" "$H"
    run R4-heavy-4c "$(c 0 3)" "" "$H"
    run R5-heavy-shared "$(c 0 15)" "$(c 0 15)" "$H"
    run R6-heavy-split "$(c 0 11)" "$(c 12 15)" "$H"
    run R7-train-2c "" "$(c 0 1)" "$H"
    echo "=== MATRIX-DONE $(date -Is)"
} >>out/matrix.log
