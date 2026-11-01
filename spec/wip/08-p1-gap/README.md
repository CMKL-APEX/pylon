# WIP: spec 08 P1, the 14 s gap (2026-11-01)

Work in progress, saved because the test host's root filesystem went
read-only (ext4 bad block bitmap checksum on the host's root disk,
journal aborted) in the middle of the test batch. Not for merging as is.

## Setup

Same as spec 08 §7.2: 64-vCPU AMD EPYC host (nested KVM, no SMT),
`pylond --cores 2-5 --engine-cpu 1 --enable-scx`, two 4-vCPU VMs with
`PYLON_PARK_IN_KVM=1`, `initial_active_vcpus=1, min_cores=1, max_cores=4`.
`run.sh` runs one test (set `PYLON_SRC` to the checkout for
`cloud-hypervisor`); `gap.sh` samples every vCPU each second and triggers
a sched_ext dump (SysRq-D) when one stays `R` for 3 s with no CPU ticks.

## Runs

- run8 (30 min, steady load, debug counters): no gap.
- run9 (10 min, churn load: guest CPUs 1-3 busy/idle every 5-7 s, so
  cores keep moving): no stall of 2 s or more.
- run10-base-1 (3 min, steady all-CPU load, baseline 65a4648b8):
  **reproduced**. VM 1 vcpu2 stayed `R` with no ticks for 6 s, parked
  bit set, woken bit clear, while VM 1 owned CPUs 2 and 4. Runs 2-5 never
  started (disk went read-only).

## What the dump shows

See `run10-base-1-dump-excerpt.txt`. The stuck vCPU is on CPU 4's
local DSQ (`dsq_id=0x8000000000000002`), `no_mig=1`, `cpus=...10`
(CPU 4 only), stack ending in `asm_sysvec_apic_timer_interrupt`: a timer
interrupt preempted it while migration was disabled. CPU 4 runs its
owner, vcpu3 of the same VM, which is busy and doesn't block.

`enqueue` takes its migration-disabled branch before any vCPU logic, so
the task goes straight to the local DSQ and never gets a woken bit.

## Consequences

- The `vcpu_blocked_on` kick change (first commit on this branch) is on
  the vCPU-queue paths, not this one. It doesn't fix the gap. Keep it
  only as a separate cleanup, or drop it. Not validated: no clippy,
  fmt or unit-test run, no host run.
- Open: the owner's 20 ms slice should end and the local DSQ should run
  before `dispatch` is asked, so vcpu2 should get the CPU. Either the
  owner keeps the CPU past its slice, or vcpu2 does run, in bursts too
  short for tick-based `utime+stime`.

## Next

1. In `gap.sh`, sample `/proc/<tid>/schedstat` (precise runtime) and
   `nonvoluntary_ctxt_switches`, and the stuck thread's kernel stack.
2. Rerun five 3-minute steady-load runs on the baseline.
3. If vcpu2 really gets no CPU: find why the owner isn't descheduled at
   slice end (slice refill?). If it runs in bursts: find what keeps it
   in a migration-disabled region for seconds.

## Test-harness notes

- Stop pylond by PID: `pkill -x pylond` doesn't match `pylond-base`,
  which once left scx_pylon attached for 2 hours.
- `run.sh` early exits don't yet restore sysrq and tracing.
