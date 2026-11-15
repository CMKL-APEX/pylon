#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX
#
# Runs in the guest at boot (bench.service): the role comes from params.json.
mkdir -p /job/out
exec > >(tee -a /job/out/job.log) 2>&1
date +%s.%N
nproc
free -g | sed -n 2p
nvidia-smi --query-gpu=name,driver_version,pci.bus_id,memory.total --format=csv || true
nvidia-smi -q | grep -iA4 "fabric" || true
role=$(/opt/venv/bin/python -c 'import json;print(json.load(open("/job/params.json"))["role"])')
case "$role" in
agent) /opt/venv/bin/python /job/agent.py ;;
trainer) /opt/venv/bin/python /job/train.py ;;
smoke) /opt/venv/bin/python -c 'import torch,time;x=torch.randn(8192,8192,device="cuda",dtype=torch.bfloat16);torch.cuda.synchronize();t=time.time();[x@x for _ in range(50)];torch.cuda.synchronize();dt=time.time()-t;print("bf16 TFLOPS",50*2*8192**3/dt/1e12)' ;;
esac
echo "role $role exit $?"
date +%s.%N
