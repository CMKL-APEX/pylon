# Specification 04: Pylon Subsystem Implementation Plan (`CMKL-APEX/pylon`)

## 1. Architectural Overview & Upstream Isolation Strategy

This document defines the implementation plan to scaffold and build the **Pylon** microsecond-scale core arbitration subsystem inside the Cloud Hypervisor fork (`CMKL-APEX/pylon`) while keeping upstream rebase diffs minimal.

### Structural Boundaries & Git Branching Strategy
1. Append `"pylon/pylon-abi"`, `"pylon/pylond"`, and `"pylon/scx-pylon"` to the bottom of the root workspace `Cargo.toml`.
2. Isolate VMM-side shared-memory setup, `gettid()` registration, and fast-park wait loops inside `vmm/src/pylon.rs`, touching upstream `vmm/src/config.rs`, `vmm/src/memory_manager.rs`, and `vmm/src/cpu.rs` (plus `vmm/src/lib.rs` and `vmm/Cargo.toml`) only for minimal hook calls.
3. **Branch Isolation (`main` kept clean):**
   - `main`: Remains untouched at upstream/origin `HEAD` (`origin/main`).
   - `pylon/scaffold`: Commits `spec/*`, root `Cargo.toml` workspace members, and minimal crate manifests.
   - `pylon/track-1-abi-pylond`: Subagent 1 branch (`pylon/pylon-abi` & `pylon/pylond`).
   - `pylon/track-2-scx-pylon`: Subagent 2 branch (`pylon/scx-pylon`).
   - `pylon/track-3-vmm-hooks`: Subagent 3 branch (`vmm/src/pylon.rs` & upstream VMM hooks).
   - `feat/pylon`: Integration branch merging all three track branches for end-to-end workspace tests and PR review.

### Upstream Touch Points Summary

| File | Nature of Change |
| :--- | :--- |
| `Cargo.toml` | Append `"pylon/pylon-abi"`, `"pylon/pylond"`, `"pylon/scx-pylon"` to `default-members` and `members` at the bottom of the workspace list. |
| `vmm/Cargo.toml` | Add `pylon-abi = { path = "../pylon/pylon-abi" }` dependency. |
| `vmm/src/lib.rs` | Declare `pub mod pylon;`. |
| `vmm/src/config.rs` | Add `ParsePylon` error variant, `pub pylon: Option<&'a str>` to `VmParams<'a>`, extract `--pylon` in `VmParams::from_arg_matches`, and invoke `crate::pylon::parse_and_configure(vm_params.pylon, config.cpus.boot_vcpus, config.cpus.max_vcpus)?` in `VmConfig::parse`. |
| `vmm/src/memory_manager.rs` | Add `Pylon(#[source] crate::pylon::Error)` error variant and invoke `crate::pylon::setup_pylon_gpa_mapping(&vm, &mut guest_memory, &mut next_memory_slot).map_err(Error::Pylon)?` in `MemoryManager::new`. |
| `vmm/src/cpu.rs` | Invoke `crate::pylon::on_vcpu_thread_start(vcpu_id)` in `CpuManager::start_vcpu`, `crate::pylon::check_and_park_vcpu(vcpu_id)` inside the `vcpu.run()` exit loop, and `crate::pylon::wake_all_for_shutdown()` in `CpuManager::shutdown`. |
| `vmm/src/pylon.rs` | **(New file)** Self-contained VMM-side Pylon state, `--pylon` option parser, `/dev/shm/<vm-id>-pylon` 8,192B shared memory creation & GPA `0xFEDF_0000` mapping, `pylond` Unix socket registration (`SCM_RIGHTS`), `gettid()` export, and per-vCPU `EventFd`/futex fast-park wait loop. |

---

## 2. Track 1: `pylon/pylon-abi` & `pylon/pylond` (Subagent 1)

### 2.1 `pylon/pylon-abi` (`#![no_std]` Shared Memory Contract)
- **Files:**
  - `pylon/pylon-abi/Cargo.toml`
  - `pylon/pylon-abi/src/lib.rs`
- **Specification Compliance (`spec/01-shared-memory-abi.md`):**
  - `#![no_std]` crate with zero external runtime dependencies.
  - Constants:
    - `MAX_VCPUS: usize = 64`
    - `PYLON_MAGIC: u64 = 0x5059_4C4F_4E56_4D31` (`"PYLONVM1"`)
    - `PYLON_ABI_VERSION: u32 = 1`
    - `VCPU_STATE_PARKED: u8 = 0`, `VCPU_STATE_RUNNING: u8 = 1`, `VCPU_STATE_PARKING: u8 = 2`, `VCPU_STATE_WAKING: u8 = 3`
    - `PYLON_DEFAULT_GPA: u64 = 0xFEDF_0000`
    - `PYLON_SHARED_PAGE_SIZE: usize = 8192`
  - Cache-line-aligned structs (`#[repr(C, align(64))]` and `#[repr(C, align(4096))]`):
    - `PylonHeader` (64 bytes)
    - `PylonTelemetry` (64 bytes)
    - `PylonVcpuSlot` (64 bytes: lower 32B guest-written, upper 32B host-written starting at `revoke_requested` at offset 32)
    - `PylonSharedPage` (8,192 bytes: `header`, `telemetry`, `vcpus: [PylonVcpuSlot; 64]`, `_tail_pad: [u8; 3968]`)
  - Compile-time `const` assertions and `#[test]` unit tests verifying:
    - `size_of::<PylonHeader>() == 64`
    - `size_of::<PylonTelemetry>() == 64`
    - `size_of::<PylonVcpuSlot>() == 64`
    - `size_of::<PylonSharedPage>() == 8192`
    - `offset_of!(PylonVcpuSlot, revoke_requested) == 32`

### 2.2 `pylon/pylond` (Two-Plane Node Core Arbiter Daemon)
- **Files:**
  - `pylon/pylond/Cargo.toml`
  - `pylon/pylond/src/lib.rs`
  - `pylon/pylond/src/main.rs`
  - `pylon/pylond/src/control.rs`
  - `pylon/pylond/src/engine.rs`
- **Plane 1 (Control Plane - Core 0):**
  - Pins control thread to Core 0 via `libc::sched_setaffinity`.
  - Runs an asynchronous Tokio Unix domain socket listener (`/run/pylon/pylond.sock` or configured socket path).
  - Accepts VM registration metadata (`vm_id`, `num_vcpus`, `min_cores`, `max_cores`, `priority_weight`) and file descriptors via `SCM_RIGHTS` (`shm_fd`, per-vCPU `kick_irqfds`, per-vCPU `wake_eventfds`).
  - Memory-maps `PylonSharedPage` via `memmap2`, constructs `VmHandle`, and publishes a new immutable `Arc<Vec<VmHandle>>` snapshot to Plane 2 via `arc_swap::ArcSwap`.
- **Plane 2 (Real-Time Matrix Engine - Core 1):**
  - Pins engine thread to Core 1 via `libc::sched_setaffinity`.
  - Zero heap allocations, zero mutexes, and zero blocking syscalls in steady state.
  - Uses x86_64 `_rdtsc()` for timing and `_tpause(0, tsc + 1000)` (with runtime `WAITPKG` CPUID detection and `_mm_pause()` fallback) between sweeps.
  - Implements the 3-phase arbitration loop:
    1. **Phase 1:** Demand & 0-core wakeup evaluation (`runnable_threads`, `head_wait_tsc`, `next_timer_tsc`, `rx_ring_cons_idx`, and `stranded_work_bitmap` `tzcnt` preference).
    2. **Phase 2:** Progress in-flight revocations (`VCPU_STATE_PARKED` or `now_tsc >= deadline_tsc`), updating `forced_preempt_count`, writing `recipient_slot.host_tid` into the `pcpu_owner_entry` map, setting `assigned_pcpu` and `VCPU_STATE_WAKING`, and signaling `wake_eventfd`.
    3. **Phase 3:** Match hungry VMs with `CoreState::Idle` cores (fast path) or over-allocated/lower-urgency donor VMs (preemption path, preferring `in_critical_section == 0`, setting 8us `revoke_deadline_tsc` and `revoke_requested = 1`, and firing `kick_revoke_irqfd`).

---

## 3. Track 2: `pylon/scx-pylon` (Subagent 2)

- **Files:**
  - `pylon/scx-pylon/Cargo.toml`
  - `pylon/scx-pylon/build.rs`
  - `pylon/scx-pylon/src/bpf/scx_pylon.bpf.c`
  - `pylon/scx-pylon/src/lib.rs`
- **Specification Compliance (`spec/03-scx-and-vmm-hooks.md`):**
  - **BPF Program (`src/bpf/scx_pylon.bpf.c`):**
    - Defines `struct pcpu_owner_entry { __u32 target_tid; __u32 flags; };`.
    - Declares `pylon_pcpu_owners` with `BPF_MAP_TYPE_ARRAY`, `max_entries = 256`, `map_flags = BPF_F_MMAPABLE`.
    - Implements `scx_ops.select_cpu(p, prev_cpu, wake_flags)` and `scx_ops.dispatch(cpu, prev)` dispatching matching `target_tid` tasks directly to `SCX_DSQ_LOCAL` and preempting expired donor vCPUs.
  - **Build Script (`build.rs`):**
    - Compiles `src/bpf/scx_pylon.bpf.c` with `clang -target bpf -O2 -g` into `$OUT_DIR/scx_pylon.bpf.o`.
  - **Rust Userspace Wrapper (`src/lib.rs`):**
    - Wraps `scx_pylon.bpf.o` via `libbpf-rs`, verifies the `pylon_pcpu_owners` map (`BPF_MAP_TYPE_ARRAY | BPF_F_MMAPABLE`, 256 entries, 8B value size), and exposes a zero-syscall memory-mapped `PcpuOwnerMap` interface with unprivileged test backing for `cargo test`.

---

## 4. Track 3: `vmm/src/pylon.rs` & VMM Hooks (Subagent 3)

- **Files:**
  - `vmm/src/pylon.rs`
  - `vmm/src/config.rs`
  - `vmm/src/memory_manager.rs`
  - `vmm/src/cpu.rs`
  - `vmm/src/lib.rs`
  - `vmm/Cargo.toml`
  - `cloud-hypervisor/src/main.rs` (optional `--pylon` CLI flag registration)
- **Specification Compliance (`spec/03-scx-and-vmm-hooks.md`):**
  - `--pylon shm=/dev/shm/<vm-id>-pylon[,socket=/run/pylon/pylond.sock,gpa=0xfedf0000]` parsed via `OptionParser`.
  - Shared memory file `/dev/shm/<vm-id>-pylon` (8,192 bytes) initialized with `PylonHeader` (`PYLON_MAGIC`, `PYLON_ABI_VERSION`, `num_vcpus`) and mapped into Guest Physical Address space at `PYLON_DEFAULT_GPA` (`0xFEDF_0000`) using `GuestRegionMmap` with `MAP_SHARED`.
  - vCPU thread initialization obtains `unsafe { libc::gettid() as u32 }`, stores it into `PylonSharedPage.vcpus[vcpu_id].host_tid`, and parks vCPUs where `vcpu_id >= initial_active_vcpus`.
  - Fast-park exit handler checks `slot.revoke_requested.load(Acquire) == 1` on exit (`Hlt` / hypercall / loop iteration), clears `revoke_requested`, sets `state = VCPU_STATE_PARKED`, blocks on the per-vCPU wait primitive until woken by `pylond`, and sets `state = VCPU_STATE_RUNNING` before re-entering `KVM_RUN`.
