# Overcommit benchmark: Pylon against upstream Cloud Hypervisor

Two 4-vCPU VMs run on three host CPUs, under three systems:

- `pylon`: `pylond` with `scx_pylon` manages the three CPUs.
- `upstream`: unmodified Cloud Hypervisor, both VMs pinned to the three
  CPUs and sharing them under the host's scheduler.
- `dedicated`: unmodified Cloud Hypervisor, VM 1 pinned to one CPU and
  VM 2 to the other two. This is the split Pylon gives two VMs that both
  want every CPU, held still, and a VM with a CPU of its own.

Eight vCPUs on three CPUs: the host is overcommitted, which is where
Pylon is meant to help.

## Why Pylon could win

Under the host's scheduler, the vCPUs of both VMs take turns on the same
CPUs, a few milliseconds at a time. A guest thread that holds a lock, or
that the others wait for at a barrier, can be descheduled for that long
while the other vCPUs of its VM spin. Pylon gives each core to one vCPU
of one VM: the other VM's vCPUs never take turns on it, and a VM's own
extra vCPUs share its cores in 100 µs turns.

Pylon also has costs: `pylond`'s engine polls on a CPU of its own, a core
moving between VMs takes about 100 µs, and a vCPU that has no core waits
for a turn. `dedicated` has neither Pylon's costs nor its sharing, so
it separates what the split does from what Pylon's mechanism does.

## Scenarios

| Scenario | VM 1 | VM 2 | Expected |
|---|---|---|---|
| `independent` | `count`, 4 threads | the same | Upstream at least as fast: nothing to gain, and Pylon pays its handoffs. The control. |
| `spinlock` | `spinlock`, 4 threads | the same | Pylon faster, if lock-holder preemption across VMs costs more than its handoffs. |
| `barrier` | `barrier`, 4 threads | the same | Pylon faster, if a thread descheduled for a host time slice stalls the round. |
| `pipes` | `pipes`, 4 pairs | the same | Not known: every step wakes a thread, often on another vCPU. |
| `noisy` | `pingpong` | `count`, 4 threads | Not known: VM 1's round-trip time next to a VM that wants every CPU. |

The loads are in `workload.c`; each runs 150 s and measures from 40 s on.
The spinning loads don't fall back to sleeping, so they show the worst
case a preempted thread can cause. The guests don't load `pv-pylon`, so
every revocation is forced, and nothing tells the guest which vCPUs have
a core.

## What it measures

- Work per second for each VM, over the window.
- The host CPU the VMs' threads use over the window. Pylon should use the
  three CPUs; `pylond` is shown apart, since its engine takes a fourth.
- Each system's work against `upstream`'s, per VM, averaged over the two
  VMs.
- For `noisy`, VM 1's round trip, p50 / p99 / p99.9 in µs.

## Running it

The test host needs KVM, `sched_ext` (Linux 6.12 or later), `sudo`,
`qemu-img` and `mtools`. The driver needs `cc` able to link a static
binary, `ssh` and `python3`.

```bash
BENCH_HOST=test-host \
PYLON_VMM=target/release/cloud-hypervisor PYLOND=target/release/pylond \
AGENT=target/release/pylon-guest-agent UPSTREAM_VMM=/path/to/upstream/cloud-hypervisor \
IMAGE=/path/on/host/ubuntu-cloudimg-amd64.img FIRMWARE=/path/on/host/CLOUDHV.fd \
pylon/bench/overcommit/run.sh
```

`REPS` (2), `SCENARIOS` and `SYSTEMS` (`pylon,upstream,dedicated`) choose
the runs; `run.sh` and `host.sh` list the other settings. Each run takes
about five minutes. Results go to `./results/<scenario>/<system>-<run>/`,
and `summarize.py` prints the table again.
