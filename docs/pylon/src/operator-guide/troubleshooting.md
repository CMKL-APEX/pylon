# Troubleshooting

## A VM doesn't show up in pylond's stats

Look for these lines in the VMM's log:

| VMM log | Cause | Fix |
|---|---|---|
| `Pylon: daemon socket ... does not exist yet` | `pylond` wasn't running when the VM started. | Start `pylond` first, then restart the VM. VMs register only at start. |
| `Pylon: failed to register VM ... with pylond` with *Permission denied* | The VMM's user can't write to the socket. | Set the socket's group and mode ([Running pylond](pylond.md#the-control-socket)). |
| neither | `socket=` is missing from `--pylon`. | Add it. |

Also check that two VMs don't share a `vm_id`. The second registration
replaces the first, so only one of them is listed.

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

If the kernel ejected the scheduler later, `dmesg` says why (for
example a runnable task stall). `pylond` keeps running without it.

## Managed CPUs stay assigned after a VM exits

`pylond` isn't told when a VM exits, and may keep CPUs assigned to that
VM's running vCPUs. Restart `pylond` to recover them
([Status](../status.md#what-doesnt-work-yet)).

## Collecting information for a bug report

```bash
pylond --version
echo '{"action":"stats"}' | socat - UNIX-CONNECT:/run/pylon/pylond.sock
cat /sys/kernel/sched_ext/state /sys/kernel/sched_ext/root/ops
journalctl -u pylond --since -1h     # or pylond's standard error
```

Remove VM names, hostnames and addresses before you post them in an
issue.
