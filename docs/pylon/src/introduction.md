# Introduction

Pylon gives virtual machines physical CPU cores for as long as they
have work, and takes the cores back as soon as they don't. It's part of
`CMKL-APEX/pylon`, a fork of [Cloud Hypervisor](https://www.cloudhypervisor.org/)
built for the [Glidex](https://cmkl-apex.github.io/glidex/) cloud
suite.

The usual ways to resize a VM's CPU are slow. ACPI CPU hotplug takes
milliseconds per vCPU and goes through the guest's CPU online/offline
path. Leaving vCPUs to the host's fair scheduler means time-slicing
them against each other on a millisecond scale. Pylon aims to move a
physical core from one VM to another in **microseconds**. It does this
with a shared-memory page that both the guest and the host read and
write, a polling arbiter on a dedicated host core, and a Linux
`sched_ext` scheduler that switches a core to its new owner without
going through the normal scheduler.

> **Status.** Pylon is under active development. Core arbitration,
> parking and waking, metering and the `sched_ext` dispatcher work.
> Cooperative revocation, which is how a running VM is asked to give a
> core back, doesn't work end to end yet. See [Status and
> roadmap](status.md) before relying on any behavior.

## Features

- **Elastic vCPUs.** A Pylon VM boots with all of its vCPU threads, but
  only `initial_active_vcpus` of them run. The others are parked and
  cost nothing until `pylond` gives them a core.
- **A per-host core arbiter.** `pylond` owns a set of host CPUs and
  hands them to VMs by demand and priority. It polls guest demand about
  every microsecond on a dedicated core, with no heap allocations, locks
  or blocking calls in its loop.
- **Per-VM policy.** Each VM has a minimum and maximum number of cores
  and a priority weight. They can be changed at runtime without
  restarting the VM.
- **Scale to zero.** A VM can hold no cores at all and be woken when its
  next timer is due.
- **Kernel-level handoff.** On a kernel with `sched_ext`, the
  `scx_pylon` BPF scheduler runs whichever vCPU thread `pylond` assigned
  to a core, read straight from a memory-mapped BPF map.
- **Metering.** `pylond` counts each VM's core-microseconds,
  cooperative yields and forced preemptions, over its control socket and
  as Prometheus metrics.

## How these docs are organized

- [Quick start](quick-start.md): build Pylon, start `pylond` and boot a
  VM with elastic vCPUs.
- [Concepts](concepts.md): the parts of Pylon and the words these docs
  use for them.
- [Status and roadmap](status.md): what works today, what doesn't, and
  what is planned.
- [User guide](user-guide/README.md): running Pylon VMs and what the
  guest sees.
- [Operator guide](operator-guide/README.md): building, installing and
  running `pylond` and `scx_pylon` on a host, setting policy and
  monitoring.
- [Developer guide](developer-guide/README.md): architecture, ABI,
  algorithms, design decisions, planned enhancements and benchmarks, for
  contributors.
- [Reference](reference/README.md): the `--pylon` option, `pylond`'s
  options, the control socket protocol, metrics and other tools.

If you run VMs through Glidex, Glidex installs and configures Pylon for
you. Its [operator guide](https://cmkl-apex.github.io/glidex/docs/operator-guide/installation.html#pylon)
covers that path. This book covers Pylon itself.
