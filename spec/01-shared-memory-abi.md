# Specification 01: Shared Memory Matrix ABI (`pylon-abi`)

## 1. Purpose & Constraints
The `pylon-abi` crate (`pylon/pylon-abi`) defines the `#![no_std]` binary contract shared across:
1. **`pylond`** (Host Node Core Arbiter)
2. **`cloud-hypervisor` (`vmm/src/pylon.rs`)** (Per-VM VMM process)
3. **Guest Kernel** (Linux `pv-pylon` module or Rust `Asterinas/ostd` scheduler)

### Hardware Coherency Rules
- Every structure written by different actors **must** be aligned to a 64-byte x86_64/ARM64 cache line (`#[repr(C, align(64))]`) to prevent false sharing and L1/L2 cache-line bouncing across cores.
- The total `PylonSharedPage` size is padded to an exact **8,192-byte (two-page)** boundary for clean page-table mapping.

---

## 2. Exact Rust Layout (`pylon/pylon-abi/src/lib.rs`)

```rust
#![no_std]

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};

pub const MAX_VCPUS: usize = 64;
pub const PYLON_MAGIC: u64 = 0x5059_4C4F_4E56_4D31; // "PYLONVM1"
pub const PYLON_ABI_VERSION: u32 = 2; // 1 until spec 06 S3: same layout, stricter writer rules

/// vCPU execution states in PylonVcpuSlot::state
pub const VCPU_STATE_PARKED: u8 = 0;
pub const VCPU_STATE_RUNNING: u8 = 1;
pub const VCPU_STATE_PARKING: u8 = 2;
pub const VCPU_STATE_WAKING: u8 = 3;

/// Fixed Guest Physical Address (GPA) where the Pylon shared page is mapped
/// when exposed via direct MMIO/RAM region.
pub const PYLON_DEFAULT_GPA: u64 = 0xFEDF_0000;

/// Cache Line 0: Header metadata written once by VMM at VM creation.
#[repr(C, align(64))]
pub struct PylonHeader {
    pub magic: u64,
    pub version: u32,
    pub num_vcpus: u32,
    pub vm_id: u64,
    pub tsc_khz: u64,
    pub _reserved: [u8; 32],
}

/// Cache Line 1: Guest -> Host Telemetry (Strictly written by Guest, Read-Only for Host).
#[repr(C, align(64))]
pub struct PylonTelemetry {
    /// Total number of runnable threads (running + queued) inside the guest OS.
    pub runnable_threads: AtomicU32,
    /// Guest virtio/vDPA RX queue consumer index (for 0-core wakeups).
    pub rx_ring_cons_idx: AtomicU32,
    /// Bitmask of parked vCPU indices (0..63) that have stranded local runqueue tasks or RCU work.
    pub stranded_work_bitmap: AtomicU64,
    /// Guest TSC since which runnable threads have waited for a vCPU; 0 if none wait.
    pub head_wait_tsc: AtomicU64,
    /// Next guest timer deadline in guest TSC (used by pylond to wake a 0-core idle VM).
    pub next_timer_tsc: AtomicU64,
    /// Monotonically increasing guest heartbeat counter.
    pub guest_heartbeat: AtomicU64,
    pub _pad: [u8; 16],
}

/// Cache Lines 2..65: One 64-byte slot per vCPU.
/// Lower 32 bytes are Guest-written; Upper 32 bytes are Host-written.
#[repr(C, align(64))]
pub struct PylonVcpuSlot {
    // --- Lower 32B: Written by Guest / VMM vCPU thread ---
    /// Current vCPU state (VCPU_STATE_*).
    pub state: AtomicU8,
    /// Non-zero when the guest vCPU holds a spinlock, is in an IRQ handler, or has preemption disabled.
    pub in_critical_section: AtomicU8,
    pub _pad_guest: [u8; 6],
    /// TSC when the guest last yielded this vCPU.
    pub last_yield_tsc: AtomicU64,
    /// Number of cooperative yields performed by this vCPU.
    pub yield_count: AtomicU64,
    /// Host TSC at which the guest's TSC-deadline timer was due when the VMM parked this
    /// vCPU; 0 if none. Written by the VMM; cleared when the vCPU runs again.
    pub park_timer_deadline_tsc: AtomicU64,

    // --- Upper 32B: Written by Host (pylond & VMM init) ---
    /// Set to 1 by pylond when requesting this vCPU to yield its physical core.
    pub revoke_requested: AtomicU8,
    /// Physical CPU core ID (pCPU) currently assigned to this vCPU by pylond.
    pub assigned_pcpu: AtomicU8,
    pub _pad_host: [u8; 2],
    /// Host Linux Thread ID (pid_t from gettid()) for this vCPU thread. Written by VMM at boot.
    pub host_tid: AtomicU32,
    /// Hard TSC deadline after which pylond will forcibly preempt the vCPU if it hasn't parked.
    pub revoke_deadline_tsc: AtomicU64,
    /// Number of forced preemptions (deadline violations) on this vCPU.
    pub forced_preempt_count: AtomicU64,
    /// Guest TSC minus host TSC (wrapping); 0 if not measured. Written by the VMM before the
    /// vCPU first runs. `pylond` converts guest-written TSC values with vCPU 0's offset.
    pub guest_tsc_offset: AtomicU64,
}

/// Full 2-page (8192-byte) shared memory region mapped at /dev/shm/<vm-id>-pylon.
#[repr(C, align(4096))]
pub struct PylonSharedPage {
    pub header: PylonHeader,
    pub telemetry: PylonTelemetry,
    pub vcpus: [PylonVcpuSlot; MAX_VCPUS],
    pub _tail_pad: [u8; 3968], // Pad to exact 8192 bytes (2 * 4096B pages)
}
```

---

## 3. Required Compile-Time Verification Tests
`pylon-abi` must include unit tests verifying exact struct sizes and offsets:
- `size_of::<PylonHeader>() == 64`
- `size_of::<PylonTelemetry>() == 64`
- `size_of::<PylonVcpuSlot>() == 64`
- `size_of::<PylonSharedPage>() == 8192`
- `offset_of!(PylonVcpuSlot, revoke_requested) == 32`
- `offset_of!(PylonVcpuSlot, park_timer_deadline_tsc) == 24`
- `offset_of!(PylonVcpuSlot, guest_tsc_offset) == 56`

### Time bases
Every `*_tsc` field and `tsc_khz` count **host counter ticks** (`spec/07-aarch64-support.md` §3):
the TSC on x86_64 and the virtual counter `CNTVCT_EL0` on aarch64, both read with
`pylon_abi::clock::read_counter`. The field names and the layout don't change.

The guest writes `head_wait_tsc` and `next_timer_tsc` with its own counter. `pylond` converts them
to the host counter with `PylonSharedPage::guest_to_host_tsc` (subtracting vCPU 0's
`guest_tsc_offset`). Every other `*_tsc` field is in the host counter. The conversion assumes the
guest counter runs at the host rate (no TSC scaling on x86_64); an offset of 0 leaves values
unconverted.
