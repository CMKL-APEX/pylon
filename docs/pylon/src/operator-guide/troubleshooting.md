# Troubleshooting

## A VM doesn't show up in pylond's stats

Look for these lines in the VMM's log:

| VMM log | Cause | Fix |
|---|---|---|
| `Pylon: pylond socket ... doesn't exist; VM ... runs unmanaged until it appears` | `pylond` wasn't running when the VM started. | Start `pylond`: the VM registers once the socket appears. |
| `Pylon: pylond closed the session of VM ...` | `pylond` stopped, or removed the VM. | Start `pylond` again: the VM registers once a new socket appears. |
| `Pylon: failed to register VM ... with pylond` with *Permission denied* | The VMM's user can't write to the socket. | Start `pylond` with `--socket-group` and `--socket-mode` ([Running pylond](pylond.md#the-control-socket)). |
| neither | `socket=` is missing from `--pylon`. | Add it. |

Also check that two VMs don't share a `vm_id`. `pylond` rejects the
second while the first VM's VMM runs, and logs `Rejected registration
of VM <id>: ... a running VMM already registered this vm_id`. The VMM
doesn't see the rejection, so look in `pylond`'s log.

## Parked vCPUs never wake

`pylond` wakes a parked vCPU only when its VM reports demand above the
cores it already has. Check `runnable_threads` in the stats:

- **Always `0`, even while the guest is busy:** nothing in the guest
  writes demand. Run [`pylon-guest-agent`](../user-guide/guest.md) in
  the guest. (The agent doesn't count itself, so an idle guest
  reports `0`.)
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
wait until `pylond` wakes the vCPU. Keep `initial_active_vcpus` high
enough for the guest's normal load. This is a known open question in
the design (spec 06, Q2). [Parking inside KVM](pylond.md#vms-that-park-inside-kvm),
a prototype, lets an interrupt wake a parked vCPU instead.

One case to avoid in particular: a process that has to keep running,
such as the guest agent, sitting on a parked vCPU. If it is the only
runnable thread, `pylond` sees no demand and never wakes that vCPU. The
guest agent pins itself to vCPU 0 (`--cpu`), which never parks; give any
other must-run process a CPU among the initial vCPUs too.

## Forced preemptions are high

That's expected for now. A guest without the `pv-pylon` module never
parks cooperatively, and with it the cooperative park is still slower
than the 8 µs grace period
([Status](../status.md#what-doesnt-work-yet)). `pylond` then signals the
donor's vCPU thread and the VMM parks it.

## Handoffs without park are above zero

`pylond` gave a core away because the donor hadn't parked 50 µs after
it was first signalled. `pylond` keeps signalling that donor until it
parks, at intervals that double up to 1 ms. Check:

- **The VMM is a version from before `kick_signal`.** Such a VMM
  registers without one, so `pylond` can't signal it. Upgrade it.
- **`pylond` can't signal the VMM.** It needs the same user as the VMM,
  or `CAP_KILL`.
- **The vCPU thread is stuck outside `KVM_RUN`**, for example in a slow
  device exit. The donor parks at its next exit; look at the VMM's log.
- **The VM parks inside KVM and the guest has interrupts disabled**
  (x86_64). The VMM doesn't park such a vCPU until the guest enables
  them again.

## vCPUs beyond `initial_active_vcpus` never park

The VMM starts every vCPU so the guest can bring it online. `pylond`
asks it to park the extra ones only once `pylon-guest-agent` has
written its first heartbeat, and then logs `VM <id>: asked <n> booted
vCPU(s) to park`. If that line never appears, start the agent in the
guest. `pylon-guest-agent --status` shows the heartbeat.

## A VM that parks inside KVM

Check the VMM's log for `Pylon: registered VM <id> with pylond, parking
vCPUs inside KVM`. If it says `in the VMM` instead, the VM was started
with `park=vmm`, or on a host where that is the default (no
`/dev/kvm`). If the line is missing, the VM didn't register.

- **`Rejected registration of VM <id>: ... needs scx_pylon` in
  `pylond`'s log:** `pylond` runs without `scx_pylon`. Attach it, or
  start the VM with `--pylon park=vmm`.
- **`--pylon park=kvm needs KVM; use park=vmm`:** the VMM runs on MSHV,
  which has no halted state to park in.

- **`woken_shared_microseconds` and `woken_unowned_microseconds` stay at
  `0` under load:** `scx_pylon` isn't attached, so woken vCPUs run on
  any CPU and `pylond` doesn't see them ([below](#scx_pylon-isnt-attached)).
- **`Pylon: failed to turn halt polling off`:** parked vCPUs may poll
  in KVM and use host CPU.
- **A parked vCPU runs while `pylond` shows no core for it:** expected.
  KVM runs a parked vCPU when an interrupt arrives, and a vCPU parked by
  the guest or a revocation runs again after 4 ms. Its state stays
  parked until `pylond` gives it a core.

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
core scheduling. A VM with `--pylon` runs without it and only logs a
warning, so this is a VM without `--pylon`, or a Cloud Hypervisor older
than Pylon 0.0.8. Start it with `--cpus ...,core_scheduling=off`
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
