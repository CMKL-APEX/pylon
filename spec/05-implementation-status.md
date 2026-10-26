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
| **Track 2** | `pylon/track-2-scx-pylon` | `pylon/scx-pylon`: `sched_ext` BPF C program (`scx_pylon.bpf.c`) & `libbpf-rs` `BPF_F_MMAPABLE` wrapper | Subagent 2 | `IN_PROGRESS` | `cargo test -p scx-pylon` |
| **Track 3** | `pylon/track-3-vmm-hooks` | `vmm/src/pylon.rs` & surgical hooks in `vmm/src/config.rs`, `vmm/src/memory_manager.rs`, `vmm/src/cpu.rs` | Subagent 3 | `IN_PROGRESS` | `cargo test -p vmm --features kvm` |
| **Integration** | `feat/pylon` | Merge `pylon/track-1-abi-pylond`, `pylon/track-2-scx-pylon`, `pylon/track-3-vmm-hooks` & full test suite | Coordinator | `PENDING` | `cargo test -p pylon-abi -p pylond -p scx-pylon && cargo test -p vmm --features kvm` |

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

- **Status:** `PENDING`
- **Checklist:**
  - [ ] Implement `pylon/scx-pylon/src/bpf/scx_pylon.bpf.c` defining `struct pcpu_owner_entry`, `pylon_pcpu_owners` (`BPF_MAP_TYPE_ARRAY | BPF_F_MMAPABLE`, 256 entries), `select_cpu`, and `dispatch`.
  - [ ] Implement `pylon/scx-pylon/build.rs` compiling `scx_pylon.bpf.c` via `clang -target bpf`.
  - [ ] Implement `pylon/scx-pylon/src/lib.rs` `libbpf-rs` wrapper and `BPF_F_MMAPABLE` userspace view (`PcpuOwnerMap`).
  - [ ] Run `cargo test -p scx-pylon`.
- **Notes / Logs:**
  - _Not started yet._

---

## Track 3 (`vmm/src/pylon.rs` & VMM Hooks)

- **Status:** `PENDING`
- **Checklist:**
  - [ ] Create `vmm/src/pylon.rs` encapsulating `PylonConfig`, `PylonVmmState`, `/dev/shm/<vm-id>-pylon` creation & GPA `0xFEDF_0000` mapping, `gettid()` registration, and fast-park wait primitives.
  - [ ] Wire `--pylon` config hook into `vmm/src/config.rs` (and `cloud-hypervisor/src/main.rs`).
  - [ ] Wire GPA `0xFEDF_0000` shared memory mapping hook into `vmm/src/memory_manager.rs`.
  - [ ] Wire vCPU `host_tid` registration and fast-park exit handler hook into `vmm/src/cpu.rs`.
  - [ ] Run `cargo test -p vmm --features kvm`.
- **Notes / Logs:**
  - _Not started yet._
