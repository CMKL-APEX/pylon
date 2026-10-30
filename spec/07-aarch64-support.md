# Specification 07: aarch64 Host & Guest Support

> **Status:** `DRAFT` — design for review. Nothing here is implemented yet.
> Tracked as **Track 6** in `spec/05-implementation-status.md`. The task board
> in §9 is the place to claim, track and hand off work.

## 1. Motivation: Where aarch64 Stands Today

Spec 06 §2 lists aarch64 as a non-goal and §9 as a follow-up. Every Pylon
crate *builds* for aarch64, because each x86-only path has a
`cfg(not(target_arch = "x86_64"))` stub. But the stubs return constants, so
on an aarch64 host Pylon builds and then does nothing useful:

| # | Gap | Location | Effect on aarch64 |
| :- | :--- | :--- | :--- |
| A1 | `read_tsc()` returns `0`; `calibrate_tsc_khz` returns `None`. | `pylon/pylond/src/engine.rs` | `now_tsc` is always 0. Deadlines never pass, metering never advances, and the rate falls back to the 3 GHz `DEFAULT_TSC_KHZ`. |
| A2 | `pause_cycles` falls back to a bare `spin_loop()`; `detect_waitpkg` returns `false`. | `pylon/pylond/src/engine.rs` | The engine core spins flat out. The sweep interval (`DEFAULT_TPAUSE_CYCLES`) has no meaning. |
| A3 | `read_host_tsc()` returns `0`. | `vmm/src/pylon.rs` | `park_decision` never sees `now_tsc >= deadline`, so the forced path (spec 06 §4.4) never runs. `last_yield_tsc` is always 0. |
| A4 | `read_guest_tsc` / `read_guest_tsc_deadline` return `None` (they read the x86 registers `IA32_TSC` / `IA32_TSC_DEADLINE`). | `vmm/src/pylon.rs` | `guest_tsc_offset` stays 0 and parked vCPUs publish no timer, which loses the K1 timer fix. |
| A5 | The guest agent writes `head_wait_tsc = 1` instead of reading a clock. | `pylon/pylon-guest-agent/src/lib.rs` `update_telemetry` | Head-of-line wait is meaningless, so demand urgency is wrong. |
| A6 | `vmlinux.h` is an x86_64 type dump; `build.rs` hard-codes `/usr/include/x86_64-linux-gnu` and sets no `__TARGET_ARCH_*`. | `pylon/scx-pylon/src/bpf/vmlinux.h`, `pylon/scx-pylon/build.rs` | The BPF object is built against x86 kernel types. The `sched_ext` structs are probably the same layout on arm64, but nobody has checked. |
| A7 | The shared page's default GPA `0xFEDF_0000` lies in the aarch64 `MEM_32BIT_RESERVED` hole, but no layout constant claims it. | `pylon/pylon-abi` `PYLON_DEFAULT_GPA`; `arch/src/aarch64/layout.rs` | No clash today (RAM ends at `0xFC00_0000`, and the TPM is at `0xFED4_0000`), but a future upstream device could land on it. |
| A8 | The guest agent maps the page through `/dev/mem` with `O_SYNC`. On arm64 that gives a Device (uncached) mapping, and the page isn't guest RAM. | `pylon/pylon-guest-agent/src/lib.rs` `from_dev_mem` | Read-modify-write atomics on Device memory are not architecturally supported. It only works if stage 2 forces write-back (`FEAT_S2FWB`), and even then by accident (§4.4). |
| A9 | CI excludes `pylond` and `scx-pylon` on aarch64; the aarch64 `quality` jobs have never been run on a real aarch64 machine. | `.github/workflows/ci.yaml` (`pylon_exclude`); spec 05 *CI Coverage* | Regressions go unnoticed. |

**Reference host** (the dev host since 2026-10-30): Cortex-X925 + Cortex-A725
(heterogeneous, big.LITTLE-style; **no SMT**), Linux 6.17, `/dev/kvm`,
`sched_ext` present, BPF LSM **not** in `/sys/kernel/security/lsm`, CPU
features `wfxt`, `ecv`, `atomics` (LSE).

---

## 2. Goals & Non-Goals

**Goals**
1. `pylond`, `scx-pylon`, the VMM hooks and the guest agent work on an
   aarch64 KVM host with an aarch64 Linux guest, with the same behaviour as
   on x86_64: forced parks, the 8 µs grace window, metering, and timer
   wakeups.
2. One counter abstraction ("host counter ticks") shared by every component,
   with no behaviour change on x86_64.
3. No shared-memory layout change. ABI v2 guests and `pylond` stay compatible
   on x86_64.
4. aarch64 builds, unit tests and the KVM end-to-end test run in CI.

**Non-Goals (this spec)**
- MSHV on aarch64. `--pylon` should be rejected there (same rule as spec 06 §6).
- Nested virtualization or guests that run at EL2. Guests are assumed to use
  the EL1 virtual timer (`CNTV_*`).
- Scheduling that knows big cores from little cores. Metering counts core-µs
  the same on every core (see Q-A3).
- The `pylon-kick` device itself. It is already architecture-neutral (spec 06
  §4.3, §6); this spec only checks it on the GICv3 ITS (task A-T9).
- 32-bit Arm and riscv64.

---

## 3. Time Base: "Host Counter Ticks"

### 3.1 Definition
Every `*_tsc` field in the ABI and every `tsc_khz` value counts **host
counter ticks**:

| Arch | Host counter | Rate | Guest counter |
| :--- | :--- | :--- | :--- |
| x86_64 | TSC (`rdtsc`) | measured (`calibrate_tsc_khz`) | guest TSC (`rdtsc` in the guest) |
| aarch64 | `CNTVCT_EL0` (`mrs`, after an `isb`) | `CNTFRQ_EL0` / 1000 | guest `CNTVCT_EL0` |

On aarch64 Linux the host's `CNTVCT_EL0` has offset 0 (the vDSO relies on
this), so every host process reads the same value. A guest's virtual counter
is the host counter minus the VM's counter offset, so the existing
`guest_tsc_offset` / `guest_to_host_tsc` conversion works unchanged.

**Decision:** keep the field names and memory layout. Only the doc comments
change ("TSC" becomes "host counter, see spec 07 §3"). `PYLON_ABI_VERSION`
stays `2`: nothing changes on x86_64, and there is no earlier aarch64 ABI to
be compatible with. Renaming the fields is left to Q-A1.

### 3.2 Shared helper
A new `no_std` module, `pylon_abi::clock`, replaces the three copies (in
`pylond`, the VMM, and the guest agent):

```rust
/// Reads the host (or, in a guest, the guest) counter.
pub fn read_counter() -> u64;
/// Counter rate in kHz if the architecture defines it (aarch64: CNTFRQ_EL0),
/// `None` where it has to be measured (x86_64).
pub fn architectural_counter_khz() -> Option<u64>;
```

- aarch64: `isb; mrs {}, cntvct_el0`. The `isb` stops the read from being
  executed early, out of order. With `ecv`, `CNTVCTSS_EL0` (self-synchronizing)
  can replace it later as an optimisation.
- x86_64: `_rdtsc()`, exactly as today.
- Other architectures: keep the existing `0` / `None` stubs.

`pylon-abi` is `no_std` and uses no `unsafe` today. The helper needs inline
asm. Each `unsafe` block gets a `SAFETY:` comment, and the module sits
behind a default-on `clock` feature so a consumer that only needs the layout
can leave it off. (If owners would rather keep the ABI crate free of `asm`,
put it in a new `pylon-clock` crate instead; see Q-A2.)

### 3.3 Fixed constants that assume 3 GHz
- `DEFAULT_TSC_KHZ = 3_000_000` → on aarch64 use `architectural_counter_khz()`.
  The fallback constant is only for x86.
- `DEFAULT_TPAUSE_CYCLES = 1000` is ~333 ns at 3 GHz but 1 µs at a 1 GHz
  counter (Armv8.6+ requires 1 GHz). Express the sweep interval in
  nanoseconds and convert it with the host rate at start-up.
- Audit every other fixed cycle count (`FORCE_HANDOFF_US`, re-signal
  interval). They must all come from µs × rate (`us_to_cycles`).

---

## 4. Component Changes

### 4.1 `pylond`
1. Use `pylon_abi::clock` for `read_tsc` and the start-up rate; on aarch64
   skip `calibrate_tsc_khz`.
2. **Timed wait.** On aarch64 with `HWCAP2_WFXT` (from `getauxval(AT_HWCAP2)`),
   pause with `WFET <Xt>` until `now + interval`. `WFET` compares against
   `CNTVCT_EL0`, which is the clock `pylond` already reads. Encode it with
   `.inst` if the assembler lacks the `wfxt` feature; **check the encoding
   against the Arm ARM** in the unit test's review. Without `WFXT`, fall back
   to `core::hint::spin_loop()` (`isb` on aarch64). Rename `has_waitpkg` →
   `has_timed_wait`.
3. **Engine core.** Plane 2 pins itself to CPU 1. On a heterogeneous host,
   whether CPU 1 is a big or a little core changes the polling latency. Make
   the engine CPU a flag (`--engine-cpu`, default 1) rather than guessing;
   the choice of default is Q-A3.
4. `scx_pylon` attach and the core-scheduling guard. With no SMT,
   `PR_SCHED_CORE_GET` fails with `ENODEV` and the guard stays off. Check that
   `pylond` then doesn't try to load the `lsm/task_prctl` program, since the
   BPF LSM isn't enabled on the reference host.

### 4.2 VMM (`vmm/src/pylon.rs`, `hypervisor`)
1. `read_host_tsc` → `pylon_abi::clock::read_counter()` on every arch. This
   fixes A3.
2. `read_guest_tsc` on aarch64 → `Vcpu::get_cntvct()` (already in
   `hypervisor/src/cpu.rs`, KVM only). `measure_guest_tsc_offset` stays as
   it is.
3. `read_guest_tsc_deadline` on aarch64 → read the guest's EL1 virtual
   timer:
   - `CTL = KVM_REG_ARM_TIMER_CTL` = `ARM64_SYS_REG(3,3,14,3,1)`
   - `CVAL = KVM_REG_ARM_TIMER_CVAL` = `ARM64_SYS_REG(3,3,14,0,2)`. Its encoding
     is swapped with `TIMER_CNT`'s; see the comment at
     `hypervisor/src/kvm/mod.rs:156`.
   - Return `Some(CVAL)` if `CTL.ENABLE == 1 && CTL.IMASK == 0`, otherwise
     `Some(0)` ("no timer"), matching the x86 meaning of `IA32_TSC_DEADLINE`.

   Add one hypervisor accessor, `Vcpu::get_vtimer() -> Result<(u64 /*ctl*/,
   u64 /*cval*/)>`, behind `cfg(all(target_arch = "aarch64", feature =
   "kvm"))`, next to `get_cntvct`. MSHV doesn't need it (non-goal).
4. Reject `--pylon` with a clear error on MSHV/aarch64. (Spec 06 §6 already
   asks for the same on MSHV in general until its Q4 is answered.)
5. Change the `cfg(target_arch = "x86_64")` VMM tests in `pylon.rs` to cover
   both arches where the logic is generic (`park_decision`,
   `guest_timer_to_host`, `measure_guest_tsc_offset`).

### 4.3 Guest address on aarch64
- Add `PYLON_SHARED_START = 0xFEDF_0000` (size `PYLON_SHARED_PAGE_SIZE`) to
  `arch/src/aarch64/layout.rs`, documented inside the 32-bit reserved hole, and
  assert at compile time that it doesn't overlap `TPM_START..+TPM_SIZE` or any
  other declared range. Mirror it in `arch/src/x86_64/layout.rs` (this also
  closes spec 06 §9's first point for x86 in the short term).
- The page stays out of the FDT `memory` node (as K4 does for E820 on x86).

### 4.4 How the guest maps the page (memory attributes)
On arm64, `/dev/mem` maps anything that isn't kernel RAM as Device-nGnRE
(`phys_mem_access_prot` → `pgprot_noncached`), with or without `O_SYNC`.
The final memory type is the combination of the guest's stage 1 and KVM's
stage 2:
- **With `FEAT_S2FWB`** (KVM uses it when the CPU has it), stage 2 forces
  Normal write-back for memslot memory. Atomics and coherency then work,
  even though the guest asked for Device.
- **Without `FEAT_S2FWB`**, the result is Device memory. LSE atomics and
  exclusives there are not architecturally supported (fault or
  CONSTRAINED UNPREDICTABLE), and the guest's accesses aren't coherent with
  `pylond`'s cached view.

**Decision:**
- Short term (this spec): support only hosts with `FEAT_S2FWB`. The VMM
  checks for it at start-up and rejects `--pylon` on aarch64 without it.
  KVM doesn't report FWB to userspace, and the kernel doesn't expose
  `ID_AA64MMFR2_EL1.FWB` through `HWCAP`s or sysfs. Finding a reliable check
  is part of task A-T5 (Q-A4). If there is none, it becomes a documented host
  requirement instead of a runtime check.
- The guest agent keeps `/dev/mem` but uses only plain loads and stores on the
  page, on every architecture. It used to bump `guest_heartbeat` with
  `fetch_add`, which is an atomic read-modify-write (`ldadd` with LSE); it is
  now a load and a store, since the agent is the field's only writer. A test
  keeps read-modify-write methods out of the agent's code (A-T4).
- Long term: spec 06 §9 (page in a `pylon-kick` prefetchable BAR, mapped by
  `pv-pylon` with `memremap(MEMREMAP_WB)`) removes the problem.

### 4.5 `scx-pylon`
1. `build.rs`: take the multiarch include directory and `-D__TARGET_ARCH_{x86,arm64}`
   from `CARGO_CFG_TARGET_ARCH`.
2. `vmlinux.h`: add `src/bpf/arm64/vmlinux.h` next to the x86 one (moved to
   `src/bpf/x86/vmlinux.h`), generated with
   `bpftool btf dump file /sys/kernel/btf/vmlinux format c`, and trimmed the
   same way the x86 one was. `build.rs` picks the directory. The program only
   uses `struct_ops`, maps and `fentry` (no `pt_regs` access), so CO-RE
   relocations absorb any layout differences; the check is that the object
   loads and attaches.
3. Run `tests/live_attach.rs` on the reference host.

### 4.6 Guest side
- Guest agent: `head_wait_tsc` from `pylon_abi::clock::read_counter()`
  (guest `CNTVCT_EL0`). This fixes A5.
- `pv-pylon` (spec 06 S5): use the kernel's `arch_timer_read_counter()` /
  `get_cycles()` from the start, so it is portable.

### 4.7 Kick path on the GICv3 ITS (validation only)
Spec 06 S4 runs on aarch64 unchanged: MSI-X through the GICv3 ITS, which is
on by default for Cloud Hypervisor on aarch64 KVM. Check that per-vCPU
affinity (managed MSI-X) lands on the intended vCPU, and measure the
kick→`PARKED` latency against the 8 µs window.

---

## 5. Architecture & Backend Considerations

- **x86_64:** no behaviour change. Every x86 path goes through the same
  helper, and the existing tests must pass unchanged.
- **aarch64 / KVM:** the target of this spec.
- **aarch64 / MSHV:** `--pylon` rejected (§4.2).
- **riscv64:** keeps the stubs; out of scope. The helper's fallback keeps it
  building.
- **musl:** `pylond` and `scx-pylon` stay excluded on musl (a `libelf`/`zlib`
  linking issue, not an arch one).

---

## 6. Lifecycle

Same as spec 06 §7. On aarch64, snapshot/restore changes the VM's counter
offset (`set_cntvct` on restore). That is already a reason to reject
snapshots with `--pylon`, and this spec doesn't change it. If that rule is
ever lifted, `guest_tsc_offset` must be measured again after restore.

---

## 7. Verification

| Level | Command / procedure | Host |
| :--- | :--- | :--- |
| Unit | `cargo test -p pylon-abi -p pylond -p pylon-guest-agent` | aarch64 + x86_64 |
| Unit | `cargo test -p vmm --features kvm --lib pylon` | aarch64 + x86_64 |
| Hypervisor | `cargo test -p hypervisor --features kvm` (new `get_vtimer` test on a real vCPU) | aarch64 |
| BPF | `cargo test -p scx-pylon` and `tests/live_attach.rs` | aarch64 (`sched_ext`) |
| Lint | `cargo clippy --locked --all --all-targets --tests --features kvm -- -D warnings` | aarch64 |
| E2E | `cargo test -p pylond --test e2e_pylon` with a real aarch64 KVM VM: forced parks with `state == PARKED`; timer wakeup of a zero-core VM | aarch64 |
| Perf | `pylon-bench --quick`; record the counter rate, timed-wait availability and mean/p99 scale-out | aarch64 |

---

## 8. Implementation Steps

Each step is a single reviewable commit series and leaves the tree working on
both architectures. A-T1 comes first; after it, A-T2 to A-T6 can run in
parallel.

| Step | Scope | Fixes | Depends on |
| :--- | :--- | :--- | :--- |
| **A-T0** | Baseline: build and run the existing tests natively on the reference host; record the failures in §9's notes. | — | — |
| **A-T1** | `pylon_abi::clock` (§3.2); ABI doc comments (§3.1); spec 01 *Time bases* update. | — | A-T0 |
| **A-T2** | `pylond`: counter, rate, ns-based sweep interval, `WFET` timed wait, `--engine-cpu` (§3.3, §4.1). | A1, A2 | A-T1 |
| **A-T3** | VMM: host counter, `get_cntvct`, `get_vtimer` accessor, timer publication, MSHV rejection, test changes (§4.2). | A3, A4 | A-T1 |
| **A-T4** | Guest agent counter (§4.6) and no-RMW test (§4.4). | A5 | A-T1 |
| **A-T5** | Layout constant and overlap asserts on both arches (§4.3); `FEAT_S2FWB` check (§4.4). | A7, A8 (short term) | A-T0 |
| **A-T6** | `scx-pylon` arm64 build and `vmlinux.h`; live attach (§4.5); guard check (§4.1 item 4). | A6 | A-T0 |
| **A-T7** | CI: drop the aarch64 `pylon_exclude` for gnu, add aarch64 unit tests; update spec 05 *CI Coverage*. | A9 | A-T2, A-T3, A-T6 |
| **A-T8** | E2E and perf on aarch64 (§7); Pylon book (`docs/pylon/`) platform page. | — | A-T2…A-T6 |
| **A-T9** | Kick path on the GICv3 ITS (§4.7). | — | spec 06 S4 |

---

## 9. Task Board

> **Rules for agents.**
> 1. **Claim** a task by setting *Owner* and *Branch* and moving it to
>    `IN_PROGRESS` in your branch's first commit. Only claim a task whose
>    *Depends on* tasks are `DONE`, or `IN_REVIEW` if you build on their branch
>    and say so in the notes.
> 2. **Edit only your own row** and your own notes subsection. To ask another
>    task's owner something, add an entry under §9.2 *Handoffs & questions*.
> 3. **Branch names:** `pylon/a64-<task-id>-<slug>` (e.g. `pylon/a64-t2-pylond-counter`).
>    One PR per task against `CMKL-APEX/pylon` `main`. Never against upstream.
> 4. **Done** means: the verification column passes on aarch64 **and** x86_64
>    (or the row says why one arch doesn't apply), the PR is merged, and the
>    *Result* column links the PR and states the test result.
> 5. If you find something out of scope, add it to §9.3 instead of fixing it
>    in passing.
>
> **Statuses:** `NOT_STARTED` · `IN_PROGRESS` · `BLOCKED (<reason>)` · `IN_REVIEW` · `DONE`

### 9.1 Board

| ID | Task | Depends on | Owner | Branch / PR | Status | Verification | Result |
| :- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| A-T0 | Native aarch64 baseline (build + existing tests) | — | Akkarit Sangpetch (Claude Code) | `pylon/a64-t0-baseline` (#22) | `DONE` | `cargo build --all --features kvm`; `cargo test -p pylon-abi -p pylond -p pylon-guest-agent -p scx-pylon`; `cargo test -p vmm --features kvm --lib pylon` | Build and `scx-pylon` pass; 4 `pylond` tests and one clippy lint fail on aarch64 (see notes). Merged in #22. |
| A-T1 | `pylon_abi::clock` + ABI docs + spec 01 | A-T0 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t1-clock` | `IN_PROGRESS` | `cargo test -p pylon-abi` (both arches); x86 tests in other crates unchanged | — |
| A-T2 | `pylond` counter, ns sweep, `WFET`, `--engine-cpu` | A-T1 | *Unassigned* | — | `NOT_STARTED` | `cargo test -p pylond`; unit test that a timed wait returns after ≥ interval | — |
| A-T3 | VMM counter, `get_vtimer`, timer publication, MSHV reject | A-T1 | *Unassigned* | — | `NOT_STARTED` | `cargo test -p vmm --features kvm --lib pylon`; `cargo test -p hypervisor --features kvm` | — |
| A-T4 | Guest agent counter + no-RMW test | A-T1 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t4-guest-agent` | `IN_REVIEW` | `cargo test -p pylon-guest-agent` | — |
| A-T5 | Layout constants, overlap asserts, `FEAT_S2FWB` check | A-T0 | *Unassigned* | — | `NOT_STARTED` | `cargo test -p arch`; `cargo test -p vmm --features kvm --lib pylon` | — |
| A-T6 | `scx-pylon` arm64 build, `vmlinux.h`, live attach, guard | A-T0 | *Unassigned* | — | `NOT_STARTED` | `cargo test -p scx-pylon`; `tests/live_attach.rs` on the reference host | — |
| A-T7 | CI for aarch64 Pylon crates | A-T2, A-T3, A-T6 | *Unassigned* | — | `NOT_STARTED` | Green aarch64 `quality` + unit-test jobs | — |
| A-T8 | aarch64 E2E + perf + book page | A-T2…A-T6 | *Unassigned* | — | `NOT_STARTED` | `cargo test -p pylond --test e2e_pylon`; `pylon-bench --quick` | — |
| A-T9 | Kick path on GICv3 ITS | spec 06 S4 | *Unassigned* | — | `BLOCKED (spec 06 S4)` | Spec 06 §8 S4/S6 checks on aarch64 | — |

### 9.2 Handoffs & Questions

Add entries as `- YYYY-MM-DD, <from task> → <to task or Coordinator>: <message>`.
The addressee answers inline and marks the entry **resolved**.

- *(none yet)*

### 9.3 Found Along the Way

Issues discovered during this work that are outside its scope. Give each one
an ID (`A-F1`, `A-F2`, …) and a pointer to evidence.

- *(none yet)*

### 9.4 Task Notes

Each owner keeps a subsection here: interface notes for dependent tasks,
deviations from the design (and why), and test logs.

#### A-T0
Run on `origin/main` at `7bf5f80be`, native aarch64 (20 CPUs, one socket, kernel `7.0.0-1019-nvidia`). Needs `libelf-dev` installed (`libbpf-sys` vendored build).

| Step | Result |
| :--- | :--- |
| `cargo build --all --features kvm` | pass |
| `cargo test -p pylon-abi` | pass (3) |
| `cargo test -p pylon-guest-agent` | pass (3) |
| `cargo test -p scx-pylon` | pass (10; `live_attach` is `#[ignore]`, needs root, **not run**) |
| `cargo test -p vmm --features kvm --lib pylon` | pass (10) |
| `cargo test -p hypervisor --features kvm` | pass (10) |
| `cargo test -p pylond --no-fail-fast` | **4 fail**, rest pass |
| `cargo clippy --locked --all --all-targets --tests --features kvm -- -D warnings` | **1 error** |

Failures, all expected from gaps A1/A2 and fixed by A-T1/A-T2:
- `tests::test_calibrated_tsc_rate_is_plausible` (`pylond/src/lib.rs:1651`): `calibrate_tsc_khz` returns `None` off x86_64 (A1).
- `e2e_pylon`, 3 of 6 fail: `test_e2e_full_stack_multi_vm_scale_out_and_cooperative_revocation` times out waiting for a core handoff; `test_e2e_zero_core_hibernation_timer_and_rx_doorbell_wakeup` gets `255` (unassigned) where it expects pCPU `2`; `test_e2e_observability_metering_and_socket_stats` sees `allocated_core_microseconds == 0`. These match a clock stuck at 0 (A1). Not yet confirmed by changing the clock.
- clippy `absolute_paths` at `pylond/src/engine.rs:140`: `core::hint::spin_loop()` in the non-x86_64 stub of `pause_cycles`. The lint only fires on aarch64; A-T2 replaces that code. With `--keep-going` this was the only error reported, but crates that depend on `pylond` were not linted past it, so re-run clippy after A-T2.

Host facts (for A-T1/A-T2/A-T5/A-T6):
- `CNTFRQ_EL0` = 1,000,000,000 Hz; 100 ms measured as 100.07 M ticks.
- CPUs: Cortex-X925 + Cortex-A725, no SMT. `/proc/cpuinfo` lists `wfxt`, `ecv`, `atomics`; `HWCAP2` bit 31 (`WFXT`) is set.
- `/sys/kernel/sched_ext` and `/sys/kernel/btf/vmlinux` exist. The BPF LSM is **not** in `/sys/kernel/security/lsm` (`lockdown,capability,landlock,yama,apparmor,ima,evm`).
- The build did not fail on the x86 `vmlinux.h` or the `x86_64-linux-gnu` include path (A6), but nothing loaded the BPF object, so A6 stays open until A-T6's live attach.
- An earlier run on kernel `6.17.0-1021-nvidia` failed only because `libelf-dev` was missing; the dev host's kernel has since changed to 7.0.0-1019.

#### A-T1
Interface for A-T2, A-T3 and A-T4:
- `pylon_abi::clock::read_counter() -> u64` and `architectural_counter_khz() -> Option<u64>`, behind the default-on `clock` feature of `pylon-abi`. Q-A2 resolved as "in `pylon-abi`, behind a feature".
- `architectural_counter_khz()` is `None` on x86_64 (keep `calibrate_tsc_khz` there) and also on aarch64 if `CNTFRQ_EL0` is 0.
- On aarch64 the read is `isb; mrs cntvct_el0`. The `CNTVCTSS_EL0` optimisation (spec §3.2) isn't done.
- The old per-crate copies (`pylond/src/engine.rs` `read_tsc`, `vmm/src/pylon.rs` `read_host_tsc`, `pylon-guest-agent/src/lib.rs`) are untouched: A-T2, A-T3 and A-T4 switch them over.
- No layout change and no `PYLON_ABI_VERSION` bump; only doc comments and spec 01 *Time bases*.

Verification (aarch64 host): `cargo test -p pylon-abi` passes (5; with `--no-default-features` the 3 layout tests), clippy `-D warnings` is clean for both feature sets. x86_64 was only **compiled** (`cargo clippy --target x86_64-unknown-linux-gnu`, after `rustup target add x86_64-unknown-linux-gnu`), not run, because this host can't execute x86_64. CI must run the x86_64 tests.

#### A-T4
Built on `main` after A-T1 (#23); independent of A-T2 and A-T3.

What changed in `pylon-guest-agent`:
- `head_wait_tsc` is `pylon_abi::clock::read_counter()` (the guest's `CNTVCT_EL0` on aarch64) instead of the constant `1` (A5). A counter that reads 0 is stored as 1, because 0 means "no backlog".
- **Correction to §4.4:** the agent did do a read-modify-write on the page: `guest_heartbeat.fetch_add(1)`. On aarch64 that is an atomic (`ldadd`) on a `/dev/mem` mapping, which is device memory unless stage 2 forces write-back (`FEAT_S2FWB`). It is now a load and a store; the agent is the only writer of the field. The spec's earlier "today it does none" was wrong, and §4.4 now says so.
- Tests: `test_head_wait_is_a_counter_reading` (the stored value lies between two counter reads) and `test_agent_does_no_read_modify_write_on_the_page`. The second scans the agent's own source (everything before `#[cfg(test)]`) for `fetch_*`, `swap` and `compare_exchange*`. It's a source scan, not an instruction-level check: it keeps the obvious regressions out and can't see an RMW inside a dependency. I confirmed it fails when a `fetch_add` is put back.

Verification (aarch64 host): `cargo test -p pylon-guest-agent` passes (5), clippy `-D warnings` and `cargo +nightly fmt --all --check` are clean. x86_64 was compiled and linted (`cargo clippy --target x86_64-unknown-linux-gnu`) but not run, since this host can't execute x86_64.

Not covered: the agent running in an aarch64 guest on `/dev/mem`. Whether that mapping works at all depends on A-T5 (`FEAT_S2FWB`) and on the guest kernel allowing `/dev/mem` for that range (`CONFIG_STRICT_DEVMEM`), which is for A-T8.

---

## 10. Open Questions

- **Q-A1:** Rename the `*_tsc` fields (and `tsc_khz`) to arch-neutral names
  (`*_ticks`, `counter_khz`)? That is a source-level break for every
  consumer, but not a layout change. Default: no, until spec 06 S5 lands, to
  avoid churn under in-flight work.
- **Q-A2:** Put the clock helper in `pylon-abi` (behind a feature) or in a new
  `pylon-clock` crate?
- **Q-A3:** On heterogeneous hosts, which core class should run `pylond`'s
  engine, and should metering weight big and little cores differently?
- **Q-A4:** Can `FEAT_S2FWB` be detected reliably from userspace on the host,
  or does it have to be a documented host requirement?
- **Q-A5:** Should hosts without `WFXT` (before Armv8.7) be supported at all,
  given that the fallback is a hot spin?
