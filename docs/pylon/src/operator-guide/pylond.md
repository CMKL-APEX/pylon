# Running pylond

`pylond` is the per-host core arbiter. Run exactly one per host. Every
option is in the [pylond reference](../reference/pylond.md).

## Host CPUs

`pylond` uses three groups of host CPUs:

| CPUs | Used for | Set by |
|---|---|---|
| 0 | **Control plane**: the Unix socket, VM registrations, policy and stats requests, the metrics server. Mostly idle. | Fixed |
| 1 | **Arbitration engine**: polls every VM's shared page about every microsecond. It keeps this CPU fully busy. | Fixed |
| `--cores` | **Managed CPUs**, given to VMs' vCPUs. | `--cores`, default `2-7` |

`--cores` takes a comma-separated list of CPUs and ranges, such as
`2-7` or `4-15,20-31`.

- **Don't put 0 or 1 in `--cores`.** `pylond` doesn't check, and its own
  threads would then compete with VMs on those CPUs.
- The engine thread spins at all times. On CPUs with `WAITPKG`, it waits
  with `TPAUSE` between sweeps, which uses less power than spinning; on
  others, it uses `PAUSE`. The engine logs `waitpkg=true` or `false` at
  start.
- Keep other work off CPU 1, for example with the kernel's
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
the socket with the process umask. It doesn't set the socket's owner or
mode, so set them yourself after it starts (see the [systemd
unit](installation.md#a-systemd-unit-for-pylond)). Anyone who can write
to the socket can register VMs, change any VM's policy and read every
VM's stats. Give access only to the users your VMMs run as.

## Start order

Start `pylond` before Pylon VMs. A VMM registers once, when it starts,
and doesn't retry or reconnect. If `pylond` restarts, it forgets every
VM. Running VMs keep their running vCPUs, but parked ones stay parked
until the VMs restart.

## Stopping

`pylond` has no signal handlers of its own: `SIGTERM` and `SIGINT` end it
immediately. The kernel then detaches `scx_pylon`, if it was attached,
and the host's normal scheduler takes over. vCPUs that were parked stay
parked. The socket file is left behind and removed at the next start.

## Logging

`pylond` logs to standard error through `env_logger`. Set `RUST_LOG`
(`info`, `debug`) to choose the level. At `info`, it logs the managed
CPUs, whether `scx_pylon` was attached, registrations, and policy
changes.
