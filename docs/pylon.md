# Pylon: Microsecond-Scale Core Elasticity Subsystem

Pylon is a microsecond-scale physical CPU (pCPU) arbitration and core elasticity subsystem integrated into this Cloud Hypervisor fork (`CMKL-APEX/pylon`). It replaces traditional 10–100 millisecond ACPI hotplug and host CFS time-slicing with a sub-10-microsecond cooperative core handoff loop driven by shared memory, `UMONITOR`/`UMWAIT`/`TPAUSE`, and Linux `sched_ext` (`scx_pylon`).

For full design specifications, see [`spec/00-architecture.md`](../spec/00-architecture.md) through [`spec/04-implementation-plan.md`](../spec/04-implementation-plan.md).

---

## Architecture Overview

Pylon uses a three-tier split-brain architecture that eliminates VM-Exit and kernel scheduler bottlenecks on the critical path:

1. **Cloud Hypervisor VMM (`vmm/src/pylon.rs`)**:
   - Allocates and memory-maps an 8,192-byte shared control page (`/dev/shm/<vm-id>-pylon`) into both host userspace and Guest Physical Address (GPA) `0xFEDF_0000` (`MAP_SHARED`, `MemoryVisibility::Shared`).
   - Exports each vCPU thread's Linux kernel TID (`libc::gettid()`) into `PylonSharedPage.vcpus[vcpu_id].host_tid`.
   - Registers the VM's shared memory FD, per-vCPU kick `irqfd`s, and per-vCPU wake `eventfd`s with `pylond` over `/run/pylon/pylond.sock` via `SCM_RIGHTS`.
   - Intercepts `KVM_EXIT_HLT` / `VcpuExit::Hlt` when `vcpus[vcpu_id].revoke_requested == 1`, transitions the vCPU to `VCPU_STATE_PARKED`, and parks the host vCPU thread on its dedicated wake `EventFd`.
2. **Host Node Core Arbiter (`pylon/pylond`)**:
   - **Plane 1 (Core 0 — Async Control Plane):** Tokio Unix domain socket listener (`/run/pylon/pylond.sock`) that accepts VM registrations via `SCM_RIGHTS`, memory-maps each VM's `PylonSharedPage`, and publishes immutable `Arc<Vec<VmHandle>>` snapshots to Plane 2 using lock-free `ArcSwap`.
   - **Plane 2 (Core 1 — Polling & Matrix Engine):** Dedicated lock-free, allocation-free polling loop using `_rdtsc()` and `_tpause()` (with `_mm_pause()` fallback) to evaluate VM demand, 0-core wakeups, cooperative 8 us revocations, and physical CPU ownership transitions.
3. **Kernel `sched_ext` Dispatcher (`pylon/scx-pylon`)**:
   - eBPF `sched_ext_ops` scheduler (`scx_pylon.bpf.c`) sharing a `BPF_MAP_TYPE_ARRAY | BPF_F_MMAPABLE` map (`pylon_pcpu_owners`, 256 entries) with `pylond`.
   - Reads `pylon_pcpu_owners[cpu].target_tid` in `pylon_select_cpu` and `pylon_dispatch` to insert the owning vCPU thread directly into `SCX_DSQ_LOCAL` and preempt evicted donor vCPUs via `scx_bpf_kick_cpu(cpu, SCX_KICK_PREEMPT)`.

---

## Shared Memory ABI (`pylon/pylon-abi`)

The `pylon-abi` crate is `#![no_std]` and defines the 8,192-byte shared memory layout (`PylonSharedPage`) mapped at GPA `0xFEDF_0000`. All structures are 64-byte cache-line aligned (`#[repr(C, align(64))]`) to prevent false sharing between the guest kernel and host arbiter:

| Offset | Size | Structure | Writer | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x0000` | 64 B | `PylonHeader` | VMM (init) | Magic `0x50594C4F` (`"PYLO"`), ABI version (`1`), `num_vcpus`, `vm_id`, `tsc_khz` |
| `0x0040` | 64 B | `PylonTelemetry` | Guest (`0x00..0x2F`) / Host (`0x30..0x3F`) | `desired_cores`, `urgency_score`, `next_timer_tsc`, `rx_ring_cons_idx`, `stranded_work_bitmap`, `allocated_cores`, `host_rx_doorbell`, `forced_preempt_count` |
| `0x0080` | 3,968 B | `_pad0` | — | Reserved padding to align the vCPU slot array to the second 4 KiB page |
| `0x1000` | 4,096 B | `[PylonVcpuSlot; 64]` | Guest (`0x00..0x1F`) / Host (`0x20..0x3F`) | Per-vCPU state (`PARKED=0`, `RUNNING=1`, `WAKING=2`, `YIELDING=3`), `in_critical_section`, `yield_count`, `revoke_requested` (offset 32), `assigned_pcpu`, `host_tid`, `revoke_deadline_tsc` |

---

## Cloud Hypervisor `--pylon` Usage

Enable Pylon on a Cloud Hypervisor VM by passing `--pylon` on the command line:

```text
--pylon shm=<path>[,socket=<path>,gpa=<hex|dec>,initial_active_vcpus=<n>,min_cores=<n>,max_cores=<n>,priority_weight=<n>,vm_id=<id>]
```

### Configuration Parameters

| Parameter | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `shm` | Path | *(Required)* | Path to the 8,192-byte shared memory file (typically `/dev/shm/<vm-id>-pylon`) |
| `socket` | Path | *(Optional)* | Path to the `pylond` Unix domain socket (e.g., `/run/pylon/pylond.sock`) |
| `gpa` | `u64` (hex/dec) | `0xFEDF0000` | 4 KiB-aligned Guest Physical Address where the shared page is mapped |
| `initial_active_vcpus` | `u32` | `boot` vCPUs | Number of vCPUs initially unparked at boot (`vcpu_id >= initial_active_vcpus` start parked) |
| `min_cores` | `u32` | `0` | Minimum guaranteed physical cores (`0` enables 0-core scale-to-zero hibernation) |
| `max_cores` | `u32` | `max` vCPUs | Maximum physical cores the VM may expand to |
| `priority_weight` | `u32` | `100` | Relative arbitration weight used by `pylond` |
| `vm_id` | `u64` | Derived from `shm` | Explicit numeric VM identifier written to `PylonHeader.vm_id` |

### End-to-End Example

1. **Start the `pylond` arbiter daemon** (managing tenant physical cores 2 through 7):

```bash
sudo mkdir -p /run/pylon
sudo ./target/release/pylond \
    --socket /run/pylon/pylond.sock \
    --cores 2-7
```

2. **Launch Cloud Hypervisor with Pylon enabled** (8 max vCPUs, 2 initially active, scale-to-zero enabled):

```bash
./target/release/cloud-hypervisor \
    --kernel vmlinux \
    --disk path=ubuntu.raw \
    --cpus boot=8,max=8 \
    --memory size=4096M,shared=on \
    --pylon shm=/dev/shm/vm1-pylon,socket=/run/pylon/pylond.sock,gpa=0xfedf0000,initial_active_vcpus=2,min_cores=0,max_cores=8,priority_weight=100,vm_id=1
```

---

## Building and Testing

### Prerequisites

In addition to the standard Cloud Hypervisor build dependencies, compiling `pylon/scx-pylon` requires `clang` with BPF target support, `libelf-dev`, and `zlib1g-dev`:

```bash
sudo apt-get update
sudo apt-get install -y clang llvm libelf-dev zlib1g-dev pkg-config
```

### Running Unit & Integration Tests

```bash
# 1. Test the #![no_std] shared memory ABI and compile-time layout assertions
cargo test -p pylon-abi

# 2. Test the two-plane pylond arbiter daemon (Control Plane + Matrix Engine)
cargo test -p pylond

# 3. Test the scx-pylon BPF object build and userspace BPF_F_MMAPABLE map wrapper
cargo test -p scx-pylon

# 4. Test VMM Pylon config parsing, GPA shared memory mapping, and fast-park lifecycle
cargo test -p vmm --features kvm --lib pylon
cargo test -p vmm --features kvm --lib config
cargo test -p cloud-hypervisor --features kvm --bin cloud-hypervisor

# 5. Run Clippy across all Pylon and VMM crates
cargo clippy -p pylon-abi -p pylond -p scx-pylon -p vmm -p cloud-hypervisor --features kvm --tests -- -D warnings
```
