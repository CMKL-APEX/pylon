# Release notes

## v0.0.10

A small release for 6.17 hosts. Nothing changes in the shared-page ABI,
the host page or the `--pylon` options, so `pylond`, the VMM and the
guest side from v0.0.9 work with this release.

### Changes since v0.0.9

- **Run `pylond` in a slice of its own on 6.17 kernels**
  ([#137](https://github.com/CMKL-APEX/pylon/issues/137),
  [#138](https://github.com/CMKL-APEX/pylon/pull/138)). On a 6.17
  kernel, with `pylond` under `system.slice`, its polling engine leaves
  every other thread on the engine CPU with no CPU time, kernel workers
  included. In a top-level slice of its own, such as
  `Slice=pylon.slice` in its unit, the other threads get their share,
  as on 6.8 and 7.0. The example unit in [Building and
  installing](operator-guide/installation.md) now sets it, and
  `pylond` warns at startup when it runs under `system.slice` on 6.17.
  It changes nothing else about how `pylond` runs.
- **CI uses Cloud Hypervisor's v7.2.8-20261125 guest kernels**
  ([#139](https://github.com/CMKL-APEX/pylon/pull/139)). Upstream
  replaced the files of the previous release, so their checksums no
  longer matched.

### Upgrading

- **On a 6.17 host,** add `Slice=pylon.slice` to the `[Service]`
  section of `pylond`'s unit before you upgrade, or `pylond` keeps
  starving its engine CPU. Other kernels don't need it.
- Otherwise, replace the binaries.

The known limits of v0.0.9 and v0.0.8 below still apply.

## v0.0.9

The same code as `v0.0.9-rc3`, with the Pylon components numbered
0.0.9. Nothing changes in the shared-page ABI, the host page or the
`--pylon` options, so `pylond`, the VMM and the guest side from v0.0.8
work with this release.

### Changes since v0.0.8

- **Synced with upstream Cloud Hypervisor v55.0**
  ([#118](https://github.com/CMKL-APEX/pylon/pull/118)). Upstream
  changes some VMM options of its own, such as the syntax for restore
  and receive-migration and some deprecated APIs; see upstream's v55.0
  release notes.
- **Kernels before 6.19 are guarded again** (rc3,
  [#132](https://github.com/CMKL-APEX/pylon/issues/132)). rc1 and rc2
  treated 6.12 to 6.18 as safe next to core scheduling
  ([#121](https://github.com/CMKL-APEX/pylon/pull/121)), but they have a
  related bug: a task that has exited can be scheduled again. On a 6.17
  host with core-scheduling cookies, rc2 crashed the kernel
  (`kernel BUG` in `do_task_dead`) and left it in soft lockups for hours.
  As in v0.0.8, `scx_pylon` attaches on such a kernel only with the
  guard, which needs the BPF LSM. **Don't run rc1 or rc2 with
  `scx_pylon` on a kernel before 6.19**: use `--no-scx`, or upgrade.
- **`scx_pylon` loads on 6.17**: its verifier rejected it as too large
  ([#123](https://github.com/CMKL-APEX/pylon/pull/123)).
- **Host tasks no longer wait while CPUs idle** under `scx_pylon`
  ([#125](https://github.com/CMKL-APEX/pylon/pull/125),
  [#128](https://github.com/CMKL-APEX/pylon/pull/128)). On a 256-CPU
  host, they waited for seconds and the load average climbed. Now:
  - A woken host task goes straight to the idle CPU it found, and idle
    CPUs are woken while host tasks queue.
  - Host tasks get a 3 ms slice instead of 20 ms.
  - With kernel core scheduling on, a CPU next to a task with a cookie
    isn't picked to run another task while a better one is idle.
  - On a 32-CPU host under a mixed load, no wakeup waits over 10 ms,
    as under the fair scheduler (681 per 30 s before).
- **GPU agentic-inference workload study** in
  [Performance and benchmarks](developer-guide/benchmarks.md)
  ([#115](https://github.com/CMKL-APEX/pylon/pull/115)).
- **The overcommit benchmark** stops `pylond` when it aborts, and
  refuses to run next to another `pylond`
  ([#119](https://github.com/CMKL-APEX/pylon/pull/119)).

### Known limits added

- **Host tasks on a large host with core scheduling on** still waited
  up to a few hundred milliseconds a handful of times per 30 s in
  testing on 6.17, before rc3 guarded it; under the fair scheduler, none
  wait over about 2 ms.
- **Loading `scx_pylon` on 6.17** logs one verifier
  `REG INVARIANTS VIOLATION` warning, which taints the kernel; 7.0 logs
  none.
- **The 3 ms host slice** costs a VM about 3% of its CPU time when host
  work competes for its managed CPUs (the overcommit suite's `noisy`
  load: 2.17 CPUs instead of 2.23).
- **The overcommit benchmark doesn't notice** when `scx_pylon` is
  disabled partway through a run
  ([#129](https://github.com/CMKL-APEX/pylon/issues/129)).

The known limits of v0.0.8 below still apply.

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

- **vCPUs park inside KVM** by default on KVM hosts, x86_64 and aarch64
  (`--pylon park=kvm`, [#51](https://github.com/CMKL-APEX/pylon/issues/51)). A
  parked vCPU stays halted in KVM, so a timer or an IPI wakes it. This
  removes the soft lockups and RCU stalls of guests without `pv-pylon`
  that hold fewer cores than they have vCPUs, at no throughput cost,
  nested and on bare metal
  ([spec 08 §7.4–§7.5](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md#74-p3-result-2026-11-09)).
  `park=vmm` keeps the previous park, in the VMM.
- **aarch64 runs real guests end to end**, with `scx_pylon` attached and
  vCPUs parked inside KVM, after a fix to how a suspended aarch64 vCPU
  wakes ([#62](https://github.com/CMKL-APEX/pylon/issues/62)).
- **VMs survive a `pylond` restart**
  ([#60](https://github.com/CMKL-APEX/pylon/issues/60)). A VMM keeps its
  registration open; when `pylond` goes away, it wakes every parked vCPU
  and runs the VM unmanaged, and it registers again once a new `pylond`
  listens. A VM started before `pylond` registers once it appears
  ([Running pylond](operator-guide/pylond.md#start-order-and-restarts)).
- **Busy VMs share an odd core** ([#110](https://github.com/CMKL-APEX/pylon/pull/110)).
  When two VMs with equal weights both want every core, the core above
  an even split moves between them every 10 ms, instead of staying with
  the VM that took it first. A VM's vCPUs beyond its cores share them
  in 100 µs turns.
- **Measured against upstream Cloud Hypervisor under overcommit**
  ([#111](https://github.com/CMKL-APEX/pylon/pull/111)). With two
  4-vCPU VMs on three CPUs, Pylon does 3.2 times upstream's work at a
  spinning barrier, and cuts a ping-pong's p99 next to a busy VM from
  about 4.7 ms to 0.3 ms. Loads whose threads don't wait on each other
  do 17–22% less (see Known limits and
  [Performance and benchmarks](developer-guide/benchmarks.md#pylon-against-upstream-cloud-hypervisor-under-overcommit-measured-2026-11-15)).
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
  which `pylond` requires. The guest side is unaffected. The VMM also
  interrupts a vCPU with `SIGUSR2` instead of `SIGRTMIN` now, which
  `pylond` from `v0.0.8-rc3` and earlier refuses.
- **`--pylon shm=<path>`** is gone, and the VMM refuses a command line
  that has it: remove it. Every `--pylon` key is now optional.
- **`PYLON_PARK_IN_KVM`** is gone; use `--pylon park=kvm|vmm`.
- **Snapshot, restore and live migration** are refused for `--pylon`
  VMs.
- **`pylon-kick`'s PCI ID** (`0x1AF4:0x10F0`) is provisional and may
  change ([#64](https://github.com/CMKL-APEX/pylon/issues/64)).

### Known limits

- **Wake latency has a tail of about 0.45 ms** when another VM is busy
  (about 1.9 ms in `v0.0.8-rc1`), nested and on bare metal
  ([#52](https://github.com/CMKL-APEX/pylon/issues/52)).
- **Cooperative parks are too slow for the grace period**: a guest
  takes 80–140 µs to park after a revocation, against `pylond`'s 8 µs
  grace, so revocations end in a forced park
  ([#74](https://github.com/CMKL-APEX/pylon/issues/74)).
- **vCPU hotplug is refused** for VMs with `--pylon`
  ([#60](https://github.com/CMKL-APEX/pylon/issues/60)).
- **Overcommit costs loads that don't wait on each other 17–22%** of
  their work against upstream Cloud Hypervisor sharing the same CPUs:
  the 100 µs turns that win at a barrier cost them elsewhere. 1 ms turns
  cost under 3%, but give up most of the barrier's gain.
- **The odd core's rotation stopped once** at a 1 to 2 split, in 1 of
  15 benchmark runs. It didn't recur in 10 later runs, and the issue is
  closed until it does
  ([#113](https://github.com/CMKL-APEX/pylon/issues/113)).
- **A VM with no core isn't woken by arriving packets**
  ([#57](https://github.com/CMKL-APEX/pylon/issues/57)).

The next release, `v0.0.11`, is tracked on its
[milestone](https://github.com/CMKL-APEX/pylon/milestone/4).
