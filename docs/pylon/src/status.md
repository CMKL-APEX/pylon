# Status and roadmap

This page says what works in Pylon today, what doesn't, and what is
planned. It reflects the `v0.0.8` release (see the [release
notes](release-notes.md)). Open work is tracked as GitHub issues on the
[`v0.0.9` milestone](https://github.com/CMKL-APEX/pylon/milestone/2).
The detailed tracker is
[`spec/05-implementation-status.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md).

## What works

| Area | Status |
|---|---|
| Shared page | Created per VM, mapped into the guest at a fixed address, and shared with `pylond`. Layout checked at compile time. |
| `--pylon` option | Parsed and validated. VMs boot with only `initial_active_vcpus` running; the guest agent's first heartbeat parks the rest. |
| Registration | The VMM registers with `pylond` at boot, passing the shared page and per-vCPU `eventfd`s, and logs whether `pylond` accepted the VM. A VM is removed when its VMM exits. |
| Waking parked vCPUs | `pylond` wakes a parked vCPU when its VM has demand and a managed CPU is idle, or takes a core from another VM. |
| Taking back idle cores | When a vCPU parks on its own, `pylond` reclaims its core. |
| Forced revocation | When a donor misses its 8 µs deadline, `pylond` signals its vCPU thread, the VMM parks it, and the core moves. Repeated signals back off up to 1 ms. |
| Parking inside KVM | With `--pylon park=kvm`, the default on KVM hosts (x86_64 and aarch64), a parked vCPU halts inside KVM, so an interrupt (an IPI, a timer) wakes it. `scx_pylon` runs a woken vCPU only on its own VM's CPUs, and `pylond` gives a core to a VM whose vCPU stays woken. This removes the soft lockups of VMs that hold fewer cores than they have vCPUs. See [spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md). |
| Arbitration | Demand evaluation, urgency ordering and donor selection run every tick (see [The arbitration engine](developer-guide/arbitration.md)). |
| Runtime policy | `min_cores`, `max_cores` and `priority_weight` can be changed over the control socket. `min_cores` is a floor under demand: an idle VM holds no cores. |
| Metering | Core-microseconds, cooperative yields, forced preemptions, handoffs without park, revocation-to-park latency and woken-vCPU time per VM, over the socket and as Prometheus metrics. |
| `scx_pylon` | Attaches to a `sched_ext` kernel and runs each managed CPU's assigned vCPU thread, keeps vCPU threads' SMT siblings apart, and shares a VM's CPUs with its woken vCPUs. `pylond` attaches it automatically when the kernel supports it. |
| Kick device and guest module | The `pylon-kick` PCI device delivers revocations to the guest as interrupts, and the `pv-pylon` guest module parks a revoked vCPU at a safe point and reports stranded work. |
| aarch64 hosts | `pylond`, `scx_pylon` and the VMM side build, run and have CI on aarch64. Real guests run end to end with `scx_pylon` and parking inside KVM, at the unmanaged guest's throughput ([spec 07](https://github.com/CMKL-APEX/pylon/blob/main/spec/07-aarch64-support.md) A-T8). |

## What doesn't work yet

**Parking inside KVM has limits.** It is the default on x86_64 KVM
hosts (`--pylon park=kvm`), because it stops the stalls in unmodified
guests and costs no throughput, nested and on bare metal
([spec 08 §7.4](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md#74-p3-result-2026-11-09),
[§7.5](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md#75-bare-metal-result-2026-11-10)).
But:

- **It needs `scx_pylon`.** `pylond` without it rejects such a VM; use
  `park=vmm` there.
- **Wake latency has a tail of about 0.45 ms** when another VM is busy
  and two vCPUs of a VM share a core: a woken vCPU spins until KVM's
  pause-loop exit yields to the other. It was about 1.9 ms before
  `scx_pylon` honoured those yields
  ([#52](https://github.com/CMKL-APEX/pylon/issues/52)).

**Cooperative revocation, within the grace period.** The parts exist
(`pylon-kick` and `pv-pylon`), but:

- **Too slow for the grace period.** Under the real `pylond`, a guest
  takes 80–140 µs on average to park after a revocation, far more than
  `pylond`'s 8 µs grace period, so revocations end in a forced park.
  A longer grace makes them cooperative but gains no throughput, so the
  guest's yield has to get faster ([#74](https://github.com/CMKL-APEX/pylon/issues/74)).
  On the bare-metal x86_64 host, the kick takes 4–32 µs to reach the
  guest, and the guest then takes 16–64 µs to ask to park: kernel code
  runs that slowly on that host, in a guest or not ([spec 09 §9](https://github.com/CMKL-APEX/pylon/blob/main/spec/09-park-by-halt.md#9-h0-result-2026-11-14)).
- **Handoffs take about 100–125 µs.** On bare metal, from a revocation
  request to the recipient vCPU running takes 100–126 µs at the median
  ([#53](https://github.com/CMKL-APEX/pylon/issues/53); see
  [Performance and benchmarks](developer-guide/benchmarks.md#handoff-latency-between-real-vms-measured-2026-11-13)),
  against a 3–6 µs target. Most of it is the VMM's 40–60 µs to park or
  wake a vCPU after a signal, which isn't explained yet
  ([#54](https://github.com/CMKL-APEX/pylon/issues/54)).
- **No module, no cooperation.** A guest without `pv-pylon` never
  cooperates: every revocation ends in a forced park.

**Other gaps** (tracked as K1–K7 in the status tracker's
[known issues](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md#known-issues-other)):

- **Scale to zero.** Without parking inside KVM, a VM that holds no
  cores is woken when a timer it armed before its vCPUs parked comes
  due: the VMM reads each vCPU's timer as it parks it (on x86_64 the
  TSC-deadline timer, on aarch64 the EL1 virtual timer). Inside KVM,
  KVM fires the timer itself. Arriving packets don't wake a VM with no
  core: nothing on the host feeds the receive doorbell yet ([#57](https://github.com/CMKL-APEX/pylon/issues/57)).
- **Critical sections.** Nothing writes `in_critical_section`, so donor
  selection can't avoid lock holders. It needs guest kernel hooks, not
  just the module ([#56](https://github.com/CMKL-APEX/pylon/issues/56)).
- **Counter scaling.** `pylond` converts the guest's counter values
  with the offset the VMM measures, assuming the guest counter runs at
  the host's rate. A VM restored with a different TSC rate would break
  that, but restore doesn't support Pylon yet anyway.
- **The shared page's address.** The page is no longer reported to the
  guest as RAM, and the layout declares it, but it still sits at the fixed
  address `0xFEDF_0000` instead of a PCI BAR ([#59](https://github.com/CMKL-APEX/pylon/issues/59)).
- **Lifecycle.** Snapshot and live migration are refused with
  `--pylon` ([#61](https://github.com/CMKL-APEX/pylon/issues/61)), and so
  is resizing the vCPUs. A `pylond` restart isn't handled: the VMM
  registers once, at boot, and doesn't reconnect ([#60](https://github.com/CMKL-APEX/pylon/issues/60)).
- **PCI ID.** `pylon-kick` uses a provisional ID, `0x1AF4:0x10F0`, from
  QEMU's experimental range ([#64](https://github.com/CMKL-APEX/pylon/issues/64)).

## Roadmap

`v0.0.8` ([milestone](https://github.com/CMKL-APEX/pylon/milestone/1)) made parking inside KVM a `--pylon`
option, the default on KVM hosts, with ABI version 3 ([#51](https://github.com/CMKL-APEX/pylon/issues/51)).
The next release is `v0.0.9` ([milestone](https://github.com/CMKL-APEX/pylon/milestone/2)):

| Work | Issues |
|---|---|
| End more parks by a wake than by the bound. | [#52](https://github.com/CMKL-APEX/pylon/issues/52) |
| Measure and cut handoff latency; make the guest yield within the grace period. | [#53](https://github.com/CMKL-APEX/pylon/issues/53), [#54](https://github.com/CMKL-APEX/pylon/issues/54), [#74](https://github.com/CMKL-APEX/pylon/issues/74) |
| Report critical sections from the guest. | [#56](https://github.com/CMKL-APEX/pylon/issues/56) |
| Scale to zero on network traffic. | [#57](https://github.com/CMKL-APEX/pylon/issues/57) |
| Move the shared page into a PCI BAR; get a PCI ID. | [#59](https://github.com/CMKL-APEX/pylon/issues/59), [#64](https://github.com/CMKL-APEX/pylon/issues/64) |
| Lifecycle: `pylond` restart, vCPU hotplug, snapshot and migration. | [#60](https://github.com/CMKL-APEX/pylon/issues/60), [#61](https://github.com/CMKL-APEX/pylon/issues/61) |
| aarch64: the kick path on the GICv3 ITS. | [#63](https://github.com/CMKL-APEX/pylon/issues/63) |
| MSHV: check that a signal-interrupted run returns cleanly. | [#65](https://github.com/CMKL-APEX/pylon/issues/65) |

[Enhancements](developer-guide/enhancements.md) explains the design of
the cooperative path.
