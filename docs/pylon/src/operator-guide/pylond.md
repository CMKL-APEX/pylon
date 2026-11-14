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
connect. The socket appears at the path only once `pylond` accepts
connections and has its group and mode: `pylond` creates it as
`<path>.tmp` and renames it.

`pylond` records the UID of the process that registers each VM. A
VM's policy can be changed, and its stats read, only by that UID, by
root, and by `pylond`'s own UID; other users see neither. But anyone
who can write to the socket can register VMs: register under a `vm_id`
before the VM that should have it starts (that VM is then rejected and
runs unmanaged), or register VMs that ask for cores. Give access only
to the users your VMMs run as, through a group of their own rather than
a shared one such as `kvm` (see the
[systemd unit](installation.md#a-systemd-unit-for-pylond)).

Each client gets 5 seconds to send its request and read the response,
and the socket serves at most 64 clients at a time; further ones are
closed at once. The metrics server has the same limits. When it runs
out of file descriptors, `pylond` logs the failed `accept` and tries
again 100 ms later. At start it raises its soft limit on open files to
the hard limit: each registered VM holds up to 129 of them.

## Start order and restarts

`pylond` and its VMs can start in either order, and `pylond` can be
restarted while VMs run ([spec 10](https://github.com/CMKL-APEX/pylon/blob/main/spec/10-pylond-restart.md)):

- **A VM keeps its registration open.** When `pylond` exits, for any
  reason, the VMM finds out at once. It wakes every parked vCPU and runs
  the VM unmanaged: all its vCPUs run where the host's scheduler puts
  them, and none park. The VMM logs `pylond closed the session of VM
  <id>; it runs unmanaged until a pylond listens at <path> again`.
- **A VM registers again** when a new `pylond` creates the socket. The
  VMM watches the socket's directory, and checks the path every 5
  seconds as well. `pylond` then parks the vCPUs beyond
  `initial_active_vcpus`, as after the VM's boot, and gives the VM
  cores from there.
- **A VM that starts before `pylond`** runs unmanaged and registers
  once the socket appears.

What a restart loses: policy changed over the control socket (a VM
registers again with its `--pylon` values), and the stats and metrics
counters. A VMM that `pylond` refuses waits for the next new socket
before it tries again. VMMs from `v0.0.8-rc1` and earlier register once
and don't come back after a restart.

## Stopping

`pylond` has no signal handlers of its own: `SIGTERM` and `SIGINT` end it
immediately. The kernel then detaches `scx_pylon`, if it was attached,
and the host's normal scheduler takes over. Each VMM sees its session
end and runs its VM unmanaged, with every vCPU awake, until a `pylond`
starts again ([Start order and restarts](#start-order-and-restarts)).
The socket file is left behind and removed at the next start.

## VMs that park inside KVM

A VMM started with `--pylon park=kvm`, the default on x86_64 KVM
hosts, parks vCPUs halted inside KVM instead of blocked in the VMM, so
an interrupt wakes a parked vCPU. It logs `Pylon: VM <id> parks vCPUs
inside KVM`
([spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md)).
`pylond` needs no option for it. What changes on the host:

- **It needs `scx_pylon`.** `scx_pylon` runs a woken vCPU only on CPUs
  its VM owns and reports it to `pylond`, which then gives it a spare
  core at once, and a core to a VM whose vCPU stays woken for 4 ms. `pylond` without `scx_pylon`
  rejects such a VM's registration, and logs `Rejected registration`;
  start the VM with `park=vmm` on such a host
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
(`warn`, `debug`) to choose the level; the default is `info`. At
`info`, it logs the managed CPUs, whether `scx_pylon` was attached,
registrations, and policy changes. If the kernel rejects `scx_pylon`,
libbpf prints the verifier's log just before `pylond`'s warning.
