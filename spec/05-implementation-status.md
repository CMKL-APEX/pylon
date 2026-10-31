# Specification 05: Pylon Implementation Status Tracker

> **Instructions for Subagents & Coding Agents:**
> Update your assigned track section in this file as you start, progress through, and complete your tasks (including test results and any interface notes for other tracks). Avoid modifying sections belonging to other tracks while they are `IN_PROGRESS`.

---

## Overall Status

| Track | Branch | Scope | Owner | Status | Verification Command |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Workspace** | `pylon/scaffold` | Root `Cargo.toml` workspace members (`pylon/pylon-abi`, `pylon/pylond`, `pylon/scx-pylon`) & `spec/*` | Coordinator | `DONE` | `cargo metadata --no-deps` |
| **Track 1A** | `pylon/track-1-abi-pylond` | `pylon/pylon-abi`: `#![no_std]` 64B cache-line ABI & compile-time size/offset assertions | Subagent 1 | `DONE` | `cargo test -p pylon-abi` |
| **Track 1B** | `pylon/track-1-abi-pylond` | `pylon/pylond`: Two-plane arbiter daemon (Core 0 Tokio Unix socket + Core 1 `_rdtsc`/`_tpause` engine) | Subagent 1 | `DONE` | `cargo test -p pylond` |
| **Track 2** | `pylon/track-2-scx-pylon` | `pylon/scx-pylon`: `sched_ext` BPF C program (`scx_pylon.bpf.c`) & `libbpf-rs` `BPF_F_MMAPABLE` wrapper | Subagent 2 | `DONE` | `cargo test -p scx-pylon` |
| **Track 3** | `pylon/track-3-vmm-hooks` | `vmm/src/pylon.rs` & surgical hooks in `vmm/src/config.rs`, `vmm/src/memory_manager.rs`, `vmm/src/cpu.rs` | Subagent 3 | `DONE` | `cargo test -p vmm --features kvm` |
| **Integration** | `feat/pylon` | Merge `pylon/track-1-abi-pylond`, `pylon/track-2-scx-pylon`, `pylon/track-3-vmm-hooks` & full test suite | Coordinator | `DONE` | `cargo test -p pylon-abi -p pylond -p scx-pylon && cargo test -p vmm --features kvm` |
| **Track 1C** | `main` (`97379cccd`) | `pylond`: `VmPolicyUpdate` IPC (runtime `min_cores`/`max_cores`/`priority_weight`) | Coordinator | `DONE` | `cargo test -p pylond` |
| **Track 1D** | `feat/pylon-observability-metering` (#8) | `pylond`: core-µs residency metering, `VmStatsRequest`, Prometheus exporter (`--metrics-port`) | Coordinator | `DONE` | `cargo test -p pylond` |
| **Track 2B** | `feat/pylon-scx-kernel-dispatch` (#7) | `scx-pylon`: live `sched_ext` struct_ops attach, `scx_pylon` CLI, `pylond --enable-scx` | Coordinator | `DONE` | `cargo test -p scx-pylon` (live attach in `tests/live_attach.rs` needs a `sched_ext` host) |
| **Track 2C** | `feat/pylond-scx-auto` (#9) | `pylond`: auto-attach `scx_pylon` when `/sys/kernel/sched_ext` exists, falling back to the in-memory owner table; `--no-scx` | Coordinator | `DONE` | `cargo test -p pylond` |
| **Track G** | `main` (`f44b3fd87`); telemetry-only since S3 | `pylon/pylon-guest-agent`: userspace guest telemetry daemon | Coordinator | `DONE` | `cargo test -p pylon-guest-agent`. Telemetry-only since spec 06 S3 (P4 fixed). |
| **E2E** | `main` (`d2c1485b2`, `dff66af86`) | `pylond/tests/e2e_pylon.rs`, real-KVM VMM test, `pylon-bench` (Pylon vs. ACPI hotplug) | Coordinator | `DONE` | `cargo test -p pylond --test e2e_pylon`; `cargo run --release -p pylond --bin pylon-bench -- --quick` |
| **CI** | `main` (`45b35ba6d`, `e03b59cb7`) → re-enable in progress | `.github/workflows/pylon-ci.yaml`; upstream `ci.yaml` targets & feature matrix | Coordinator | `IN_PROGRESS` | See [CI Coverage](#ci-coverage) |
| **Docs** | `docs/pylon-book` (#11) | Pylon book in `docs/pylon/` (mdBook) and `pylon-docs.yaml`, publishing to `gh-pages` | Coordinator | `DONE` | `mdbook build docs/pylon` |
| **Track 4** | S1–S2: `pylon/s1-park-decision` (#12); S3: `pylon/s3-and-fixes` | Spec 06 S1–S4, S6: VMM park decision table, `pylond` forced-park via `tgkill`, guest agent telemetry-only and ABI v2, `pylon-kick` PCI device + MSI-X kick FDs, latency metrics | Coordinator | `IN_PROGRESS` (S1–S4 done, S4 with the provisional PCI ID `0x1AF4:0x10F0` from spec 06 Q1; S6 not started) | `cargo test -p vmm --features kvm --lib pylon`, `cargo test -p pylond`, `cargo test -p pylon-guest-agent`; later steps per `spec/06-guest-kick-path.md §8` |
| **Track 5** | *TBD* | Spec 06 S5: `pylon/guest/linux` `pv-pylon` kernel module | *Unassigned* | `NOT_STARTED` | Guest integration test (spec 06 §8) |
| **Track 6** | `pylon/a64-<task>-<slug>` | Spec 07: aarch64 host & guest support (counter abstraction, `WFET`, guest virtual timer, arm64 `scx-pylon`, CI). Per-task board: `spec/07-aarch64-support.md §9` | Coordinator | `IN_PROGRESS` (A-T0 to A-T7 done; A-T8 in progress; A-T9 can start now that spec 06 S4 is done) | Spec 07 §7 |

---

## Known Issues (Revocation Path)

These were found while drafting `spec/06-guest-kick-path.md`; full details are in §1 of that spec. Until Track 4 S1–S3 land, **do not treat the cooperative revocation path as functional**.

| ID | Summary | Fixed by |
| :- | :--- | :--- |
| P1 | Kick `EventFd`s are sent to `pylond` but never wired to a guest interrupt. | **Fixed** by Track 4 S4 (`pylon-kick`); a guest driver has to enable the vectors (S5) |
| P2 | The HLT fast-park hook sits on `VmExit::Reset`. It never fires under KVM's in-kernel LAPIC, and it can swallow a guest triple-fault reset. | **Fixed** by Track 4 S1 |
| P3 | The VMM parks on any userspace exit while `revoke_requested == 1`, ignoring `in_critical_section`. | **Fixed** by Track 4 S1 |
| P4 | The guest agent marks other vCPUs `PARKED` and clears `revoke_requested` while those vCPUs are still running. | **Fixed** by Track 4 S3 (agent is telemetry-only) |
| P5 | On deadline expiry, `pylond` writes `PARKED`/clears `revoke_requested` for a still-runnable vCPU. | **Fixed** by Track 4 S2 |
| P6 | `PylonVcpuSlot::state` has three writers and no ownership rules. | **Fixed**: `pylond` stopped writing `state` (S2), the guest agent too (S3); ABI v2 documents each field's writer |

**After S3:** a donor that misses its 8 µs deadline is signalled (and re-signalled every 10 µs) until the VMM force-parks it, and the core moves only once the VMM has written `PARKED`, or after 50 µs as a counted *handoff without park*. No component writes a vCPU's `state` against the ownership rules any more. Revocations are still never cooperative in practice, because nothing tells the guest about them (P1, S4) and no guest component writes `PARKING` (S5).

## Known Issues (Other)

Found on 2026-10-29 while writing the Pylon book (`docs/pylon/`). The `K` prefix keeps them apart from spec 06's `P1`–`P6`. K2–K6 are fixed (K4 for the short term); K1 is partly fixed. The book's *Status and roadmap* page describes their user-visible effects.

| ID | Summary | Evidence | Proposed fix |
| :- | :--- | :--- | :--- |
| K1 | No producer for `next_timer_tsc`, `stranded_work_bitmap`, `in_critical_section` or the host RX doorbell (`VmHandle::host_rx_doorbell`). Zero-core wakeups, stranded-work preference and critical-section-aware donor selection never trigger outside tests. | Only test code stores to them (`pylond/src/lib.rs`, `tests/e2e_pylon.rs`). The guest agent writes only `runnable_threads`, `head_wait_tsc` and `guest_heartbeat`. | **Partly fixed**: timers now have a host-side producer. When the VMM parks a vCPU it reads the guest's `IA32_TSC_DEADLINE` and publishes it as `park_timer_deadline_tsc` (host TSC); `pylond` wakes a zero-core VM when one is due and prefers that vCPU. Needs the in-kernel LAPIC and a guest in TSC-deadline mode (Linux's default when offered). **Open**: `in_critical_section` and `stranded_work_bitmap` need guest kernel state (S5, `pv-pylon`); the RX doorbell needs a host-side design (virtio-net/vDPA backend hook). |
| K2 | `PylonHeader::tsc_khz` is hard-coded to `DEFAULT_TSC_KHZ` (3,000,000). The 8 µs grace period and core-µs metering are only correct on a 3 GHz TSC. | `vmm/src/pylon.rs` `PylonVmmState::new`. | **Fixed**: the VMM writes `tsc_khz = 0` (unknown), and `pylond` measures the host TSC rate at start (`calibrate_tsc_khz`) and uses it for such VMs. |
| K3 | Mixed time bases: the guest writes `head_wait_tsc` (and, when implemented, `next_timer_tsc`) from the guest TSC, while `pylond` compares it with the host TSC. Under KVM the two differ by the VM's TSC offset, which skews urgency and would make timer wakeups fire at the wrong time. | `pylon-guest-agent/src/lib.rs` `update_telemetry`; `pylond/src/engine.rs` `evaluate_demand_phase1`. **Fixed**: the guest keeps writing guest TSC; the VMM measures each vCPU's guest TSC offset (an `IA32_TSC` read between two host `rdtsc`s) into `guest_tsc_offset`, and `pylond` converts with `PylonSharedPage::guest_to_host_tsc`. Assumes no TSC scaling. Also, the guest agent now keeps the time a backlog started instead of overwriting it each step, so `head_wait` grows while threads wait. |
| K4 | The shared page is inserted into `GuestMemoryMmap`, so on x86_64 `generate_ram_ranges` reports it to the guest as an 8 KiB E820 RAM range at `0xFEDF_0000`, overlapping the `E820_RESERVED` 32-bit hole. How direct boot, firmware boot and guests resolve the overlap is unverified. | `vmm/src/pylon.rs` `setup_gpa_mapping`; `arch/src/x86_64/mod.rs` `generate_ram_ranges`, `generate_e820_memory_map`. **Fixed** (short term): the page keeps its own hypervisor memory slot but is no longer inserted into `GuestMemoryMmap`, so no RAM listing (E820, PVH memmap, ACPI, hotplug layout) includes it; `MemoryManager` holds the mapping. Not yet checked by booting a guest. Long term: spec 06 §9 (page in the `pylon-kick` BAR). |
| K5 | `pylond` doesn't set its socket's owner or mode, so with root's umask it is `0755` and non-root VMMs can't register. Anyone who can write to it can register VMs, change any VM's policy and read all stats. | `pylond/src/control.rs` `ControlPlane::run` (bind without `chmod`/`chown`). | **Fixed**: `pylond --socket-group <group> --socket-mode <octal>`. |
| K6 | `pylond` never removes a VM. A VM that exits keeps its registration, and managed CPUs assigned to its still-`RUNNING` vCPUs are never reclaimed (reclaim needs `PARKED`). Two VMs with the default `vm_id=1` silently replace each other. | No deregistration path in `pylond/src/control.rs`; `reclaim_idle_assigned_cores` in `engine.rs`; `PylonConfig::parse` default `vm_id = 1`. *(From code reading; not yet observed on a host.)* | **Fixed**: Plane 1 removes VMs whose VMM exited (pidfd, every 1 s), the engine frees CPUs still assigned to removed VMs, and a `vm_id` held by a running VMM is rejected (and logged) instead of replaced. |

Measured context: `pylon-bench` (2026-10-29, AMD EPYC 9124, no `WAITPKG`) shows a 138 µs mean scale-out with threads standing in for vCPUs, far above the ~3–6 µs handoff target in spec 00. The cause hasn't been investigated; see the book's *Performance and benchmarks* page.

---

## Track 1 (`pylon/pylon-abi` & `pylon/pylond`)

- **Status:** `DONE`
- **Checklist:**
  - [x] Implement `pylon/pylon-abi/Cargo.toml` and `pylon/pylon-abi/src/lib.rs` (`#![no_std]`).
  - [x] Add compile-time and unit tests for `PylonHeader` (64B), `PylonTelemetry` (64B), `PylonVcpuSlot` (64B, `revoke_requested` offset 32), and `PylonSharedPage` (8192B).
  - [x] Implement `pylon/pylond` Plane 1 (Core 0 Tokio Unix socket `/run/pylon/pylond.sock` with `SCM_RIGHTS` FD passing and `ArcSwap<Vec<VmHandle>>` publication).
  - [x] Implement `pylon/pylond` Plane 2 (Core 1 `_rdtsc`/`_tpause` 3-phase arbitration loop using `memmap2` and `arc-swap`).
  - [x] Run `cargo test -p pylon-abi` and `cargo test -p pylond`.
- **Notes / Logs:**
  - `cargo test -p pylon-abi -p pylond`: 6 passed (2 in `pylon-abi`, 4 in `pylond`), 0 failed.
  - `cargo clippy -p pylon-abi -p pylond --all-targets`: 0 warnings, 0 errors.
  - Key interfaces exported by `pylond`: `VmRegistrationRequest`, `VmRegistrationResponse`, `VmHandle`, `VmSnapshot` (`ArcSwap<Vec<VmHandle>>`), `PcpuOwnerEntry`, `PcpuOwnerTable`, `CoreState`, `control::ControlPlane`, `control::send_registration`, and `engine::MatrixEngine`.

---

## Track 2 (`pylon/scx-pylon`)

- **Status:** `DONE`
- **Checklist:**
  - [x] Implement `pylon/scx-pylon/src/bpf/scx_pylon.bpf.c` defining `struct pcpu_owner_entry`, `pylon_pcpu_owners` (`BPF_MAP_TYPE_ARRAY | BPF_F_MMAPABLE`, 256 entries), `select_cpu`, and `dispatch`.
  - [x] Implement `pylon/scx-pylon/build.rs` compiling `scx_pylon.bpf.c` via `clang -target bpf`.
  - [x] Implement `pylon/scx-pylon/src/lib.rs` `libbpf-rs` wrapper and `BPF_F_MMAPABLE` userspace view (`PcpuOwnerMap`).
  - [x] Run `cargo test -p scx-pylon`.
- **Notes / Logs:**
  - Implemented `scx_pylon.bpf.c` with `struct pcpu_owner_entry` (8B), `pylon_pcpu_owners` (`BPF_MAP_TYPE_ARRAY`, `max_entries = 256`, `map_flags = BPF_F_MMAPABLE`), `pylon_select_cpu` (`SCX_DSQ_LOCAL` fast-path insertion + bounded pCPU scan), `pylon_dispatch` (donor preemption via `scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT)` and direct `bpf_task_from_pid` dispatch to `SCX_DSQ_LOCAL`), and `SEC(".struct_ops.link") struct sched_ext_ops pylon_ops`.
  - Implemented `build.rs` compiling `src/bpf/scx_pylon.bpf.c` into `$OUT_DIR/scx_pylon.bpf.o` via `clang -target bpf -O2 -g -Wall -Werror`.
  - Implemented `ScxPylonUserspace` (`verify_embedded_object`, `open_embedded`, `load_and_attach`) and zero-syscall `PcpuOwnerMap` (`new_anonymous`, `from_shm`, `from_map_fd`, `set_owner`, `clear_owner`, `get_owner`, `find_pcpu_for_tid`).
  - Verified with `cargo test -p scx-pylon` (4/4 tests passing) and `cargo clippy -p scx-pylon --all-targets` (0 warnings).

---

## Track 3 (`vmm/src/pylon.rs` & VMM Hooks)

- **Status:** `DONE`
- **Checklist:**
  - [x] Create `vmm/src/pylon.rs` encapsulating `PylonConfig`, `PylonVmmState`, `/dev/shm/<vm-id>-pylon` creation & GPA `0xFEDF_0000` mapping, `gettid()` registration, and fast-park wait primitives.
  - [x] Wire `--pylon` config hook into `vmm/src/config.rs` (and `cloud-hypervisor/src/main.rs`).
  - [x] Wire GPA `0xFEDF_0000` shared memory mapping hook into `vmm/src/memory_manager.rs`.
  - [x] Wire vCPU `host_tid` registration and fast-park exit handler hook into `vmm/src/cpu.rs`.
  - [x] Run `cargo test -p vmm --features kvm`.
- **Notes / Logs:**
  - Implemented `PylonConfig` and `PylonVmmState` in `vmm/src/pylon.rs`, including 8,192B `/dev/shm/<vm-id>-pylon` creation/mmap, `PylonHeader` initialization, `GuestRegionMmap` GPA mapping (`0xFEDF_0000`, `MAP_SHARED`, `MemoryVisibility::Shared`), optional `pylond` Unix socket `SCM_RIGHTS` registration, `gettid()` export to `PylonVcpuSlot::host_tid`, initial vCPU parking (`vcpu_id >= initial_active_vcpus`), and `revoke_requested` fast-park/wake lifecycle.
  - Verified with `cargo test -p vmm --features kvm --lib pylon` (6/6 passed), `cargo test -p vmm --features kvm --lib config` (38/38 passed), `cargo test -p cloud-hypervisor --features kvm --bin cloud-hypervisor` (44/44 passed), and `cargo clippy -p vmm -p cloud-hypervisor --features kvm --tests` (0 warnings).

---

## CI Coverage

`e03b59cb7` masked a large part of the upstream `ci.yaml` matrix. On 2026-10-29 it was re-enabled **except** for jobs that need self-hosted runners, because `CMKL-APEX/pylon` has none registered (`gh api repos/CMKL-APEX/pylon/actions/runners` → 0).

**Pylon host crates on cross/musl targets:** `pylond` and `scx-pylon` are glibc host daemons (x86_64 and aarch64). `scx-pylon` builds a BPF object with `clang` and links vendored `libbpf` against `libelf`/`zlib`, and `pylond` depends on it. The cross and musl jobs therefore pass `--exclude scx-pylon --exclude pylond`, through a `pylon_exclude` matrix variable in `quality` and `build`. `scripts/run_unit_tests.sh` already does the same for musl. `pylon-abi`, `pylon-guest-agent`, and `vmm/src/pylon.rs` are still built on every target. aarch64 gnu gets them natively instead: `pylon-ci.yaml`'s `pylon-aarch64` job runs on a GitHub-hosted `ubuntu-24.04-arm` runner (spec 07 A-T7).

| Job / step | Status | Verified locally (2026-10-29) |
| :--- | :--- | :--- |
| `quality` x86_64-gnu: clippy `mshv`, `mshv,kvm`, `pvmemcontrol`, `fw_cfg`, `ivshmem`, `kvm,sev_snp`, `mshv,sev_snp`, `mshv,igvm,sev_snp`, `kvm,igvm`, `mshv,igvm`, `kvm,igvm,sev_snp,fw_cfg`, `sev_snp,igvm,fw_cfg` | Re-enabled | ✅ All 12 pass `cargo clippy --locked --all --all-targets --tests --examples -- -D warnings` |
| `quality` x86_64-musl | Re-enabled (Pylon host crates excluded) | ✅ `cargo check` of `pylon-abi`, `pylon-guest-agent`, `vmm`, `cloud-hypervisor` for the musl target |
| `quality` aarch64-gnu / aarch64-musl | Re-enabled (Pylon host crates excluded) | ⚠️ The cross jobs themselves are not verified (no aarch64 sysroot/`cross` on the dev host). The Pylon crates are built, linted and tested natively on aarch64 by `pylon-aarch64` (see below). |
| `pylon-aarch64` (`pylon-ci.yaml`) | Added by spec 07 A-T7; runs on `ubuntu-24.04-arm` | ✅ Its commands pass on the aarch64 dev host (2026-10-30): clippy `-D warnings` for `pylon-abi`, `pylon-guest-agent`, `pylond`, `scx-pylon`, `arch`, `hypervisor`, `vmm`, `cloud-hypervisor`; tests of the Pylon crates, `arch`, and `vmm --lib pylon`/`config`. Hosted arm64 runners have no `/dev/kvm`: the `vmm` KVM test skips itself (checked by hiding `/dev/kvm`), `hypervisor` tests are left out, and the benchmark isn't run. The job's first run on GitHub is the open check. |
| `formatting` aarch64-musl | Re-enabled | n/a (`rustfmt` only) |
| `build` x86_64-musl + `mshv`/`sev_snp`/`igvm` build steps | Re-enabled (Pylon host crates excluded from release `--all`) | ✅ x86_64-gnu feature builds covered by the clippy runs above |
| `build-riscv64` | Re-enabled | ⚠️ Not verified (no riscv64 sysroot); builds only `cloud-hypervisor` |
| `unit-tests-x86-64` musl | Re-enabled | ⚠️ Not verified (runs in the `dev_cli.sh` container) |
| `virtio-villain` | Re-enabled (GitHub-hosted runner with KVM) | ⚠️ Not verified |
| `integration-x86-64-pr` | **Still disabled** (`garm-noble-16` self-hosted runner) | — |
| `integration-x86-64-mq`, `-graviton`, `-vfio`, `-windows`, `-mshv-x86-64`, `-sev-snp` | Unchanged (merge-queue/push only; self-hosted runners) | — |
| `pylon-docs.yaml` | New: builds the book on PRs that touch `docs/pylon/`; publishes `gh-pages` from `main` | ✅ `mdbook build docs/pylon` (mdBook 0.5.4), all internal links and anchors resolve |
| `integration-metrics.yaml` | Gated on `github.repository == 'cloud-hypervisor/cloud-hypervisor'`; it used to queue forever on fork `main` pushes (`garm-jammy-16` runner absent) | — |

**Follow-ups:**
- Gate the remaining self-hosted jobs in `ci.yaml` on `github.repository == 'cloud-hypervisor/cloud-hypervisor'` instead of `if: false`, as `integration-metrics.yaml` now is, or register equivalent runners for the fork.
