# Operator guide

This guide is for people who run hosts with Pylon: building and
installing the binaries, running `pylond`, deciding whether to use the
`scx_pylon` kernel scheduler, setting VM policy, and watching what
happens.

| Page | What it covers |
|---|---|
| [Building and installing](installation.md) | Build requirements, the binaries, where to put them, and a systemd unit for `pylond`. |
| [Running pylond](pylond.md) | Which host CPUs it uses and manages, its socket, startup and shutdown. |
| [Kernel scheduling with scx_pylon](sched-ext.md) | What the `sched_ext` dispatcher does, when `pylond` attaches it, and what it means for the rest of the host. |
| [Policy and monitoring](monitoring.md) | Changing a VM's cores and weight at runtime, reading stats, and Prometheus metrics. |
| [Troubleshooting](troubleshooting.md) | Common problems and how to find their cause. |

## A Pylon host at a glance

```text
host CPU 0      pylond control plane (socket, registrations, metrics)
host CPU 1      pylond arbitration engine (polls every ~1 µs)
--cores CPUs    given to Pylon VMs' vCPUs by pylond
other CPUs      everything else: VMM threads, devices, host services
```

Each Pylon VM is a `cloud-hypervisor` process started with `--pylon`.
It registers with `pylond` over a Unix socket when it starts.

Glidex installs all of this as `glidex-pylond.service`, with its own
defaults. Its [installation guide](https://cmkl-apex.github.io/glidex/docs/operator-guide/installation.html#pylon)
describes that setup. The pages here describe Pylon's own binaries and
options.
