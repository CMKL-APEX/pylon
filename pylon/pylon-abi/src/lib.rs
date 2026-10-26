// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

#![no_std]

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};

pub const MAX_VCPUS: usize = 64;
pub const PYLON_MAGIC: u64 = 0x5059_4C4F_4E56_4D31; // "PYLONVM1"
pub const PYLON_ABI_VERSION: u32 = 1;

/// vCPU execution states in PylonVcpuSlot::state
pub const VCPU_STATE_PARKED: u8 = 0;
pub const VCPU_STATE_RUNNING: u8 = 1;
pub const VCPU_STATE_PARKING: u8 = 2;
pub const VCPU_STATE_WAKING: u8 = 3;

/// Fixed Guest Physical Address (GPA) where the Pylon shared page is mapped
/// when exposed via direct MMIO/RAM region.
pub const PYLON_DEFAULT_GPA: u64 = 0xFEDF_0000;

/// Total size in bytes of the Pylon shared memory region (2 * 4096B pages).
pub const PYLON_SHARED_PAGE_SIZE: usize = 8192;

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
    /// Guest TSC timestamp (rdtsc) of the oldest thread waiting in the runqueue.
    pub head_wait_tsc: AtomicU64,
    /// Next guest timer deadline TSC (used by pylond to wake a 0-core idle VM).
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
    pub _reserved_guest: [u8; 8],

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
    pub _reserved_host: [u8; 8],
}

/// Full 2-page (8192-byte) shared memory region mapped at /dev/shm/<vm-id>-pylon.
#[repr(C, align(4096))]
pub struct PylonSharedPage {
    pub header: PylonHeader,
    pub telemetry: PylonTelemetry,
    pub vcpus: [PylonVcpuSlot; MAX_VCPUS],
    pub _tail_pad: [u8; 3968], // Pad to exact 8192 bytes (2 * 4096B pages)
}
