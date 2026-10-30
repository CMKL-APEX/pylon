# Troubleshooting

## A VM doesn't show up in pylond's stats

Look for these lines in the VMM's log:

| VMM log | Cause | Fix |
|---|---|---|
| `Pylon: daemon socket ... does not exist yet` | `pylond` wasn't running when the VM started. | Start `pylond` first, then restart the VM. VMs register only at start. |
| `Pylon: failed to register VM ... with pylond` with *Permission denied* | The VMM's user can't write to the socket. | Start `pylond` with `--socket-group` and `--socket-mode` ([Running pylond](pylond.md#the-control-socket)). |
| neither | `socket=` is missing from `--pylon`. | Add it. |

Also check that two VMs don't share a `vm_id`. `pylond` rejects the
second while the first VM's VMM runs, and logs `Rejected registration
of VM <id>: ... a running VMM already registered this vm_id`. The VMM
doesn't see the rejection, so look in `pylond`'s log.

## Parked vCPUs never wake

`pylond` wakes a parked vCPU only when its VM reports demand above the
cores it already has. Check `runnable_threads` in the stats:

- **Always `0`:** nothing in the guest writes demand. Run
  [`pylon-guest-agent`](../user-guide/guest.md) in the guest.
- **Above zero, but `current_cores` doesn't grow:** check that the VM's
  `max_cores` is above its current cores, and that a managed CPU is free
  or a donor VM is above its `min_cores`. On a host where every
  managed CPU is taken, a VM waits for a donor with lower urgency.
- **The guest agent fails with an `/dev/mem` error:** the guest kernel
  doesn't allow mapping the page's address through `/dev/mem`, or the
  agent isn't running as root.

## The guest hangs or stalls while some vCPUs are parked

The guest sees parked vCPUs as online. Work queued on them, and
interprocessor interrupts sent to them (for example TLB shootdowns),
wait until `pylond` wakes the vCPU. Until the guest kernel module
exists, keep `initial_active_vcpus` high enough for the guest's normal
load. This is a known open question in the design (spec 06, Q2).

## Forced preemptions are high

That's expected for now: the guest isn't told about revocations, so
donors don't park within the grace period
([Status](../status.md#what-doesnt-work-yet)). `pylond` then signals the
donor's vCPU thread and the VMM parks it.

## Handoffs without park are above zero

`pylond` gave a core away because the donor hadn't parked 50 µs after
its deadline, although it was signalled. Check:

- **The VMM is a version from before `kick_signal`.** Such a VMM
  registers without one, so `pylond` can't signal it. Upgrade it.
- **`pylond` can't signal the VMM.** It needs the same user as the VMM,
  or `CAP_KILL`.
- **The vCPU thread is stuck outside `KVM_RUN`**, for example in a slow
  device exit. The donor parks at its next exit; look at the VMM's log.

## The host feels sluggish after pylond starts

`scx_pylon` is probably attached and scheduling every task on the
host. Check `cat /sys/kernel/sched_ext/root/ops`. Restart `pylond` with
`--no-scx`, or move host work off the managed CPUs
([Kernel scheduling with scx_pylon](sched-ext.md)).

## `scx_pylon` isn't attached

| `pylond` log | Cause |
|---|---|
| no `sched_ext` message at all | The kernel has no `/sys/kernel/sched_ext` (it wasn't built with `CONFIG_SCHED_CLASS_EXT`), or `--no-scx` or `--pcpu-map-file` was given. |
| `scx_pylon not attached, using the in-memory pCPU table: ...` | Attaching failed. The error after the colon says why: commonly missing privileges, or another `sched_ext` scheduler already attached. |
| `... Thread <tid> (<name>) has a core scheduling cookie` | The core scheduling guard found a process using core scheduling, such as a Cloud Hypervisor VM without `core_scheduling=off` ([Core scheduling](sched-ext.md#core-scheduling)). |
| `... BPF LSM is not active` | The guard can't refuse new cookies on this kernel. Boot with `bpf` in `lsm=`, or pass `--core-sched-deny off`. |

If `dmesg` shows `core scheduling turned on by pid <pid>`, a process got
a core scheduling cookie while `scx_pylon` was attached (possible only
with `--core-sched-deny off`), and `scx_pylon` disabled itself.

## A VM fails to start with `Error enabling core scheduling`

`scx_pylon` is attached with the core scheduling guard, which refuses
core scheduling. Start the VM with `--cpus ...,core_scheduling=off`
([Core scheduling](sched-ext.md#core-scheduling)). Upstream Cloud
Hypervisor doesn't fail in this case: it hangs before booting the
guest and stops answering its API.

If the kernel ejected the scheduler later, `dmesg` says why (for
example a runnable task stall). `pylond` keeps running without it.

## A VM's CPUs after it exits

`pylond` notices within about a second that a VM's VMM has exited (it
holds a pidfd for each VMM), logs `VMM of VM <id> exited; removed its
registration`, and frees the CPUs the VM held. A VM registered without
peer credentials can't be tracked this way, but every VMM registering
over the Unix socket has them.

## Collecting information for a bug report

```bash
pylond --version
echo '{"action":"stats"}' | socat - UNIX-CONNECT:/run/pylon/pylond.sock
cat /sys/kernel/sched_ext/state /sys/kernel/sched_ext/root/ops
journalctl -u pylond --since -1h     # or pylond's standard error
```

Remove VM names, hostnames and addresses before you post them in an
issue.
