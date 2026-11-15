#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX
#
# Builds models.raw (ext4, label MODELS): the models and dataset, attached
# read-only to both benchmark VMs. Downloads as the user, formats as root.
set -euxo pipefail
W=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$W"/img
python3 -m venv "$W"/hostvenv
"$W"/hostvenv/bin/pip install -q huggingface_hub
D="$W"/models-staging
mkdir -p "$D"
for m in Qwen/Qwen2.5-7B-Instruct Qwen/Qwen2.5-1.5B-Instruct; do
    "$W"/hostvenv/bin/hf download "$m" --local-dir "$D/$(basename "$m")" \
        --exclude "original/*"
done
"$W"/hostvenv/bin/hf download tatsu-lab/alpaca --repo-type dataset --local-dir "$D"/alpaca
du -sh "$D"
rm -f "$W"/img/models.raw
truncate -s 40G "$W"/img/models.raw
sudo -n mkfs.ext4 -q -L MODELS -d "$D" "$W"/img/models.raw
echo MODELS-OK
