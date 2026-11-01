#!/bin/bash
# Usage: gap.sh <seconds>. Every 1 s: vCPU state/progress, maps, debug counters.
# When a vCPU stays runnable with no progress for 3 s, trigger a sched_ext dump (SysRq-D), at most 3 times.
end=$(( $(date +%s) + $1 )); declare -A last stuck; dumps=0
while [ $(date +%s) -lt $end ]; do
  echo "T $(date +%T.%N | cut -c1-12) scx=$(cat /sys/kernel/sched_ext/state)"
  for p in $(pgrep -x cloud-hyperviso); do
    vm=$(tr '\0' ' ' < /proc/$p/cmdline | grep -o 'vm_id=[0-9]*' | cut -d= -f2)
    for t in /proc/$p/task/*; do c=$(cat $t/comm 2>/dev/null); [[ $c == vcpu* ]] || continue
      s=($(sed 's/.*) //' $t/stat)); tk=$((s[11]+s[12])); k="vm$vm/$c"; tid=${t##*/}
      if [[ ${s[0]} == R && ${last[$k]} == $tk ]]; then stuck[$k]=$(( ${stuck[$k]:-0} + 1 )); else stuck[$k]=0; fi
      last[$k]=$tk
      echo "vm$vm $c tid=$tid st=${s[0]} cpu=${s[36]} ticks=$tk stuck=${stuck[$k]}"
      if [[ ${stuck[$k]} -eq 3 && $dumps -lt 3 ]]; then dumps=$((dumps+1)); echo "DUMP $dumps for vm$vm $c tid=$tid"; echo D > /proc/sysrq-trigger; fi
    done
  done
  id=$(bpftool map list | awk '/name [0-9a-f]+\.bss/{sub(":","",$1); print $1; exit}')
  echo "MAP vv=$(bpftool map dump name pylon_vm_vcpus -j 2>/dev/null | jq -c '[.[]|select(.formatted.value.parked>0)|.formatted]') own=$(bpftool map dump name pylon_pcpu_owne -j 2>/dev/null | jq -c '[.[]|select(.formatted.value.target_tid>0)|[.formatted.key,.formatted.value.target_tid,(.formatted.value.flags/65536|floor)]]') dbg=$(bpftool map dump id $id -j 2>/dev/null | jq -c '.[0].formatted.value[".bss"][]|select(.dbg)|.dbg')"
  sleep 1
done
