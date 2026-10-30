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
- Short term (this spec): `--pylon` on aarch64 needs a host with `FEAT_S2FWB`
  for the guest's `/dev/mem` mapping to be coherent. This is a **documented
  host requirement, not a runtime check**: A-T5 found no reliable way to test
  for it from userspace (Q-A4). `ID_AA64MMFR2_EL1.FWB` reads 0 through the
  kernel's `MRS` emulation even on a core that must implement it, because the
  kernel hides that field, and neither `HWCAP`s, `/proc/cpuinfo` nor sysfs
  report it. Every Armv8.4-A or later core has it, which covers current
  server Arm parts.
- The guest agent keeps `/dev/mem` but uses only plain loads and stores on the
  page, on every architecture. It used to bump `guest_heartbeat` with
  `fetch_add`, which is an atomic read-modify-write (`ldadd` with LSE); it is
  now a load and a store, since the agent is the field's only writer. A test
  keeps read-modify-write methods out of the agent's code (A-T4).
- Long term: spec 06 §9 (page in a `pylon-kick` prefetchable BAR, mapped by
  `pv-pylon` with `memremap(MEMREMAP_WB)`) removes the problem.

### 4.5 `scx-pylon`
1. `build.rs`: take the multiarch include directory and `-D__TARGET_ARCH_{x86,arm64}`
   from `CARGO_CFG_TARGET_ARCH` (done, A-T6).
2. `vmlinux.h`: **one header serves both architectures; no arm64 copy.** The
   program reads kernel types only through CO-RE (the header carries
   `preserve_access_index`), and sched_ext's types and flags are the same on
   both. A-T6 checked this against the aarch64 kernel's BTF (see the A-T6
   notes): the CO-RE relocations resolve, `struct sched_ext_ops` has the same
   42 members, and the five sched_ext enums the program uses are identical.
   If a future kernel changes a type the program uses, regenerate the one
   header with `bpftool btf dump file /sys/kernel/btf/vmlinux format c`.
3. Run `tests/live_attach.rs` on the reference host (needs root; done, A-T6).

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
| **A-T7** | CI: build, lint and test the Pylon crates on a native aarch64 runner (`pylon-aarch64` in `pylon-ci.yaml`); update spec 05 *CI Coverage*. | A9 | A-T2, A-T3, A-T6 |
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
| A-T1 | `pylon_abi::clock` + ABI docs + spec 01 | A-T0 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t1-clock` (#23) | `DONE` | `cargo test -p pylon-abi` (both arches); x86 tests in other crates unchanged | — |
| A-T2 | `pylond` counter, ns sweep, `WFET`, `--engine-cpu` | A-T1 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t2-pylond-counter` (#24) | `DONE` | `cargo test -p pylond`; unit test that a timed wait returns after ≥ interval | — |
| A-T3 | VMM counter, `get_vtimer`, timer publication, MSHV reject | A-T1 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t3-vmm-counter` (#25) | `DONE` | `cargo test -p vmm --features kvm --lib pylon`; `cargo test -p hypervisor --features kvm` | — |
| A-T4 | Guest agent counter + no-RMW test | A-T1 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t4-guest-agent` (#26) | `DONE` | `cargo test -p pylon-guest-agent` | — |
| A-T5 | Layout constants, overlap asserts, `FEAT_S2FWB` check | A-T0 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t5-layout` (#27) | `DONE` | `cargo test -p arch`; `cargo test -p vmm --features kvm --lib pylon` | — |
| A-T6 | `scx-pylon` arm64 build, `vmlinux.h`, live attach, guard | A-T0 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t6-scx-arm64` (#28) | `DONE` | `cargo test -p scx-pylon`; `tests/live_attach.rs` on the reference host | — |
| A-T7 | CI for aarch64 Pylon crates | A-T2, A-T3, A-T6 | Akkarit Sangpetch (Claude Code) | `pylon/a64-t7-ci` (#29) | `DONE` | Green `pylon-aarch64` job on a hosted arm64 runner | — |
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

#### A-T2
What changed in `pylond`:
- `read_tsc` is now `pylon_abi::clock::read_counter`. `calibrate_tsc_khz` returns `CNTFRQ_EL0` on aarch64 and still measures on x86_64. The name stays to keep the change small.
- `default_counter_khz()` replaces the bare `DEFAULT_TSC_KHZ` as the engine's initial rate, and in the e2e tests and `pylon-bench`: on aarch64 it is the architectural rate, so the 8 µs grace window is 8 µs there too. `DEFAULT_TSC_KHZ` stays as the x86 fallback.
- `DEFAULT_TPAUSE_CYCLES` (1000 ticks) is replaced by `SWEEP_INTERVAL_NS` (333), converted with the host rate when `run` starts. On a 3 GHz TSC this is 999 ticks.
- `detect_waitpkg`/`has_waitpkg` are now `detect_timed_wait`/`has_timed_wait`: `WAITPKG` on x86_64, `HWCAP2_WFXT` on aarch64. `pause_cycles` uses `WFET` there, else `spin_loop()`.
- `WFET` is written as `.inst 0xd5031009` (`wfet x9`). The encoding `0xd5031000 | Rt` was checked by assembling `wfet x0`, `wfet x17` and `wfit x3` with `clang -march=armv8.7-a` and disassembling (`d5031000`, `d5031011`, `d5031023`).
- `--engine-cpu` (default 1, `DEFAULT_ENGINE_CPU`) and `MatrixEngine::set_engine_cpu`. `pylond` warns if it is also in `--cores`. Which CPU class is the right default stays Q-A3.

Verification (aarch64 host): `cargo test -p pylond` passes: 19 lib, 1 bench, 7 main, 6 e2e. Before this change `test_calibrated_tsc_rate_is_plausible` and 3 e2e tests failed (A-T0), so the stuck clock (A1) was the cause. `cargo clippy --locked --all --all-targets --tests --features kvm -- -D warnings` and `cargo +nightly fmt --all --check` are clean, which includes the `absolute_paths` error A-T0 found. New tests: `test_pause_cycles_waits_about_the_interval`, `engine_cpu_defaults_to_the_engine_default`. The rate-plausibility test's range is now 10 MHz to 10 GHz.

x86_64 was not built as `pylond`: cross-building its vendored `libbpf` fails here. The functions that changed were checked as a standalone crate for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` with clippy `-D warnings` (that is how an x86-only unused `spin_loop` import was found and fixed). CI must build and test `pylond` on x86_64.

`WFET` on the reference host (Cortex-X925 core 2, Cortex-A725 core 12; 20,000 waits each; throwaway program, not committed). Overshoot past the deadline in ns, p50 / p99: 67 / 115 at a 333 ns interval, 72 / 120 at 1 µs, 72 / 136 at 5 µs, the same on both core types. The worst case was 30 µs. 0.5% to 18% of waits returned early (more at short intervals), which the polling loop absorbs, so the pause is a hint, not a timer. For comparison, an `isb` spin iteration costs 12 ns. This bears on Q-A5 and on the A-T8 latency numbers.

#### A-T3
Built on `main` after A-T1 (#23); independent of A-T2.

What changed:
- `hypervisor`: `Vcpu::get_vtimer() -> (CNTV_CTL_EL0, CNTV_CVAL_EL0)` (aarch64 + `kvm`; MSHV is `unimplemented!()` like `get_cntvct`), via `KVM_REG_ARM_TIMER_CTL` (`0x6030_0000_0013_df19`) and `KVM_REG_ARM_TIMER_CVAL` (`0x6030_0000_0013_df02`). A unit test pins these ids, and `CNT` (`..._df1a`), to the kernel's `kvm.h` values.
- `vmm/src/pylon.rs`: `read_host_tsc` is `pylon_abi::clock::read_counter` on every arch (A3). `read_guest_tsc` is `get_cntvct`, and `read_guest_tsc_deadline` is `CVAL` when `CTL.ENABLE == 1 && CTL.IMASK == 0`, else 0 (A4). Without `kvm` on aarch64 both still return `None`.
- `pylon::check_hypervisor`, called from `CpuManager::new`: `--pylon` on MSHV on aarch64 fails with "--pylon is not supported on MSHV on aarch64". MSHV on x86_64 is unchanged (spec 06 Q4 still open).
- `test_measure_guest_tsc_offset` now runs on both arches. New: `test_vtimer_deadline_needs_enable_and_no_mask`, `test_real_kvm_vcpu_counter_offset_and_idle_timer` (real vCPU: offset, conversion, idle timer reads 0) and, in `hypervisor`, `test_timer_register_ids` and `test_vtimer_and_counter_read`.

Findings:
- KVM starts a new vCPU's counter near 0, like the x86 TSC: on the reference host `guest_tsc_offset` was about -5.0e12 ticks (the host's uptime) in three runs, so it is never the reserved 0 and the "offset unknown" handling doesn't hide timers on aarch64.
- Converting a fresh guest counter to the host counter is off by 660 to 850 ticks (0.7 to 0.85 µs at 1 GHz): half the duration of the register read between the two host reads. It applies to `head_wait_tsc` and published timer deadlines, against the 8 µs grace window.
- The guest's armed timer is read from the vCPU's saved state outside `KVM_RUN`, as `check_and_park_vcpu` does. Only the EL1 virtual timer is read; a guest using the physical timer (`CNTP_*`) would publish no deadline (Q-A6).

Verification (aarch64 host): `cargo test -p vmm --features kvm --lib` passes (148, 13 `pylon`), `cargo test -p hypervisor --features kvm` passes (8 lib, 4 more). `cargo clippy --locked -p vmm -p hypervisor -p cloud-hypervisor --all-targets -- -D warnings` is clean with `kvm`, `mshv` and `mshv,kvm`, and `cargo +nightly fmt --all --check` is clean.

Not verified: x86_64. `vmm` cross-builds fail here (`zstd-sys` needs an x86_64 C toolchain), so CI must build and test it. The x86 code is unchanged except `read_host_tsc`, the removed `_rdtsc` import and the widened `cfg`s of the non-x86 stubs. The forced-park-by-signal end-to-end test with a running guest exists only for x86_64; an aarch64 equivalent needs a bootable guest and is left to A-T8.

#### A-T4
Built on `main` after A-T1 (#23); independent of A-T2 and A-T3.

What changed in `pylon-guest-agent`:
- `head_wait_tsc` is `pylon_abi::clock::read_counter()` (the guest's `CNTVCT_EL0` on aarch64) instead of the constant `1` (A5). A counter that reads 0 is stored as 1, because 0 means "no backlog".
- **Correction to §4.4:** the agent did do a read-modify-write on the page: `guest_heartbeat.fetch_add(1)`. On aarch64 that is an atomic (`ldadd`) on a `/dev/mem` mapping, which is device memory unless stage 2 forces write-back (`FEAT_S2FWB`). It is now a load and a store; the agent is the only writer of the field. The spec's earlier "today it does none" was wrong, and §4.4 now says so.
- Tests: `test_head_wait_is_a_counter_reading` (the stored value lies between two counter reads) and `test_agent_does_no_read_modify_write_on_the_page`. The second scans the agent's own source (everything before `#[cfg(test)]`) for `fetch_*`, `swap` and `compare_exchange*`. It's a source scan, not an instruction-level check: it keeps the obvious regressions out and can't see an RMW inside a dependency. I confirmed it fails when a `fetch_add` is put back.

Verification (aarch64 host): `cargo test -p pylon-guest-agent` passes (5), clippy `-D warnings` and `cargo +nightly fmt --all --check` are clean. x86_64 was compiled and linted (`cargo clippy --target x86_64-unknown-linux-gnu`) but not run, since this host can't execute x86_64.

Not covered: the agent running in an aarch64 guest on `/dev/mem`. Whether that mapping works at all depends on A-T5 (`FEAT_S2FWB`) and on the guest kernel allowing `/dev/mem` for that range (`CONFIG_STRICT_DEVMEM`), which is for A-T8.

#### A-T5
Built on `main` after A-T0 (#22); independent of A-T1 to A-T4.

Layout (A7):
- `arch::layout::PYLON_SHARED_START` (`0xFEDF_0000`) and `PYLON_SHARED_SIZE` (`0x2000`) on both x86_64 and aarch64, with compile-time asserts. Both: 64 KiB aligned (a guest with 64 KiB pages can map it), and no overlap with the TPM. x86_64: also clear of the IOAPIC, below the APIC and 4 GiB, and above the KVM identity map. aarch64: inside the 32-bit reserved hole. I checked that each assert fires by moving the address onto the IOAPIC (x86_64) and the TPM (aarch64).
- `vmm/src/pylon.rs` asserts at compile time that these equal `pylon_abi::PYLON_DEFAULT_GPA` and `PYLON_SHARED_PAGE_SIZE`; `arch` and `pylon-abi` can't see each other, so the VMM, which sees both, checks. Gated to x86_64 and aarch64 so the riscv64 build isn't affected.
- On x86_64 the page lies outside the range `MEM_32BIT_RESERVED_*` declares (that ends at `0xF800_0000`), as the TPM, IOAPIC and APIC do, so those asserts are the only protection against a future device landing on it. The page is still not in the FDT `memory` node or any RAM listing (K4).
- No behaviour change: nothing moved, and `gpa=` still overrides the default.

`FEAT_S2FWB` (A8, Q-A4): not detectable, so a documented requirement (§4.4). Evidence on the reference host (Cortex-X925, kernel `7.0.0-1019-nvidia`): `mrs ID_AA64MMFR2_EL1` from EL0 gives `0x0000000100000000`, with `FWB[43:40]` = 0 while `AT[35:32]` = 1 is visible; `ECV` in `ID_AA64MMFR0_EL1` reads 2 and `WFxT` is advertised, so the core is at least Armv8.7 and must implement `FEAT_S2FWB`. No runtime check or `--pylon` rejection was added, because a check that reads 0 on a core that has the feature would reject valid hosts. With A-T4 the agent no longer does atomic read-modify-writes on the page, so a host without `FEAT_S2FWB` loses coherency guarantees and not correctness of the agent's code. Still to do (A-T8): a note in the book and a check in a real guest.

Verification (aarch64 host): `cargo test -p arch --features kvm` passes (17); `cargo test -p vmm --features kvm --lib pylon` passes (10); `cargo clippy --locked -p arch -p vmm --all-targets --features vmm/kvm,arch/kvm -- -D warnings`, the same for `arch` on `x86_64-unknown-linux-gnu`, and `cargo +nightly fmt --all --check` are clean. `vmm` for x86_64 can't be cross-built here (`zstd-sys`), so CI covers the x86_64 side of the VMM's assert.

#### A-T6
Built on `main` after A-T0 (#22); independent of A-T1 to A-T5.

What changed: `scx-pylon/build.rs` takes `-D__TARGET_ARCH_{x86,arm64}` and the multiarch include directory from `CARGO_CFG_TARGET_ARCH` instead of the hard-coded `/usr/include/x86_64-linux-gnu`. An architecture it doesn't know gets neither, as before. x86_64 gets the same include directory and the new define, which `bpf_tracing.h` only uses for its `PT_REGS` macros.

**No arm64 `vmlinux.h`** (A6; §4.5 item 2 was planned the other way). Checked offline, against the reference host's kernel BTF (`7.0.0-1019-nvidia`, aarch64), for the object built on aarch64 from the existing header:
- `bpftool gen min_core_btf /sys/kernel/btf/vmlinux out.btf scx_pylon.bpf.o` succeeds, and the output holds `task_struct` with the 5 fields the program reads (`pid`, `tgid`, `migration_disabled`, `nr_cpus_allowed`, `cpus_ptr`). So every CO-RE relocation resolves.
- `struct sched_ext_ops` has 42 members in the object and in the kernel, and none of the object's is missing in the kernel (struct_ops members are matched by name at load).
- The enums `scx_ops_flags`, `scx_enq_flags`, `scx_kick_flags`, `scx_dsq_id_flags` and `scx_public_consts` are identical in the checked-in header (from an x86_64 7.0.0-42 kernel) and the aarch64 kernel's dump, so the flag values compiled in are right.

Guard (§4.1 item 4): on the reference host `PR_SCHED_CORE_GET` fails with `ENODEV` and `/sys/devices/system/cpu/smt/active` is 0, so `KernelCoreSched::detect()` is `NoSmt` and `needs_guard(Auto)` is false: `pylond` doesn't ask for the `lsm/task_prctl` hook, so the missing BPF LSM doesn't matter. This is by reading the code and the two probes, not by running `pylond` (needs root). With `--core-sched-guard on` the guard is asked for on a no-SMT host, and without BPF LSM the load then fails.

Verification (aarch64 host): `cargo build`, `cargo test -p scx-pylon` (10 passed, `live_attach` ignored) and `cargo clippy --locked -p scx-pylon --all-targets -- -D warnings` are clean, and so is rustfmt.

**Live attach, run as root** (the test binary built as a normal user, then `sudo`; kernel `7.0.0-1019-nvidia`, aarch64): `tests/live_attach.rs` passes. It loads the object built from the existing `vmlinux.h`, attaches it, sees `/sys/kernel/sched_ext/state` go `enabled`, sets and reads back a pCPU owner entry, detaches, and sees `disabled`; the whole test took 1.03 s and the host was back to `disabled` afterwards. So the object loads and attaches on aarch64, which settles A6, and it confirms the offline checks above. It does not exercise scheduling under load or `pylond`'s path (`--enable-scx`); that is for A-T8. x86_64: `scx-pylon` can't be cross-built here (vendored `libbpf`), so CI covers it.

#### A-T7
Built on `main` after A-T6 (#28).

**Deviation from §8:** the plan said to drop the aarch64 `pylon_exclude` in the `quality` job. That job cross-compiles with `houseabsolute/actions-rust-cross` on an x86_64 runner, and the vendored `libbpf` needs the target's `libelf`, which the cross image lacks; so the exclusion stays (only its comment changed, in `quality` and `build`). The Pylon crates get aarch64 coverage from a new native job instead.

What changed: `.github/workflows/pylon-ci.yaml` gets `pylon-aarch64` ("Pylon aarch64 (clippy & unit tests)") on `ubuntu-24.04-arm`, free for this public repo. It installs the same BPF/VMM build dependencies as the x86_64 jobs and runs, with `RUSTFLAGS=-D warnings`:
- clippy `-D warnings` on `pylon-abi`, `pylon-guest-agent`, `pylond`, `scx-pylon`, `arch`, `hypervisor`, `vmm` and `cloud-hypervisor` with `--features kvm --tests`;
- tests: `pylon-abi` and `pylon-guest-agent`; `pylond` (including `e2e_pylon`); `scx-pylon`; `arch --features kvm`; `vmm --features kvm --lib pylon` and `--lib config`.

It is a new job, not a matrix on the existing ones, so the existing check names don't change. It adds `pylon-guest-agent` and `arch`, which the x86_64 Pylon jobs don't run (the x86_64 unit-test job in `ci.yaml` runs the workspace).

Left out on purpose: `hypervisor` tests (hosted arm64 runners have no `/dev/kvm`, and two tests create a VM with `unwrap`), the `pylon-bench` benchmark (shared runners say nothing about latency), and the live `sched_ext` attach (needs root and a `sched_ext` kernel); A-T8 covers the last two on real hardware.

Verification: I ran every command of the job on the aarch64 dev host in a clean target directory with `RUSTFLAGS=-D warnings`: all pass (clippy clean; `pylon-abi`+`pylon-guest-agent` 5+5, `pylond` 19+1+7+6, `scx-pylon` 10, `arch` 17, `vmm` pylon 13 and config 38). With `/dev/kvm` hidden (bind-mounted over with `/dev/null` in a user namespace) the `vmm` real-KVM test prints "Skipping" and passes, as it would on a runner without KVM. The workflow file parses as YAML. **Not verified: the job on a GitHub runner**: its first run is this PR's check. Package or toolchain differences on the hosted image (Ubuntu 24.04 arm) could show up there.

Housekeeping in the same PR: A-T6 is `DONE` (its row was left `IN_REVIEW` when #28 merged).

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
- **Q-A4:** *Resolved by A-T5:* it can't be detected reliably from userspace, so
  it is a documented host requirement (§4.4).
- **Q-A5:** Should hosts without `WFXT` (before Armv8.7) be supported at all,
  given that the fallback is a hot spin?
- **Q-A6:** Should the VMM also read the physical timer (`CNTP_CTL_EL0`/`CNTP_CVAL_EL0`) when it publishes a parked vCPU's timer? Linux guests use the virtual timer by default, and `get_vtimer` reads only that.
