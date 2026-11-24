# Specification 11: Parking Inside KVM Without `sched_ext`

> **Status:** `DRAFT`, measured. §6 lists the measurements and §9 their
> results, from a prototype (`pylon/spec-11-proto`). They changed §4.1 and
> §4.2: an owner vCPU is pinned to its core. §7's gates pass except the
> barrier's throughput against `scx_pylon` (§9.9).

## 1. Motivation

Parking inside KVM (spec 08) keeps an unmodified guest alive while its
vCPUs are parked: a parked vCPU halts in KVM, and an interrupt wakes it
without the VMM. Parking in the VMM (`park=vmm`) strands guest work on
parked vCPUs, and in every measured run it ended in soft lockups, RCU
stalls or a guest that stopped making progress (spec 08 §7.4–7.5, the
aarch64 results in the developer guide's benchmarks page).

`pylond` refuses `park=kvm` without `scx_pylon` (`control.rs`), because
a vCPU that KVM wakes runs guest code with no core assigned, and only
`scx_pylon` keeps it on its VM's cores (spec 08 §3.2, §4.3).

`scx_pylon` can't run on most hosts today. `sched_ext` next to kernel
core scheduling corrupts the core-scheduling pick on kernels without
the upstream fixes (#15). Kernels before v6.19 have a related bug that
schedules a task that has exited, which crashed a 6.17 host for hours
(#132). Stock Ubuntu 24.04 and 26.04 kernels, 6.17 and 7.0, lack the
fixes, and the guard that makes `scx_pylon` safe there needs the BPF
LSM, which these kernels don't enable. Every host Pylon runs on in
production therefore runs `pylond --no-scx`. That leaves only
`park=vmm`, which unmodified guests don't survive.

This spec keeps the VMM's park inside KVM and replaces what `scx_pylon`
does with the kernel's ordinary scheduling interfaces: CPU affinity,
`SCHED_OTHER` slices and core-scheduling cookies. The stock fair
scheduler then does the scheduling, and `sched_ext` is never attached.

## 2. Goals & Non-Goals

**Goals**
- **G1.** `park=kvm` on a stock kernel without `sched_ext`: 6.8, 6.17,
  7.0, x86_64 and aarch64.
- **G2.** A vCPU without a core runs only on cores its VM owns. A VM
  that owns none runs its woken vCPUs on a shared pool (§4.2).
- **G3.** A core's owner gets most of the core; a woken vCPU of the same
  VM gets short turns on it.
- **G4.** `pylond` still learns when a parked vCPU wakes, so a VM whose
  parked vCPUs keep waking gets a core.
- **G5.** No two VMs share an SMT core at the same time.
- **G6.** No guest-side change. The VMM's park and the shared-page ABI
  stay as they are.

**Non-Goals**
- **Matching `scx_pylon`'s handoff latency.** Changing a thread's
  affinity costs more than writing a BPF map. §6 measures how much, and
  §7 sets the bar.
- **Replacing `scx_pylon`.** Where it can run safely (a fixed kernel, or
  the guard with the BPF LSM), it stays the faster option. This spec is
  a second mode, chosen when `sched_ext` isn't attached.
- **Isolating host tasks from tenant cores by default.** §4.6 describes
  it as an operator option.
- **MSHV.** It doesn't park inside the hypervisor.

## 3. What `scx_pylon` Does Today, and What Replaces It

| Duty of `scx_pylon` (spec 03, spec 08 §4.3) | Replacement in this spec |
|---|---|
| Run each core's owner there, ahead of anything else (owner queue, `SCX_KICK_PREEMPT`) | The owner's affinity includes its core, and the owner has a larger share of it than anything else that may run there (§4.1, §4.3) |
| Keep a woken parked vCPU on its VM's cores, in 100 µs turns with the owner (VM queue, turn timer) | Every vCPU thread's affinity is its VM's owned cores. A short per-task fair-scheduler slice gives the turns (§4.2, §4.3) |
| Keep a running vCPU without a core (a donor not yet parked) on its VM's cores | The same affinity (§4.2) |
| Set a parked vCPU's `woken` bit, which `pylond` reads as demand | A BPF tracing program on KVM's wake, with no `sched_ext` and no LSM (§4.4) |
| Keep two VMs off one SMT core (claims) | Kernel core scheduling: each VM's vCPU threads share one cookie, as Cloud Hypervisor sets with `core_scheduling=vm` (§4.5) |
| Give `pylond`'s engine its CPU (host owner) | Pin the engine; optionally keep host tasks off it (§4.6) |
| Honour KVM's directed yield between a VM's vCPUs (#93) | The fair scheduler already implements `yield_to` |
| Rescue a VM's woken vCPUs when it loses its last core | Its vCPUs' affinity moves to the shared pool (§4.2) |

## 4. Design

### 4.1 Owner placement

When `pylond` gives core *c* to vCPU *v* of VM *A* (`assign_pcpu`):

1. Set *v*'s affinity to *c* alone, and add *c* to the affinity of
   *A*'s other vCPU threads (§4.2). *v* must be pinned to *c* before it
   is woken.
2. Wake *v* as today: write the wake `EventFd`, and signal the thread
   when it parks inside KVM (spec 08 §3.3).

When `pylond` takes *c* back from *A* (the donor parks, or *A* loses
the core), remove *c* from the affinity of every vCPU thread of *A*
once the donor is `PARKED`. Until then the donor may still run on *c*,
as it does today under `scx_pylon` until the forced park lands.

The engine's owner map (`PcpuOwnerTable`) stays as `pylond`'s own
record; without `scx_pylon` it is already in-memory (`main.rs`).

**Per-thread affinity or a per-VM cpuset.** Two ways to set "the VM's
owned cores" on all its vCPU threads:

- *`sched_setaffinity` per thread:* one system call per vCPU thread per
  change. A parked thread is blocked in KVM, so the call only updates
  its mask. A running thread on a core it may no longer use is moved by
  the stopper thread (`migration/N`), which takes longer.
- *A cgroup-v2 cpuset per VM:* one write to `cpuset.cpus` updates every
  thread in the cgroup. The VMM's vCPU threads would have to be moved
  into it at registration, and the write's cost grows with the number
  of threads and may take `cpuset_mutex` against other cgroup writers.

§6 M1 measures both, and the choice follows from it: per-thread
affinity (§9.1).

**Why the owner is pinned.** The first prototype gave every vCPU of *A*,
the owner included, all of *A*'s cores. The fair scheduler then woke a
recipient onto a busy core of its VM instead of the idle core it had
just been given, and it ran there only when that core's slice ended:
2–3 ms from assignment to running, against 32 µs under `scx_pylon`, with
the given core idle 86% of the time (§9.2). Pinned to its core, it runs
in 40 µs.

### 4.2 Containment

Every vCPU thread of a registered `park=kvm` VM always has an affinity
mask:

- its own core, if it owns one (§4.1);
- otherwise the cores the VM owns, if it owns any;
- otherwise the **shared pool**: the managed cores that no VM owns
  right now, or all managed cores if every one is owned.

A vCPU that KVM wakes therefore runs only where G2 allows. A VM that
owns no core still gets its interrupts handled, on spare managed cores,
as `scx_pylon` does with its vCPU queue.

The VMM's other threads (its event loop, I/O and device threads) are
left alone. They aren't vCPUs, and they may already be pinned by the
operator.

### 4.3 Turns on an owned core

A core *c* owned by vCPU *v* of VM *A* may also run *A*'s woken parked
vCPUs and *A*'s donors before they park. Under the fair scheduler they
share *c* by weight and slice:

- **Slice.** Since Linux 6.12, a `SCHED_OTHER` task's `sched_attr.sched_runtime`
  sets its slice under EEVDF, and a task with a shorter slice can preempt
  on wakeup. The VMM, or `pylond` at registration, sets every vCPU
  thread's slice to 100 µs, like `PYLON_SHARE_SLICE`. 6.8 ignores it and
  uses the default slice; §6 M3 measures what that costs.
- **Weight.** The owner keeps its normal weight. A vCPU that is only
  woken spends most of its time halted, so it doesn't accumulate much
  run time and wins when it wakes, which gives interrupts low latency.
  A woken vCPU that keeps running (a busy loop on a parked vCPU) shares
  the core fairly with the owner, which is also what `scx_pylon`'s
  turns give it. `pylond` sees that as demand (§4.4) and gives the VM
  another core.

### 4.4 Wake detection

`scx_pylon` sets a parked vCPU's bit in its VM's `woken` mask when the
vCPU is enqueued, and the engine reads it (`engine.rs`, "woken"). Without
`sched_ext`, a small BPF **tracing** object does it instead:

- `tp_btf/kvm_vcpu_wakeup`, or `tp_btf/sched_waking` filtered by the
  thread IDs `pylond` lists (the `pylon_vcpu_dom` map exists for this),
  sets the bit when a parked vCPU's thread wakes;
- the bit is cleared as today, when the vCPU is assigned a core or
  halts again (`kvm_vcpu_block` or the vCPU thread's next sleep).

Tracing programs need `CAP_BPF` and `CAP_PERFMON`, which `pylond`
already has as root. They need neither `sched_ext` nor the BPF LSM, and
they don't change scheduling, so #15 and #132 don't apply. If BPF
isn't available at all, `pylond` can fall back to the guest agent's
runnable count, which reports demand late but doesn't need BPF (§6 M4).

### 4.5 SMT isolation

`scx_pylon` keeps two VMs off one SMT core with its claims. Here kernel
core scheduling does it: each VM's vCPU threads share one cookie.
Cloud Hypervisor already creates one per VM with `core_scheduling=vm`,
its default, for `--pylon` VMs too. It starts the VM without one only
when `pylond`'s guard refuses the request next to `sched_ext`
(`pylon::core_scheduling_refused` in `vmm/src/cpu.rs`). With no
`sched_ext` attached, nothing refuses, so the VMM needs no change.

Core scheduling with the fair scheduler is the configuration the kernel
supports, so #15 and #132 don't apply. Its cost is forced idle: a
sibling with no task of the same cookie idles. `pylond` keeps placing
two VMs' cores on different SMT cores, as it does now while
`scx_pylon` is attached, so forced idle comes mostly from host tasks
(§6 M6).

### 4.6 Host tasks on managed cores

`scx_pylon` lets host tasks run on a managed core only when its owner
doesn't need it. With the fair scheduler, a host task on a managed core
competes with the owner by weight. Two levels:

- **Default:** do nothing. Host tasks may land on managed cores, and the
  owner loses the share they take (§6 M5 measures it).
- **Operator option:** keep host tasks off managed cores and the engine
  CPU with systemd's `AllowedCPUs=` on `system.slice` and `user.slice`
  (cgroup-v2 cpusets). Per-CPU kernel threads stay, as they do under
  `scx_pylon`. `pylond` documents it and checks it at startup, but
  doesn't change it.

### 4.7 Mode selection

| `scx_pylon` attached | `park=kvm` VM registers | `--containment` |
|---|---|---|
| yes | as today | ignored |
| no | refused, as today | `none` (default until §7 passes) |
| no | accepted, contained as above | `affinity` |

`--containment=affinity` makes `pylond` load the tracing object (§4.4),
set every `park=kvm` VM's vCPU affinities and slices (§4.2, §4.3), and
expect cookies (§4.5). Once §7's gates pass, `affinity` becomes the
default when `sched_ext` isn't attached.

### 4.8 Changes

- **`pylond`:** affinity updates in `assign_pcpu` and when a revocation
  completes; the shared-pool update when a VM gains or loses its last
  core; slices at registration; the tracing object; mode selection.
- **VMM:** none required. It already asks for a cookie per VM (§4.5).
  Optionally, it sets its vCPU threads' slice itself (§4.3).
- **ABI:** none. The `woken` mask is `pylond`'s and stays where it is.
- **`scx_pylon`:** none.

## 5. Risks and Unknowns

- **Affinity changes on the handoff path.** A handoff becomes "update
  affinities, then wake", and a donor running on a core it may no longer
  use is moved by the stopper thread. If M1 and M2 show that this adds
  tens of µs, handoffs slow down by that much.
- **No owner priority.** The fair scheduler has no "this core belongs
  to this thread". An owner that stopped to wait (I/O, a page fault)
  competes again when it wakes, against woken vCPUs and host tasks on
  its core.
- **6.8 has no per-task slice.** Turns on an owned core then use the
  default slice (about 3 ms), so an interrupt on a parked vCPU may wait
  that long behind the owner (M3).
- **Forced idle.** Core scheduling may idle a sibling more than
  `scx_pylon`'s claims did (M6).
- **Wake detection.** The tracepoint has to fire for a vCPU that KVM
  wakes from `kvm_vcpu_block`, on every supported kernel and both
  architectures (M4).

## 6. Measurements Before Implementation

Each is a small experiment, not an implementation of this spec. Run on
the bare-metal AMD EPYC 9124 host (32 CPUs, 7.0), a 6.17 host, and the
aarch64 reference host where noted, with `sched_ext` disabled
throughout.

**M1. Affinity update cost.** A test program with 4 to 16 KVM vCPU
threads, using the forced-park harness (`bench_forced_park_latency_real_kvm*`):
- `sched_setaffinity` on a vCPU thread halted in KVM, on a runnable
  one, and on one running on a core being removed from its mask;
- one `cpuset.cpus` write for a cgroup holding those threads.

Report p50, p90 and p99 over 10,000 changes. Also measure the time from
the call until the moved thread runs on an allowed core.

**M2. Handoff latency without `sched_ext`.** The handoff of #53,
"revocation request → recipient running", with the affinity update in
the path: a prototype in a test build of `pylond` that only does §4.1
and §4.2, nothing else. Compare it with the same build under
`scx_pylon` on the 7.0 host (where the guard allows it with no cookies)
and with the published 100–126 µs median.

**M3. Woken-vCPU latency and the owner's share.** Two 4-vCPU guests,
3 managed cores, spec 08 P3's loads (work and ping-pong), VM 2 idle,
bursty or busy. Affinity set by hand as §4.2 would set it:
- vCPU slice default (about 3 ms), 100 µs (6.12+), and 6.8;
- report work units per 10 s, ping-pong p50 and p99, soft lockups and
  RCU stalls;
- compare with `park=vmm` (no affinity), with `scx_pylon` where it can
  run, and with the unmanaged baseline.

**M4. Wake detection.** A BPF tracing object on `kvm_vcpu_wakeup` and on
`sched_waking`:
- check that it loads and fires for a vCPU woken from
  `kvm_vcpu_block` on 6.8, 6.17 and 7.0, x86_64 and aarch64, with no
  BPF LSM;
- the delay from the wake to the bit being set, and the cost per wakeup
  for host tasks (filtered out by thread ID);
- for comparison, how late the guest agent's runnable count reports the
  same demand.

**M5. Host tasks on managed cores.** M3's work load with a host load
that competes for the managed cores (`stress-ng --cpu`, and a
fork/exec loop), with host tasks allowed on managed cores and with them
kept off (§4.6). Report the owner's throughput and the ping-pong tail.

**M6. Core scheduling's cost.** The overcommit suite's five loads
(`pylon/bench/overcommit/`) with cookies per VM (§4.5) against the same
runs without cookies, both with affinity containment. Report throughput,
how much time CPUs spend forced idle (`core_forceidle_sum` with
`kernel.sched_schedstats=1`), and that no two VMs' vCPUs ran on one SMT
core at once (a `sched_switch` trace).

**M7. End to end.** The overcommit suite against upstream Cloud
Hypervisor, with the M2 prototype plus M3's best slice, on the 7.0 and
6.17 hosts. Compare with the published `scx_pylon` results (0.78–0.83×
on loads that don't wait on each other, 3.2× at a barrier) and with
`park=vmm`.

**M8. Stability on the kernels that broke.** 8 hours on the 6.17 host
with VMs holding cookies, the prototype containing two `park=kvm` VMs,
and a churn of short-lived processes and VM starts and stops (#132's
trigger). Pass: no kernel warning, oops or soft lockup, and no guest
soft lockup or RCU stall.

## 7. Decision Gates

Build this spec if all of these hold:

| Gate | From | Threshold |
|---|---|---|
| No guest stalls | M3, M7, M8 | None in any run, where `park=vmm` has them |
| Kernel stability | M8 | No warning, oops or soft lockup in 8 h |
| Handoff | M2 | Median at most 2× `scx_pylon`'s on the same host |
| Throughput | M7 | At least `park=vmm`'s on every load, and within 10% of `scx_pylon`'s where both run |
| Interrupt latency on a parked vCPU | M3 | Ping-pong p99 under 1 ms with the other VM busy |
| Wake detection | M4 | Fires on every supported kernel without the BPF LSM |

If M1 or M2 fail the handoff gate but the rest pass, the mode is still
worth having for hosts that can't run `scx_pylon`. It would then be
documented as slower, not made the default.

## 8. Implementation Outline (if §7 passes)

1. **C1:** the tracing object and the `woken` mask from it (§4.4).
2. **C2:** affinity containment in `pylond`, `--containment=affinity`
   (§4.1, §4.2, §4.7).
3. **C3:** vCPU slices (§4.3).
4. **C4:** a check that `park=kvm` VMs in this mode have a cookie, and a
   warning when one runs with `core_scheduling=off` (§4.5).
5. **C5:** docs: the operator guide's `sched_ext` page, the host-task
   option (§4.6), and the release notes.

## 9. Results (2026-11-24)

**Setup.** Unless noted: a bare-metal AMD EPYC 7282 host (Zen 2, 16 cores
with SMT, one NUMA node), Ubuntu `7.0.0-43-generic`, `sched_ext` disabled
outside the `scx_pylon` runs. The overcommit suite (`pylon/bench/overcommit/`):
two 4-vCPU Ubuntu 26.04 guests on host CPUs 2–4, 150 s runs measured from
40 s, one run per load. The prototype is branch `pylon/spec-11-proto`:
`pylond --no-scx --containment-affinity` plus harness switches. It has no
wake detection (§4.4), so `pylond` sees demand only from the guest agent.
`scx_pylon` attached with the guard (this host has the BPF LSM), and the
VMs ran without cookies unless noted.

### 9.1 M1: affinity update cost

`pylon/bench/spec11/m1.c` and `m1cg.c`, 2,000–5,000 changes each, µs:

| Change | p50 | p99 |
|---|---|---|
| `sched_setaffinity`, thread blocked (as a vCPU halted in KVM) | 1.9 | 2.2 |
| `sched_setaffinity`, thread running, its CPU stays allowed | 4–7 | 5–11 |
| `sched_setaffinity`, thread running, its CPU removed (waits for the migration) | 21–27 | 29–37 |
| One `cpuset.cpus` write, 4–16 sleeping threads | 80–96 | 95–118 |
| One `cpuset.cpus` write, 4–16 spinning threads | 1,000–14,000 | 7,000–26,000 |

Per-thread affinity, as §4.1 now says.

### 9.2 M2: handoff latency

From `pylond`'s metrics, all five loads, µs, p50 / p99:

| | Wake (assigned → running) | Handoff (revocation → recipient running) |
|---|---|---|
| `scx_pylon` | 32–36 / 80–160 | 72–96 / 128–224 |
| Affinity, owner on all the VM's cores (first prototype) | 80–2,048 / 1,664–3,840 | 176–2,304 / 1,792–4,096 |
| Affinity, owner pinned to its core (§4.1) | 36–40 / 64–80 | 128–144 / 160–192 |

With the owner pinned, the median handoff is 1.8–2.0× `scx_pylon`'s and
its p99 is lower. The first prototype's recipient waited behind a busy
core of its VM while its own core idled: a 5 s `sched_switch` trace showed
CPU 2, the core that moved between the VMs, idle 86% of the time, and the
VMs used 2.1 of the 3 CPUs.

### 9.3 M3: turns on an owned core, and the overcommit suite (M7)

Work per second for both VMs together, and the VMs' CPUs:

| Load | `upstream` | `scx_pylon` | Pinned, default slice | Pinned, 100 µs slice | Pinned, 100 µs, cookies |
|---|---|---|---|---|---|
| `independent` | 2,370M (2.99) | 1,923M (3.00) | 2,334M (3.00) | 2,312M (2.99) | 2,304M (2.98) |
| `barrier` | 2.5k (3.00) | 10.3k (3.00) | 3.1k (3.00) | 3.1k (3.00) | 3.2k (2.98) |
| `spinlock` | 2.17M (3.00) | 1.68M (3.00) | 2.19M (3.00) | 2.10M (2.99) | 2.09M (2.98) |
| `pipes` | 753k (3.00) | 570k (3.00) | 747k (3.00) | 742k (3.00) | 736k (2.97) |

`noisy` (VM 1's ping-pong next to VM 2 spinning on every vCPU):

| | Ping-pong p50 / p99 / p99.9, µs | VM 2 | VMs' CPUs |
|---|---|---|---|
| `upstream` | 62 / 4,610 / 6,713 | 2,257M | 2.99 |
| `scx_pylon` | 189 / 374 / 467 | 1,269M | 2.21 |
| Pinned, default slice | 316 / 1,741 / 3,087 | 1,461M | 2.22 |
| Pinned, 100 µs slice | 308 / 907 / 1,985 | 1,489M | 2.25 |
| Pinned, 100 µs, cookies | 307 / 995 / 2,001 | 1,483M | 2.25 |

**`park=vmm`** (`pylond --no-scx`, `--pylon ...,park=vmm`): on `barrier`
and `noisy` the guests never started their loads within 150 s, as in
spec 08.

**What it shows:**
- No guest logged a soft lockup, RCU stall or hung task in any run of
  the prototype, where `park=vmm` guests don't get as far as the load.
- On loads that don't wait on each other, the prototype does what
  upstream does (0.96–0.98×), and better than `scx_pylon` (0.76–0.81×),
  whose 100 µs turns cost them (spec 08, the benchmarks page).
- At the barrier it keeps a little of the gain: 1.28× upstream, against
  4.1× for `scx_pylon`. The likely reason, not traced: the fair scheduler
  switches a core between an owner and its VM's woken vCPUs at the tick
  or a wakeup, not every 100 µs as `scx_pylon`'s turn timer does, and the
  shorter slice (`sched_runtime`) doesn't add a timer of its own.
- The 100 µs slice halves the ping-pong's p99 next to a busy VM
  (1,741 → 907 µs) and does nothing else measurable.

### 9.4 M4: wake detection

`pylon/bench/spec11/m4.bt`, 10 s on each kernel, with the hosts' own
VMs:

| Kernel | BPF LSM | `kvm_vcpu_wakeup` events | `sched_waking` → vCPU running |
|---|---|---|---|
| Ubuntu 6.8.0-138 | no | 14,092 | 4–16 µs, a few to 1–2 ms |
| Ubuntu 6.17.0-42 | no | 42,682 | 4–32 µs |
| Ubuntu 7.0.0-43 | yes | 1,055 | 8–64 µs, a few to 1–2 ms |

The tracepoint loads and fires on all three without `sched_ext` or the
BPF LSM. `sched_waking` filtered on vCPU thread names sees far fewer
wakes (3,551 of 14,092 on 6.8), most likely because halt polling catches
the rest before the thread sleeps, so `kvm_vcpu_wakeup` is the hook to
use. A program counting events costs about 0.8 µs per event
(`kernel.bpf_stats_enabled`, 1,238 events on 7.0). Not measured: aarch64,
for lack of a host.

### 9.5 M5: host tasks on managed cores

32 host busy loops allowed on every CPU, during the load:

| | `barrier`, both VMs | `noisy` p50 / p99 | VMs' CPUs | `pylond`'s CPU |
|---|---|---|---|---|
| `scx_pylon` | 9.9k | 197 / 356 µs | 3.00 / 2.27 | 1.00 |
| Affinity, owner on all the VM's cores | 0.76k | 79 / 3,819 µs | 2.00 / 1.31 | 0.71 / 0.87 |
| Pinned, 100 µs slice | 1.07k | 73 / 3,109 µs | 2.52 / 1.75 | 0.64 / 0.67 |

Under the fair scheduler, host tasks take a share of every managed core
and of the engine's CPU: `pylond`'s engine got 0.64–0.87 of its CPU, and
the barrier lost two thirds of its work. `scx_pylon` keeps both. So §4.6's
operator option, keeping host tasks off managed cores and the engine CPU,
is needed in practice, not optional. Not measured with it.

### 9.6 M6: core scheduling

With cookies per VM (`core_scheduling=vm`), every load is within run-to-run
noise of the same runs without (§9.3, last two columns). Not measured: time
spent forced idle (`core_forceidle_sum`), and a trace that no two VMs shared
an SMT core.

### 9.7 M8: stability

_Running: 8 hours on the 7.0 host, see the PR._

### 9.8 Not measured

- aarch64 (no host), and 6.8 and 6.17 beyond M4.
- §4.6's operator option (host tasks kept off managed cores).
- Repeat runs: one run per configuration, so differences under about 10%
  aren't significant.

### 9.9 Against the gates (§7)

| Gate | Result |
|---|---|
| No guest stalls | **Pass.** None in any run; `park=vmm` guests didn't start their load. |
| Kernel stability | Pending (M8). |
| Handoff, median at most 2× `scx_pylon`'s | **Pass, barely.** 128–144 µs against 72–96 µs, 1.8–2.0×. |
| Throughput: at least `park=vmm`'s, within 10% of `scx_pylon`'s | **Fail at the barrier** (0.31× `scx_pylon`); 1.2–1.3× `scx_pylon` on the other loads. At least `park=vmm`'s everywhere. |
| Ping-pong p99 under 1 ms with the other VM busy | **Pass with the 100 µs slice** (907–995 µs); fails with the default slice (1,741 µs). |
| Wake detection without the BPF LSM | **Pass** on 6.8, 6.17 and 7.0; aarch64 untested. |

**Reading.** As a replacement for `scx_pylon`, this mode loses most of its
barrier gain: it can't time-slice an owned core at 100 µs. As the mode for
hosts that can't run `scx_pylon` safely, which today leaves `park=vmm`, it
keeps unmodified guests alive and runs them about as upstream does, with a
better ping-pong tail. §7 allows that: a mode documented as slower, not
the default where `scx_pylon` runs. Before building it, §4.6's host-task
option needs measuring, since without it host load costs this mode far
more than `scx_pylon`.
