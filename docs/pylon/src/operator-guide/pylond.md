# Running pylond

`pylond` is the per-host core arbiter. Run exactly one per host. Every
option is in the [pylond reference](../reference/pylond.md).

## Host CPUs

`pylond` uses three groups of host CPUs:

| CPUs | Used for | Set by |
|---|---|---|
| 0 | **Control plane**: the Unix socket, VM registrations, policy and stats requests, the metrics server. Mostly idle. | Fixed |
| 1 (default) | **Arbitration engine**: polls every VM's shared page about every microsecond. It keeps this CPU fully busy. | `--engine-cpu`, default `1` |
| `--cores` | **Managed CPUs**, given to VMs' vCPUs. | `--cores`, default `2-7` |

`--cores` takes a comma-separated list of CPUs and ranges, such as
`2-7` or `4-15,20-31`.

- **Don't put 0 or the engine CPU in `--cores`.** `pylond` warns about
  the engine CPU but doesn't check CPU 0. Its own threads would then
  compete with VMs on those CPUs.
- The engine thread spins at all times. On x86_64 CPUs with `WAITPKG`
  and aarch64 CPUs with `FEAT_WFxT`, it waits with `TPAUSE` or `WFET`
  between sweeps, which uses less power than spinning; on others, it
  uses `PAUSE` or a spin hint. The engine logs `timed_wait=true` or
  `false` at start, with the CPU it pinned to.
- Once a guest reports it is up (a non-zero `guest_heartbeat`, which
  only `pylon-guest-agent` writes), Plane 1 asks the VMM to park the
  vCPUs beyond `initial_active_vcpus`, signalling each thread; it checks
  once a second and logs `VM <id>: asked <n> booted vCPU(s) to park`.
  Without the agent, those vCPUs keep running.
- The engine is pinned to CPU 1 unless you pass `--engine-cpu`. On a host
  with big and little cores, pick a big one; `pylond` warns if it is also
  in `--cores`.
- Keep other work off the engine CPU, for example with the kernel's
  `isolcpus=` or `nohz_full=` boot options, or a cgroup `cpuset` that
  excludes it. Any interruption of the engine delays every handoff on
  the host.
- If pinning fails, `pylond` logs a warning and keeps running unpinned.

`pylond` doesn't stop other processes from running on the managed CPUs.
Keep VMMs' non-vCPU threads and host services on other CPUs, with
`cpuset`s or `taskset`, or use [`scx_pylon`](sched-ext.md).

## The control socket

`pylond` listens on `--socket` (default `/run/pylon/pylond.sock`).
VMMs register on it, and you use it to change policy and read stats
([Policy and monitoring](monitoring.md)).

At start, `pylond` removes any file already at that path and creates
the socket. `--socket-group` (a group name or GID) and `--socket-mode`
(octal, such as `660`) set its group and permissions; without them it
keeps `pylond`'s umask and group, which as root means only root can
connect. Anyone who can write to the socket can register VMs, change any
VM's policy and read every VM's stats. Give access only to the users
your VMMs run as (see the [systemd unit](installation.md#a-systemd-unit-for-pylond)).

## Start order

Start `pylond` before Pylon VMs. A VMM registers once, when it starts,
and doesn't retry or reconnect. If `pylond` restarts, it forgets every
VM. Running VMs keep their running vCPUs, but parked ones stay parked
until the VMs restart. (In a VM that [parks inside KVM](#vms-that-park-inside-kvm),
an interrupt still runs a parked vCPU, and a vCPU parked by the guest or
a revocation runs again without a core after 4 ms.)

## Stopping

`pylond` has no signal handlers of its own: `SIGTERM` and `SIGINT` end it
immediately. The kernel then detaches `scx_pylon`, if it was attached,
and the host's normal scheduler takes over. vCPUs that were parked stay
parked. The socket file is left behind and removed at the next start.

## VMs that park inside KVM

A VMM started with `PYLON_PARK_IN_KVM=1` in its environment parks
vCPUs halted inside KVM instead of blocked in the VMM, so an interrupt
wakes a parked vCPU. It logs `Pylon: VM <id> parks vCPUs inside KVM`.
This is a prototype ([Status](../status.md#what-doesnt-work-yet),
[spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md)).
`pylond` needs no option for it. What changes on the host:

- **It needs `scx_pylon`.** `scx_pylon` runs a woken vCPU only on CPUs
  its VM owns and reports it to `pylond`, which then gives a core to a
  VM whose vCPU stays woken for 4 ms. Without `scx_pylon`, a woken vCPU
  runs on any CPU and `pylond` doesn't see it
  ([Kernel scheduling with scx_pylon](sched-ext.md)).
- **Halt polling is off for that VM.** The VMM sets the VM's halt
  polling limit to 0, so a parked vCPU costs no host CPU. It logs
  `Pylon: failed to turn halt polling off` if it can't.
- **Parks are bounded.** A vCPU that the guest or a revocation parked
  runs again, still parked and without a core, after 4 ms. Boot parks
  have no bound.
- **Re-signals back off**, for every VM. A donor that doesn't park is
  signalled again at intervals that double from 10 µs up to 1 ms. In a
  VM that parks inside KVM, an x86_64 vCPU with interrupts disabled
  doesn't park until the guest enables them again.

## Logging

`pylond` logs to standard error through `env_logger`. Set `RUST_LOG`
(`info`, `debug`) to choose the level. At `info`, it logs the managed
CPUs, whether `scx_pylon` was attached, registrations, and policy
changes.
