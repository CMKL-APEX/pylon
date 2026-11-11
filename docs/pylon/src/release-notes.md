# Release notes

## v0.0.8

The first Pylon release. Pylon is a fork of Cloud Hypervisor; the VMM
keeps upstream's version number (55.0.0), and the Pylon components
(`pylond`, `scx_pylon`, `pylon-abi`, `pylon-guest-agent` and the
`pv-pylon` guest module) are numbered 0.0.8. The files of a release
are listed in [Building and
installing](operator-guide/installation.md#download-a-release).

### What's in it

- **`--pylon`**: a VM gets a shared page with `pylond`, boots with only
  `initial_active_vcpus` running, and parks the rest once the guest
  agent reports that it is up.
- **`pylond`**: arbitrates a set of host CPUs between Pylon VMs. It
  wakes parked vCPUs of VMs with demand, reclaims idle cores, and takes
  cores from other VMs by urgency and `priority_weight`, within each
  VM's `min_cores` and `max_cores`. A donor that doesn't park within
  8 µs is parked by force. Policy can be changed at run time over the
  control socket, and usage is exported as Prometheus metrics.
- **`scx_pylon`**: a `sched_ext` scheduler, built into `pylond`, that
  runs each managed CPU's assigned vCPU thread and keeps vCPU threads
  apart from their SMT siblings.
- **Guest side**: `pylon-guest-agent` reports the guest's demand, and
  the optional `pv-pylon` module with the `pylon-kick` device lets a
  guest park a vCPU at a safe point when asked.
- **aarch64**: `pylond`, `scx_pylon` and the VMM side build and run on
  aarch64 hosts.

See [Status and roadmap](status.md) for what works and what doesn't.

### Highlights of this release

- **vCPUs park inside KVM** by default on x86_64 KVM hosts (`--pylon
  park=kvm`, [#51](https://github.com/CMKL-APEX/pylon/issues/51)). A
  parked vCPU stays halted in KVM, so a timer or an IPI wakes it. This
  removes the soft lockups and RCU stalls of guests without `pv-pylon`
  that hold fewer cores than they have vCPUs, at no throughput cost,
  nested and on bare metal
  ([spec 08 §7.4–§7.5](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md#74-p3-result-2026-11-09)).
  `park=vmm` keeps the previous park, in the VMM; aarch64 hosts use it
  by default.
- **`pylond` refuses a VM that parks inside KVM when `scx_pylon` isn't
  attached**, and the VMM now reads `pylond`'s reply: it logs whether
  the registration was accepted, and where its vCPUs park.
- **Core scheduling**: on kernels without the `sched_ext`
  core-scheduling fixes, where `pylond` makes the kernel refuse core
  scheduling ([#15](https://github.com/CMKL-APEX/pylon/issues/15)), a
  `--pylon` VM now runs without it and logs one warning, instead of
  failing to start. `scx_pylon` keeps its vCPU threads apart from their
  SMT siblings.
- **`min_cores` is a floor under demand**: a VM with runnable work is
  raised to `min_cores` first, and an idle VM holds no cores
  ([#58](https://github.com/CMKL-APEX/pylon/issues/58)).
- **The VMM keeps the shared page in a sealed `memfd`** instead of a
  file under `/dev/shm`, and `pylond` keeps the state it acts on in a
  host page the guest doesn't map. `--pylon shm=<path>` is gone.
- **The control socket is per user**: a VM's policy and stats are
  available only to the user that registered it, root, and `pylond`'s
  own user. Each connection has 5 seconds, and at most 64 are served at
  once. Give the socket to a group of the users your VMMs run as
  ([Running pylond](operator-guide/pylond.md#the-control-socket)).
- **`pylond` checks what a VM registers with**: the shared memory file
  must be a `memfd` whose size is sealed, and the kick and wake
  descriptors `eventfd`s. A VM whose `eventfd` write blocks is dropped.
- **`pv-pylon` checked against the real `pylond`**
  ([#55](https://github.com/CMKL-APEX/pylon/issues/55)): cooperative
  parks work end to end, but take longer than the grace period (see
  below).

### Requirements

- A Linux host with KVM, x86_64 or aarch64.
- For the default `park=kvm`: a kernel with `CONFIG_SCHED_CLASS_EXT`
  (6.12 or later), so that `pylond` can attach `scx_pylon`. On other
  kernels, start VMs with `--pylon ...,park=vmm`.
- The released `pylond` and `scx_pylon` need glibc 2.39 or later,
  `libelf`, `zlib` and `zstd`.

### Compatibility

- **Shared-page ABI version 3.** Parking inside KVM changed what two
  shared-page fields mean, so the ABI version is now 3. `pylond`, the
  VMM, `pylon-guest-agent` and `pv-pylon` from this release work
  together; builds from before #51 (version 2) refuse the page. Update
  all of them together.
- **Update the VMM and `pylond` together.** Besides the shared page, the
  VMM's shared memory now holds a host page with its own version (3),
  which `pylond` requires. The guest side is unaffected.
- **`--pylon shm=<path>`** is gone, and the VMM refuses a command line
  that has it: remove it. Every `--pylon` key is now optional.
- **`PYLON_PARK_IN_KVM`** is gone; use `--pylon park=kvm|vmm`.
- **Snapshot, restore and live migration** are refused for `--pylon`
  VMs.
- **`pylon-kick`'s PCI ID** (`0x1AF4:0x10F0`) is provisional and may
  change ([#64](https://github.com/CMKL-APEX/pylon/issues/64)).

### Known limits

- **Wake latency has a tail of about 1.9 ms** when another VM is busy,
  nested and on bare metal
  ([#52](https://github.com/CMKL-APEX/pylon/issues/52)).
- **Cooperative parks are too slow for the grace period**: a guest
  takes 80–140 µs to park after a revocation, against `pylond`'s 8 µs
  grace, so revocations end in a forced park
  ([#74](https://github.com/CMKL-APEX/pylon/issues/74)).
- **`pylond` restarts and vCPU hotplug aren't handled**: the VMM
  registers once and doesn't reconnect
  ([#60](https://github.com/CMKL-APEX/pylon/issues/60)).
- **A VM with no core isn't woken by arriving packets**
  ([#57](https://github.com/CMKL-APEX/pylon/issues/57)).
- **A booted aarch64 guest hasn't been run with Pylon**
  ([#62](https://github.com/CMKL-APEX/pylon/issues/62)).

The next release, `v0.0.9`, is tracked on its
[milestone](https://github.com/CMKL-APEX/pylon/milestone/2).
