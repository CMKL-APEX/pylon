// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

#![no_std]

use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

pub const MAX_VCPUS: usize = 64;
pub const PYLON_MAGIC: u64 = 0x5059_4C4F_4E56_4D31; // "PYLONVM1"
pub const PYLON_ABI_VERSION: u32 = 1;

/// vCPU execution states in `PylonVcpuSlot::state`
pub const VCPU_STATE_PARKED: u8 = 0;
pub const VCPU_STATE_RUNNING: u8 = 1;
pub const VCPU_STATE_PARKING: u8 = 2;
pub const VCPU_STATE_WAKING: u8 = 3;

/// Sentinel value indicating no physical CPU is assigned to a vCPU slot.
pub const PCPU_UNASSIGNED: u8 = u8::MAX;

/// Fixed Guest Physical Address (GPA) where the Pylon shared page is mapped
/// when exposed via direct MMIO/RAM region.
pub const PYLON_DEFAULT_GPA: u64 = 0xFEDF_0000;

/// Total size in bytes of the Pylon shared memory region (2 * 4096B pages).
pub const PYLON_SHARED_PAGE_SIZE: usize = 8192;

/// Cache Line 0: Header metadata written once by VMM at VM creation.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PylonHeader {
    pub magic: u64,
    pub version: u32,
    pub num_vcpus: u32,
    pub vm_id: u64,
    pub tsc_khz: u64,
    pub _reserved: [u8; 32],
}

impl PylonHeader {
    /// Creates a new initialized `PylonHeader`.
    #[inline]
    pub const fn new(vm_id: u64, num_vcpus: u32, tsc_khz: u64) -> Self {
        Self {
            magic: PYLON_MAGIC,
            version: PYLON_ABI_VERSION,
            num_vcpus,
            vm_id,
            tsc_khz,
            _reserved: [0u8; 32],
        }
    }

    /// Returns `true` if the magic value, ABI version, and vCPU count are valid.
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.magic == PYLON_MAGIC
            && self.version == PYLON_ABI_VERSION
            && self.num_vcpus > 0
            && (self.num_vcpus as usize) <= MAX_VCPUS
    }
}

/// Cache Line 1: Guest -> Host Telemetry (Strictly written by Guest, Read-Only for Host).
#[repr(C, align(64))]
#[derive(Debug)]
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

impl PylonTelemetry {
    /// Creates a zero-initialized `PylonTelemetry` instance.
    #[inline]
    pub const fn new() -> Self {
        Self {
            runnable_threads: AtomicU32::new(0),
            rx_ring_cons_idx: AtomicU32::new(0),
            stranded_work_bitmap: AtomicU64::new(0),
            head_wait_tsc: AtomicU64::new(0),
            next_timer_tsc: AtomicU64::new(0),
            guest_heartbeat: AtomicU64::new(0),
            _pad: [0u8; 16],
        }
    }

    /// Resets all telemetry fields to zero.
    #[inline]
    pub fn reset(&self) {
        self.runnable_threads.store(0, Ordering::Release);
        self.rx_ring_cons_idx.store(0, Ordering::Release);
        self.stranded_work_bitmap.store(0, Ordering::Release);
        self.head_wait_tsc.store(0, Ordering::Release);
        self.next_timer_tsc.store(0, Ordering::Release);
        self.guest_heartbeat.store(0, Ordering::Release);
    }
}

impl Default for PylonTelemetry {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Cache Lines 2..65: One 64-byte slot per vCPU.
/// Lower 32 bytes are Guest-written; Upper 32 bytes are Host-written.
#[repr(C, align(64))]
#[derive(Debug)]
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

impl PylonVcpuSlot {
    /// Creates a new parked `PylonVcpuSlot`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(VCPU_STATE_PARKED),
            in_critical_section: AtomicU8::new(0),
            _pad_guest: [0u8; 6],
            last_yield_tsc: AtomicU64::new(0),
            yield_count: AtomicU64::new(0),
            _reserved_guest: [0u8; 8],
            revoke_requested: AtomicU8::new(0),
            assigned_pcpu: AtomicU8::new(PCPU_UNASSIGNED),
            _pad_host: [0u8; 2],
            host_tid: AtomicU32::new(0),
            revoke_deadline_tsc: AtomicU64::new(0),
            forced_preempt_count: AtomicU64::new(0),
            _reserved_host: [0u8; 8],
        }
    }

    /// Resets the vCPU slot to default parked state.
    #[inline]
    pub fn reset(&self) {
        self.state.store(VCPU_STATE_PARKED, Ordering::Release);
        self.in_critical_section.store(0, Ordering::Release);
        self.last_yield_tsc.store(0, Ordering::Release);
        self.yield_count.store(0, Ordering::Release);
        self.revoke_requested.store(0, Ordering::Release);
        self.assigned_pcpu.store(PCPU_UNASSIGNED, Ordering::Release);
        self.host_tid.store(0, Ordering::Release);
        self.revoke_deadline_tsc.store(0, Ordering::Release);
        self.forced_preempt_count.store(0, Ordering::Release);
    }
}

impl Default for PylonVcpuSlot {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Full 2-page (8192-byte) shared memory region mapped at /dev/shm/<vm-id>-pylon.
#[repr(C, align(4096))]
#[derive(Debug)]
pub struct PylonSharedPage {
    pub header: PylonHeader,
    pub telemetry: PylonTelemetry,
    pub vcpus: [PylonVcpuSlot; MAX_VCPUS],
    pub _tail_pad: [u8; 3968], // Pad to exact 8192 bytes (2 * 4096B pages)
}

impl PylonSharedPage {
    /// Initializes a `PylonSharedPage` in place with the given VM metadata.
    #[inline]
    pub fn init(&mut self, vm_id: u64, num_vcpus: u32, tsc_khz: u64) {
        self.header = PylonHeader::new(vm_id, num_vcpus, tsc_khz);
        self.telemetry.reset();
        for slot in &self.vcpus {
            slot.reset();
        }
        self._tail_pad.fill(0);
    }

    /// Counts how many vCPUs are currently in `VCPU_STATE_RUNNING` or `VCPU_STATE_WAKING`.
    #[inline]
    pub fn active_vcpu_count(&self) -> u32 {
        let limit = (self.header.num_vcpus as usize).min(MAX_VCPUS);
        let mut count = 0u32;
        let mut idx = 0;
        while idx < limit {
            let st = self.vcpus[idx].state.load(Ordering::Acquire);
            if st == VCPU_STATE_RUNNING || st == VCPU_STATE_WAKING {
                count += 1;
            }
            idx += 1;
        }
        count
    }
}

// Compile-time layout assertions required by Specification 01.
const _: () = {
    assert!(size_of::<PylonHeader>() == 64);
    assert!(align_of::<PylonHeader>() == 64);
    assert!(size_of::<PylonTelemetry>() == 64);
    assert!(align_of::<PylonTelemetry>() == 64);
    assert!(size_of::<PylonVcpuSlot>() == 64);
    assert!(align_of::<PylonVcpuSlot>() == 64);
    assert!(size_of::<PylonSharedPage>() == 8192);
    assert!(align_of::<PylonSharedPage>() == 4096);
    assert!(offset_of!(PylonVcpuSlot, revoke_requested) == 32);
    assert!(offset_of!(PylonSharedPage, header) == 0);
    assert!(offset_of!(PylonSharedPage, telemetry) == 64);
    assert!(offset_of!(PylonSharedPage, vcpus) == 128);
};

#[cfg(test)]
mod tests {
    use core::mem::zeroed;

    use super::*;

    #[test]
    fn test_abi_struct_sizes_and_offsets() {
        assert_eq!(size_of::<PylonHeader>(), 64);
        assert_eq!(align_of::<PylonHeader>(), 64);
        assert_eq!(size_of::<PylonTelemetry>(), 64);
        assert_eq!(align_of::<PylonTelemetry>(), 64);
        assert_eq!(size_of::<PylonVcpuSlot>(), 64);
        assert_eq!(align_of::<PylonVcpuSlot>(), 64);
        assert_eq!(size_of::<PylonSharedPage>(), PYLON_SHARED_PAGE_SIZE);
        assert_eq!(size_of::<PylonSharedPage>(), 8192);
        assert_eq!(align_of::<PylonSharedPage>(), 4096);
        assert_eq!(offset_of!(PylonVcpuSlot, revoke_requested), 32);
        assert_eq!(offset_of!(PylonSharedPage, header), 0);
        assert_eq!(offset_of!(PylonSharedPage, telemetry), 64);
        assert_eq!(offset_of!(PylonSharedPage, vcpus), 128);
    }

    #[test]
    fn test_shared_page_initialization() {
        // SAFETY: All fields in PylonSharedPage are valid when zero-initialized.
        let mut page: PylonSharedPage = unsafe { zeroed() };
        page.init(42, 8, 3_000_000);
        assert!(page.header.is_valid());
        assert_eq!(page.header.magic, PYLON_MAGIC);
        assert_eq!(page.header.version, PYLON_ABI_VERSION);
        assert_eq!(page.header.vm_id, 42);
        assert_eq!(page.header.num_vcpus, 8);
        assert_eq!(page.header.tsc_khz, 3_000_000);
        assert_eq!(page.active_vcpu_count(), 0);
        assert_eq!(
            page.vcpus[0].assigned_pcpu.load(Ordering::Acquire),
            PCPU_UNASSIGNED
        );
    }
}
