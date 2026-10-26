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
