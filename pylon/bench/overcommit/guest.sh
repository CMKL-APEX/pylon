#!/bin/sh
# Copyright © 2026 CMKL-APEX
#
# SPDX-License-Identifier: Apache-2.0
#
# Runs in the guest from the seed disk: starts pylon-guest-agent when the
# seed has one, waits for the guest to settle, then runs the load named in
# the seed's `load` file. Every line goes to the serial console with a
# PYLONBENCH prefix, which host.sh waits for and summarize.py reads.
seed=/mnt/seed
out() { echo "PYLONBENCH $*" >/dev/ttyS0; }

if [ -f "$seed/pylon-guest-agent" ]; then
    cp "$seed/pylon-guest-agent" /usr/local/bin/ && chmod +x /usr/local/bin/pylon-guest-agent
    nohup /usr/local/bin/pylon-guest-agent --interval-us 1000 >/var/log/pylon-guest-agent.log 2>&1 &
fi
cp "$seed/pylon-overcommit-load" /usr/local/bin/ && chmod +x /usr/local/bin/pylon-overcommit-load
read -r load args <"$seed/load"
out "boot nproc=$(nproc) kernel=$(uname -r) load=$load $args"
sleep 15
out "load start"
# shellcheck disable=SC2086 # $args is a list of options
/usr/local/bin/pylon-overcommit-load "$load" $args 2>&1 | while read -r line; do out "$line"; done
out "done"
