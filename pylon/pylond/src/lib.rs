// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

pub mod control;
pub mod engine;

use core::array::from_fn;
use std::ffi::CString;
use std::fs::{self, File};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{io, ptr};

use arc_swap::ArcSwap;
use log::warn;
use memmap2::{MmapMut, MmapOptions};
use pylon_abi::{
    MAX_VCPUS, PYLON_HOST_PAGE_OFFSET, PYLON_SHM_FILE_SIZE, PylonHostPage, PylonSharedPage,
    VCPU_STATE_PARKED,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default Unix domain socket path for `pylond` control plane.
pub const DEFAULT_SOCKET_PATH: &str = "/run/pylon/pylond.sock";

/// Maximum number of physical CPUs tracked in the `scx_pylon` BPF map.
pub const MAX_PCPUS: usize = 256;

/// Errors returned by `pylond` operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error")]
    Io(#[source] io::Error),

    #[error("JSON serialization error")]
    Json(#[source] serde_json::Error),

    #[error("Shared memory region too small: got {actual} bytes, expected at least {expected}")]
    SharedMemoryTooSmall { actual: usize, expected: usize },

    #[error("Invalid Pylon shared page header for VM {vm_id}")]
    InvalidSharedHeader { vm_id: u64 },

    #[error("Invalid Pylon host page header for VM {vm_id}")]
    InvalidHostHeader { vm_id: u64 },

    #[error("Invalid registration request for VM {vm_id}: {reason}")]
    InvalidRegistration { vm_id: u64, reason: String },

    #[error("Invalid policy update for VM {vm_id}: {reason}")]
    InvalidPolicyUpdate { vm_id: u64, reason: String },

    #[error("Invalid file descriptor count: got {actual}, expected {expected}")]
    InvalidFdCount { actual: usize, expected: usize },

    #[error("Failed to open a pidfd for VMM process {pid}")]
    PidfdOpen {
        pid: i32,
        #[source]
        source: io::Error,
    },

    #[error("Failed to signal vCPU {vcpu} thread {tid} of VMM process {pid}")]
    SignalVcpu {
        vcpu: usize,
        tid: u32,
        pid: i32,
        #[source]
        source: io::Error,
    },

    #[error("vCPU index {vcpu} out of bounds (num_vcpus = {num_vcpus})")]
    VcpuOutOfBounds { vcpu: usize, num_vcpus: u32 },

    #[error("Physical CPU index {pcpu} out of bounds (max = {max})")]
    PcpuOutOfBounds { pcpu: usize, max: usize },

    #[error("Failed to set CPU affinity to core {core_id}")]
    CpuAffinity {
        core_id: usize,
        #[source]
        source: io::Error,
    },
}

/// Entry in the `pylon_pcpu_owners` `BPF_F_MMAPABLE` array map (`spec/03-scx-and-vmm-hooks.md`).
#[repr(C)]
#[derive(Debug)]
pub struct PcpuOwnerEntry {
    pub target_tid: AtomicU32,
    pub flags: AtomicU32,
}

impl PcpuOwnerEntry {
    /// Creates a zero-initialized `PcpuOwnerEntry`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            target_tid: AtomicU32::new(0),
            flags: AtomicU32::new(0),
        }
    }
}

impl Default for PcpuOwnerEntry {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

const _: () = {
    assert!(size_of::<PcpuOwnerEntry>() == 8);
};

/// Backing storage for the 256-entry `pylon_pcpu_owners` map.
#[derive(Debug)]
enum PcpuOwnerStorage {
    Mmap(MmapMut),
    Heap(Box<[PcpuOwnerEntry; MAX_PCPUS]>),
}

impl PcpuOwnerStorage {
    #[inline]
    fn entry(&self, pcpu_id: usize) -> &PcpuOwnerEntry {
        assert!(pcpu_id < MAX_PCPUS, "pcpu_id out of bounds");
        match self {
            Self::Mmap(mmap) => {
                #[expect(
                    clippy::cast_ptr_alignment,
                    reason = "mmap(2) returns page-aligned memory"
                )]
                // SAFETY: `mmap` was verified to be at least `MAX_PCPUS * 8` bytes and page-aligned.
                unsafe {
                    &*mmap.as_ptr().cast::<PcpuOwnerEntry>().add(pcpu_id)
                }
            }
            Self::Heap(entries) => &entries[pcpu_id],
        }
    }
}

/// Memory-mapped or heap-backed table of `PcpuOwnerEntry` records indexed by physical CPU ID.
#[derive(Debug)]
pub struct PcpuOwnerTable {
    storage: PcpuOwnerStorage,
    kick: Option<scx_pylon::OwnerKick>,
}

impl PcpuOwnerTable {
    /// Creates an in-memory `PcpuOwnerTable` suitable for unprivileged execution and testing.
    pub fn new_in_memory() -> Self {
        let entries: Box<[PcpuOwnerEntry; MAX_PCPUS]> =
            Box::new(from_fn(|_| PcpuOwnerEntry::new()));
        Self {
            storage: PcpuOwnerStorage::Heap(entries),
            kick: None,
        }
    }

    /// Creates a `PcpuOwnerTable` backed by a memory-mapped file or BPF map fd.
    pub fn from_mmap(mmap: MmapMut) -> Result<Self, Error> {
        let expected = MAX_PCPUS * size_of::<PcpuOwnerEntry>();
        if mmap.len() < expected {
            return Err(Error::SharedMemoryTooSmall {
                actual: mmap.len(),
                expected,
            });
        }
        Ok(Self {
            storage: PcpuOwnerStorage::Mmap(mmap),
            kick: None,
        })
    }

    /// Kicks each pCPU given a new owner with `kick` (see [`Self::kick`]).
    #[must_use]
    pub fn with_kick(mut self, kick: scx_pylon::OwnerKick) -> Self {
        self.kick = Some(kick);
        self
    }

    /// Makes `pcpu_id` schedule again, so the owner just written runs there at once: an owner
    /// that is already runnable isn't enqueued again, and `scx_pylon` doesn't watch the table.
    /// Does nothing without `scx_pylon`.
    #[inline]
    pub fn kick(&self, pcpu_id: usize) -> io::Result<()> {
        match &self.kick {
            Some(kick) => kick.kick(pcpu_id as u32),
            None => Ok(()),
        }
    }

    /// Writes `target_tid` and `flags` for `pcpu_id` without any syscalls.
    #[inline]
    pub fn write_owner(&self, pcpu_id: usize, target_tid: u32, flags: u32) {
        let entry = self.storage.entry(pcpu_id);
        entry.flags.store(flags, Ordering::Relaxed);
        entry.target_tid.store(target_tid, Ordering::Release);
    }

    /// Reads `(target_tid, flags)` for `pcpu_id`.
    #[inline]
    pub fn read_owner(&self, pcpu_id: usize) -> (u32, u32) {
        let entry = self.storage.entry(pcpu_id);
        let tid = entry.target_tid.load(Ordering::Acquire);
        let flags = entry.flags.load(Ordering::Relaxed);
        (tid, flags)
    }
}

impl Default for PcpuOwnerTable {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

/// A started vCPU thread of a registered VM, as `scx_pylon` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcpuThread {
    /// The VMM's process ID.
    pub pid: u32,
    /// The VM's slot (`VM_SLOT_NONE` if it has none).
    pub vm_slot: u16,
    /// The vCPU's index.
    pub vcpu: u16,
}

/// VM slots for `VmVcpusTable`, as in `scx_pylon`'s `pylon_vm_vcpus`.
pub const MAX_VM_SLOTS: usize = scx_pylon::MAX_VM_SLOTS;

/// No VM slot: the VM gets no `VmVcpusTable` entry.
pub const VM_SLOT_NONE: u16 = u16::MAX;

/// One VM's vCPU bitmasks in `pylon_vm_vcpus` (`spec/08-park-in-kvm.md` §4.3).
#[repr(C)]
#[derive(Debug, Default)]
pub struct VmVcpusEntry {
    /// vCPUs that are `PARKED` with no core assigned. Written by the engine.
    pub parked: AtomicU64,
    /// Parked vCPUs that KVM woke and that are runnable. Written by `scx_pylon`.
    pub woken: AtomicU64,
}

const _: () = {
    assert!(size_of::<VmVcpusEntry>() * MAX_VM_SLOTS == scx_pylon::VM_VCPUS_MAP_BYTES);
};

/// Per-VM vCPU bitmasks shared with `scx_pylon`, indexed by VM slot: a memory-mapped
/// `pylon_vm_vcpus`, or heap memory when `scx_pylon` isn't attached.
#[derive(Debug)]
pub struct VmVcpusTable {
    storage: VmVcpusStorage,
}

#[derive(Debug)]
enum VmVcpusStorage {
    Mmap(MmapMut),
    Heap(Box<[VmVcpusEntry; MAX_VM_SLOTS]>),
}

impl VmVcpusTable {
    /// Creates a heap-backed table, for `--no-scx` and tests.
    pub fn new_in_memory() -> Self {
        Self {
            storage: VmVcpusStorage::Heap(Box::new(from_fn(|_| VmVcpusEntry::default()))),
        }
    }

    /// Wraps a mapping of `pylon_vm_vcpus`.
    pub fn from_mmap(mmap: MmapMut) -> Result<Self, Error> {
        let expected = scx_pylon::VM_VCPUS_MAP_BYTES;
        if mmap.len() < expected {
            return Err(Error::SharedMemoryTooSmall {
                actual: mmap.len(),
                expected,
            });
        }
        Ok(Self {
            storage: VmVcpusStorage::Mmap(mmap),
        })
    }

    /// The entry of VM slot `slot`, or `None` if out of range.
    #[inline]
    pub fn entry(&self, slot: u16) -> Option<&VmVcpusEntry> {
        let slot = slot as usize;
        if slot >= MAX_VM_SLOTS {
            return None;
        }
        Some(match &self.storage {
            VmVcpusStorage::Mmap(mmap) => {
                #[expect(
                    clippy::cast_ptr_alignment,
                    reason = "mmap(2) returns page-aligned memory"
                )]
                // SAFETY: `mmap` is page-aligned and at least `MAX_VM_SLOTS` entries long, and
                // `VmVcpusEntry` is two `AtomicU64`s with no invalid bit patterns.
                unsafe {
                    &*mmap.as_ptr().cast::<VmVcpusEntry>().add(slot)
                }
            }
            VmVcpusStorage::Heap(entries) => &entries[slot],
        })
    }
}

impl Default for VmVcpusTable {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

/// JSON payload sent by Cloud Hypervisor over `/run/pylon/pylond.sock` during VM registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationRequest {
    pub vm_id: u64,
    pub num_vcpus: u32,
    pub min_cores: u32,
    pub max_cores: u32,
    pub priority_weight: u32,
    /// Signal that interrupts a vCPU's `KVM_RUN` in the registering VMM: `SIGUSR1`, `SIGUSR2`
    /// or a real-time signal. The Pylon VMM sends `SIGUSR2`, a standard signal, so that the
    /// signals `pylond` sends again while a vCPU waits to run don't queue up. `pylond` sends it
    /// when a revocation's deadline passes (`spec/06-guest-kick-path.md` §4.4). Without it,
    /// `pylond` never signals the VMM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kick_signal: Option<i32>,
    /// Set by a VMM that parks vCPUs inside KVM (`spec/08-park-in-kvm.md`). The wake `eventfd`
    /// can't reach a thread blocked in `KVM_RUN`, so `pylond` also sends `kick_signal` when it
    /// assigns a core, and again until the vCPU leaves `PARKED`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wake_by_signal: bool,
    /// Asks `pylond` to keep the connection open for as long as the VM is registered
    /// (`spec/10-pylond-restart.md` §3.1). The VMM then reads the end of the session as
    /// `pylond` going away, and `pylond` reads it as the VMM going away.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session: bool,
}

/// Whether a VMM may register `signal` as its `kick_signal`: one whose default action a VMM
/// replaces with a handler, so that `pylond` can't stop or kill it by sending one.
fn is_kick_signal(signal: i32) -> bool {
    signal == libc::SIGUSR1
        || signal == libc::SIGUSR2
        || (libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal)
}

/// JSON response returned by `pylond` after processing a `VmRegistrationRequest`, followed by
/// a newline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationResponse {
    pub accepted: bool,
    pub message: String,
    /// Set when `pylond` accepted the VM and keeps the connection open as its session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session: bool,
}

/// JSON payload sent over `/run/pylon/pylond.sock` to update a running VM's elastic policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPolicyUpdateRequest {
    pub vm_id: u64,
    #[serde(default)]
    pub min_cores: Option<u32>,
    #[serde(default)]
    pub max_cores: Option<u32>,
    #[serde(default)]
    pub priority_weight: Option<u32>,
}

/// JSON response returned by `pylond` after processing a `VmPolicyUpdateRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPolicyUpdateResponse {
    pub accepted: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority_weight: Option<u32>,
}

/// Statistics and real-time metering for a registered VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStats {
    pub vm_id: u64,
    pub num_vcpus: u32,
    pub min_cores: u32,
    pub max_cores: u32,
    pub priority_weight: u32,
    pub current_cores: u32,
    pub allocated_core_microseconds: u64,
    pub cooperative_yields: u64,
    pub forced_preemptions: u64,
    pub runnable_threads: u32,
    /// Revocations where the core was handed over although the donor never parked.
    #[serde(default)]
    pub handoffs_without_park: u64,
    /// Time the VM's parked vCPUs spent woken by KVM while the VM owned a core
    /// (`spec/08-park-in-kvm.md` §4.2).
    #[serde(default)]
    pub woken_shared_microseconds: u64,
    /// Time the VM's parked vCPUs spent woken by KVM while the VM owned no core.
    #[serde(default)]
    pub woken_unowned_microseconds: u64,
}

/// JSON payload sent over `/run/pylon/pylond.sock` to query VM statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStatsRequest {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub vm_id: Option<u64>,
}

/// JSON response returned by `pylond` after processing a `VmStatsRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmStatsResponse {
    pub accepted: bool,
    #[serde(default)]
    pub message: String,
    pub vms: Vec<VmStats>,
}

#[derive(Debug)]
struct VmHandleInner {
    vm_id: u64,
    num_vcpus: u32,
    min_cores: AtomicU32,
    max_cores: AtomicU32,
    priority_weight: AtomicU32,
    allocated_core_usec: AtomicU64,
    cooperative_yield_count: AtomicU64,
    forced_preempt_count: AtomicU64,
    handoff_without_park_count: AtomicU64,
    woken_shared_usec: AtomicU64,
    woken_unowned_usec: AtomicU64,
    /// Host counter value when a zero-core wakeup of this VM last took a core from a donor at
    /// its `min_cores`, or 0. Written by Plane 2 only.
    floor_preempt_tsc: AtomicU64,
    /// Set when a write to one of the VM's `eventfd`s blocked until `WriteWatch` interrupted
    /// it. `pylond` writes to none of them again, and Plane 1 drops the VM.
    faulty: AtomicBool,
    park_latency: LatencyHistogram,
    /// From a core's assignment to its vCPU running, for every assignment.
    wake_latency: LatencyHistogram,
    /// From a revocation request to the recipient vCPU running on the revoked core.
    handoff_latency: LatencyHistogram,
    kick_signal: Option<i32>,
    wake_by_signal: bool,
    /// Slot in `VmVcpusTable` and `scx_pylon`'s per-VM queues, given when the handle is
    /// published (`VM_SLOT_NONE` until then, or if all are taken).
    vm_slot: AtomicU32,
    /// The registering VMM's PID from `SO_PEERCRED` (0 if unknown), and a pidfd that keeps
    /// referring to that process, so a reused PID is never signalled.
    vmm_pid: i32,
    vmm_pidfd: Option<OwnedFd>,
    /// `pylond`'s end of the registration connection, kept open while the VM is registered
    /// when the VMM asked for a session. Dropping the last handle closes it, which tells the
    /// VMM that `pylond` no longer manages the VM.
    session: Option<OwnedFd>,
    /// The registering peer's UID from `SO_PEERCRED`, if known. Only that UID, root and
    /// `pylond`'s own UID may change the VM's policy or read its stats.
    owner_uid: Option<u32>,
    shm_mmap: MmapMut,
    kick_irqfds: Vec<OwnedFd>,
    wake_eventfds: Vec<OwnedFd>,
    host_rx_doorbell: AtomicU32,
}

/// Handle to a registered VM shared between Plane 1 (Control) and Plane 2 (Matrix Engine).
///
/// Cloning `VmHandle` is an inexpensive `Arc` bump so Plane 1 can publish new immutable
/// `Arc<Vec<VmHandle>>` snapshots via `ArcSwap` without copying the underlying mapping.
#[derive(Debug, Clone)]
pub struct VmHandle {
    inner: Arc<VmHandleInner>,
}

impl VmHandle {
    /// Constructs a new `VmHandle` from registration parameters, a shared memory file descriptor,
    /// per-vCPU kick `irqfd`s, and per-vCPU wake `eventfd`s.
    pub fn new(
        req: &VmRegistrationRequest,
        shm_fd: OwnedFd,
        kick_irqfds: Vec<OwnedFd>,
        wake_eventfds: Vec<OwnedFd>,
    ) -> Result<Self, Error> {
        if req.num_vcpus == 0 || (req.num_vcpus as usize) > MAX_VCPUS {
            return Err(Error::InvalidRegistration {
                vm_id: req.vm_id,
                reason: format!("num_vcpus {} must be in 1..={MAX_VCPUS}", req.num_vcpus),
            });
        }
        if req.max_cores == 0 || req.min_cores > req.max_cores || req.max_cores > req.num_vcpus {
            return Err(Error::InvalidRegistration {
                vm_id: req.vm_id,
                reason: format!(
                    "invalid core bounds min_cores={} max_cores={} num_vcpus={}",
                    req.min_cores, req.max_cores, req.num_vcpus
                ),
            });
        }
        if let Some(signal) = req.kick_signal
            && !is_kick_signal(signal)
        {
            return Err(Error::InvalidRegistration {
                vm_id: req.vm_id,
                reason: format!(
                    "kick_signal {signal} is not SIGUSR1, SIGUSR2 or a real-time signal"
                ),
            });
        }
        let expected_vcpus = req.num_vcpus as usize;
        if kick_irqfds.len() != expected_vcpus || wake_eventfds.len() != expected_vcpus {
            return Err(Error::InvalidFdCount {
                actual: 1 + kick_irqfds.len() + wake_eventfds.len(),
                expected: 1 + 2 * expected_vcpus,
            });
        }

        let shm_file = File::from(shm_fd);
        check_shared_memory_file(&shm_file, req.vm_id)?;
        for fd in kick_irqfds.iter().chain(&wake_eventfds) {
            check_eventfd(fd, req.vm_id)?;
        }
        // SAFETY: `shm_file` refers to the Pylon shared memory file, whose contents are
        // accessed solely via atomic operations defined in `pylon-abi`. Its size is sealed
        // at `PYLON_SHM_FILE_SIZE` or more, so the mapping never extends past its end.
        let shm_mmap = unsafe {
            MmapOptions::new()
                .len(PYLON_SHM_FILE_SIZE)
                .map_mut(&shm_file)
                .map_err(Error::Io)?
        };

        let handle = Self {
            inner: Arc::new(VmHandleInner {
                vm_id: req.vm_id,
                num_vcpus: req.num_vcpus,
                min_cores: AtomicU32::new(req.min_cores),
                max_cores: AtomicU32::new(req.max_cores),
                priority_weight: AtomicU32::new(req.priority_weight.max(1)),
                allocated_core_usec: AtomicU64::new(0),
                cooperative_yield_count: AtomicU64::new(0),
                forced_preempt_count: AtomicU64::new(0),
                handoff_without_park_count: AtomicU64::new(0),
                woken_shared_usec: AtomicU64::new(0),
                woken_unowned_usec: AtomicU64::new(0),
                floor_preempt_tsc: AtomicU64::new(0),
                faulty: AtomicBool::new(false),
                park_latency: LatencyHistogram::default(),
                wake_latency: LatencyHistogram::default(),
                handoff_latency: LatencyHistogram::default(),
                kick_signal: req.kick_signal,
                wake_by_signal: req.wake_by_signal,
                vmm_pid: 0,
                vmm_pidfd: None,
                session: None,
                owner_uid: None,
                shm_mmap,
                kick_irqfds,
                wake_eventfds,
                host_rx_doorbell: AtomicU32::new(0),
                vm_slot: AtomicU32::new(u32::from(VM_SLOT_NONE)),
            }),
        };

        if !handle.shared_page().header.is_valid() {
            return Err(Error::InvalidSharedHeader { vm_id: req.vm_id });
        }
        if !handle.host_page().header.is_valid() {
            return Err(Error::InvalidHostHeader { vm_id: req.vm_id });
        }

        Ok(handle)
    }

    #[inline]
    pub fn vm_id(&self) -> u64 {
        self.inner.vm_id
    }

    #[inline]
    pub fn num_vcpus(&self) -> u32 {
        self.inner.num_vcpus
    }

    #[inline]
    pub fn min_cores(&self) -> u32 {
        self.inner.min_cores.load(Ordering::Acquire)
    }

    #[inline]
    pub fn max_cores(&self) -> u32 {
        self.inner.max_cores.load(Ordering::Acquire)
    }

    #[inline]
    pub fn priority_weight(&self) -> u32 {
        self.inner.priority_weight.load(Ordering::Acquire)
    }

    /// Cumulative physical core-microseconds allocated to this VM.
    #[inline]
    pub fn allocated_core_usec(&self) -> u64 {
        self.inner.allocated_core_usec.load(Ordering::Acquire)
    }

    /// Atomically accumulates physical core-microseconds allocated to this VM.
    #[inline]
    pub fn add_allocated_core_usec(&self, delta: u64) {
        self.inner
            .allocated_core_usec
            .fetch_add(delta, Ordering::AcqRel);
    }

    /// Cumulative number of voluntary cooperative yields performed by this VM.
    #[inline]
    pub fn cooperative_yield_count(&self) -> u64 {
        self.inner.cooperative_yield_count.load(Ordering::Acquire)
    }

    /// Atomically increments the voluntary cooperative yield count for this VM.
    #[inline]
    pub fn record_cooperative_yield(&self) {
        self.inner
            .cooperative_yield_count
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Cumulative number of forced preemptions (hard deadline expirations) on this VM.
    #[inline]
    pub fn forced_preempt_count(&self) -> u64 {
        self.inner.forced_preempt_count.load(Ordering::Acquire)
    }

    /// Atomically increments the forced preemption count for this VM.
    #[inline]
    pub fn record_forced_preempt(&self) {
        self.inner
            .forced_preempt_count
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Cumulative number of revocations handed over although the donor vCPU never parked.
    #[inline]
    pub fn handoff_without_park_count(&self) -> u64 {
        self.inner
            .handoff_without_park_count
            .load(Ordering::Acquire)
    }

    /// Atomically increments the handoff-without-park count for this VM.
    #[inline]
    pub fn record_handoff_without_park(&self) {
        self.inner
            .handoff_without_park_count
            .fetch_add(1, Ordering::AcqRel);
    }

    /// Cumulative time the VM's parked vCPUs spent woken by KVM while the VM owned a core, in
    /// microseconds.
    #[inline]
    pub fn woken_shared_usec(&self) -> u64 {
        self.inner.woken_shared_usec.load(Ordering::Acquire)
    }

    /// Cumulative time the VM's parked vCPUs spent woken by KVM while the VM owned no core, in
    /// microseconds.
    #[inline]
    pub fn woken_unowned_usec(&self) -> u64 {
        self.inner.woken_unowned_usec.load(Ordering::Acquire)
    }

    /// When a zero-core wakeup of this VM last took a core from a donor at its `min_cores`.
    #[inline]
    pub fn floor_preempt_tsc(&self) -> u64 {
        self.inner.floor_preempt_tsc.load(Ordering::Relaxed)
    }

    /// Records that a zero-core wakeup of this VM took a core from a donor at its `min_cores`.
    #[inline]
    pub fn set_floor_preempt_tsc(&self, now_tsc: u64) {
        self.inner
            .floor_preempt_tsc
            .store(now_tsc, Ordering::Relaxed);
    }

    /// Adds `usec` of woken vCPU time, shared on the VM's own cores if `owned`.
    #[inline]
    pub fn add_woken_usec(&self, owned: bool, usec: u64) {
        let counter = if owned {
            &self.inner.woken_shared_usec
        } else {
            &self.inner.woken_unowned_usec
        };
        counter.fetch_add(usec, Ordering::AcqRel);
    }

    /// Records the time from a revocation request to the donor vCPU parking.
    #[inline]
    pub fn record_park_latency_us(&self, usec: u64) {
        self.inner.park_latency.record(usec);
    }

    /// Returns the revocation-to-park latency histogram for this VM.
    #[inline]
    pub fn park_latency(&self) -> &LatencyHistogram {
        &self.inner.park_latency
    }

    /// Records the time from assigning a core to one of this VM's vCPUs to that vCPU running.
    #[inline]
    pub fn record_wake_latency_us(&self, usec: u64) {
        self.inner.wake_latency.record(usec);
    }

    /// Returns the assignment-to-running latency histogram for this VM.
    #[inline]
    pub fn wake_latency(&self) -> &LatencyHistogram {
        &self.inner.wake_latency
    }

    /// Records the time from a revocation request to this VM's recipient vCPU running.
    #[inline]
    pub fn record_handoff_latency_us(&self, usec: u64) {
        self.inner.handoff_latency.record(usec);
    }

    /// Returns the revocation-to-recipient-running latency histogram for this VM.
    #[inline]
    pub fn handoff_latency(&self) -> &LatencyHistogram {
        &self.inner.handoff_latency
    }

    /// Binds this freshly registered VM to the VMM process `pid` that registered it, so
    /// `signal_vcpu` can reach its vCPU threads. Opens a pidfd so a reused PID is never signalled.
    pub fn with_vmm_pid(mut self, pid: i32) -> Result<Self, Error> {
        // SAFETY: `pidfd_open` takes a PID and flags and returns a new file descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(Error::PidfdOpen {
                pid,
                source: io::Error::last_os_error(),
            });
        }
        // SAFETY: `fd` is a new pidfd owned by nothing else.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let vm_id = self.inner.vm_id;
        let inner = Arc::get_mut(&mut self.inner).ok_or(Error::InvalidRegistration {
            vm_id,
            reason: "VM handle is already shared".to_owned(),
        })?;
        inner.vmm_pid = pid;
        inner.vmm_pidfd = Some(pidfd);
        Ok(self)
    }

    /// Records the UID of the peer that registered this VM.
    pub fn with_owner_uid(mut self, uid: u32) -> Result<Self, Error> {
        let vm_id = self.inner.vm_id;
        let inner = Arc::get_mut(&mut self.inner).ok_or(Error::InvalidRegistration {
            vm_id,
            reason: "VM handle is already shared".to_owned(),
        })?;
        inner.owner_uid = Some(uid);
        Ok(self)
    }

    /// Keeps `session`, `pylond`'s end of the registration connection, open for as long as
    /// the VM is registered (`spec/10-pylond-restart.md` §3.1).
    pub fn with_session(mut self, session: OwnedFd) -> Result<Self, Error> {
        let vm_id = self.inner.vm_id;
        let inner = Arc::get_mut(&mut self.inner).ok_or(Error::InvalidRegistration {
            vm_id,
            reason: "VM handle is already shared".to_owned(),
        })?;
        inner.session = Some(session);
        Ok(self)
    }

    /// Whether the VMM closed its end of the VM's session. `false` for a VM without one.
    pub fn session_closed(&self) -> bool {
        let Some(session) = &self.inner.session else {
            return false;
        };
        let mut byte = 0u8;
        // SAFETY: `recv` writes at most one byte into `byte`, which outlives the call. With
        // `MSG_PEEK | MSG_DONTWAIT` it neither consumes data nor blocks.
        let ret = unsafe {
            libc::recv(
                session.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match ret {
            // End of file: the VMM closed the socket.
            0 => true,
            // The VMM sends nothing over a session; anything it sent doesn't end it.
            1.. => false,
            _ => !matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        }
    }

    /// The UID of the peer that registered this VM, if known.
    #[inline]
    pub fn owner_uid(&self) -> Option<u32> {
        self.inner.owner_uid
    }

    /// Whether a control socket peer with UID `uid` may change this VM's policy or read its
    /// stats: root, `pylond`'s own UID, or the UID that registered the VM.
    pub fn is_managed_by(&self, uid: Option<u32>) -> bool {
        let Some(uid) = uid else {
            return false;
        };
        // SAFETY: `geteuid` takes no arguments and can't fail.
        let own_uid = unsafe { libc::geteuid() };
        uid == 0 || uid == own_uid || self.inner.owner_uid == Some(uid)
    }

    /// Whether the VMM process that registered this VM has exited: `Some(true)` or
    /// `Some(false)` when it is known through a pidfd, `None` when the registration had no PID.
    pub fn vmm_exited(&self) -> Option<bool> {
        let pidfd = self.inner.vmm_pidfd.as_ref()?;
        // SAFETY: `pidfd_send_signal` with signal 0 only checks that the process behind `pidfd`
        // still exists; the null `info` pointer is allowed.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                0,
                ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        Some(ret != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    }

    /// The VM's started vCPU threads, by thread ID. Empty when the registration had no PID.
    pub fn vcpu_threads(&self) -> impl Iterator<Item = (u32, VcpuThread)> + '_ {
        let pid = u32::try_from(self.inner.vmm_pid).unwrap_or(0);
        let num_vcpus = if pid == 0 {
            0
        } else {
            (self.inner.num_vcpus as usize).min(MAX_VCPUS)
        };
        let vm_slot = self.vm_slot();
        self.host_page().vcpus[..num_vcpus]
            .iter()
            .enumerate()
            .filter_map(move |(vcpu, slot)| {
                let tid = slot.host_tid.load(Ordering::Acquire);
                (tid != 0).then_some((
                    tid,
                    VcpuThread {
                        pid,
                        vm_slot,
                        vcpu: vcpu as u16,
                    },
                ))
            })
    }

    /// The VM's slot in `VmVcpusTable`, or `VM_SLOT_NONE`.
    #[inline]
    pub fn vm_slot(&self) -> u16 {
        u16::try_from(self.inner.vm_slot.load(Ordering::Relaxed)).unwrap_or(VM_SLOT_NONE)
    }

    /// Sets the VM's slot. `ControlPlane::publish_vm_handle` gives each VM the lowest free one.
    pub fn set_vm_slot(&self, slot: u16) {
        self.inner.vm_slot.store(u32::from(slot), Ordering::Relaxed);
    }

    /// Whether the VMM parks vCPUs inside KVM, so waking one takes a signal as well as the wake
    /// `eventfd` (`spec/08-park-in-kvm.md` §3.3).
    #[inline]
    pub fn wake_by_signal(&self) -> bool {
        self.inner.wake_by_signal
    }

    /// Sends the VMM's `kick_signal` to the thread of `vcpu_idx`, interrupting its `KVM_RUN`
    /// (`spec/06-guest-kick-path.md` §4.4). Returns `Ok(false)` without sending when the VMM gave
    /// no signal, its PID is unknown, the vCPU's thread ID isn't known yet, or the VMM has exited.
    pub fn signal_vcpu(&self, vcpu_idx: usize) -> Result<bool, Error> {
        let slot = self
            .host_page()
            .vcpus
            .get(vcpu_idx)
            .filter(|_| vcpu_idx < self.inner.num_vcpus as usize)
            .ok_or(Error::VcpuOutOfBounds {
                vcpu: vcpu_idx,
                num_vcpus: self.inner.num_vcpus,
            })?;
        let tid = slot.host_tid.load(Ordering::Acquire);
        let Some(signal) = self.inner.kick_signal else {
            return Ok(false);
        };
        if tid == 0 || self.vmm_exited() != Some(false) {
            return Ok(false);
        }
        let pid = self.inner.vmm_pid;
        // SAFETY: `tgkill` takes plain integers and only signals `tid` if it is a thread of `pid`.
        let ret = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid as libc::pid_t, signal) };
        if ret != 0 {
            return Err(Error::SignalVcpu {
                vcpu: vcpu_idx,
                tid,
                pid,
                source: io::Error::last_os_error(),
            });
        }
        Ok(true)
    }

    /// Asks the VMM to park the vCPUs it tagged `park_on_boot`, once the guest has booted: its
    /// `guest_heartbeat` is non-zero. The VMM left those vCPUs running so the guest could bring
    /// them online. For each one still running, sets `revoke_requested` and signals its thread,
    /// and the VMM parks it at once. Call it again until it returns 0: a signal that lands
    /// outside `KVM_RUN` doesn't stop the next guest entry, so a vCPU still tagged is asked
    /// again. Returns the number of vCPUs asked.
    pub fn park_booted_vcpus(&self) -> usize {
        let page = self.shared_page();
        if page.telemetry.guest_heartbeat.load(Ordering::Acquire) == 0 {
            return 0;
        }
        let num_vcpus = (self.inner.num_vcpus as usize).min(MAX_VCPUS);
        let mut asked = 0;
        for (vcpu_idx, slot) in self.host_page().vcpus[..num_vcpus].iter().enumerate() {
            if slot.park_on_boot.load(Ordering::Acquire) == 0
                || slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
            {
                continue;
            }
            // The deadline doesn't matter to a boot park, and 0 keeps it from ever looking like
            // a revocation that is still inside its grace period.
            self.request_park(vcpu_idx, 0);
            match self.signal_vcpu(vcpu_idx) {
                Ok(_) => asked += 1,
                Err(err) => warn!(
                    "VM {}: couldn't ask vCPU {vcpu_idx} to park: {err}",
                    self.inner.vm_id
                ),
            }
        }
        asked
    }

    /// Dynamically updates the elastic core bounds and scheduling weight for this VM.
    pub fn update_policy(
        &self,
        min_cores: Option<u32>,
        max_cores: Option<u32>,
        priority_weight: Option<u32>,
    ) -> Result<(u32, u32, u32), Error> {
        let cur_min = self.inner.min_cores.load(Ordering::Acquire);
        let cur_max = self.inner.max_cores.load(Ordering::Acquire);
        let cur_weight = self.inner.priority_weight.load(Ordering::Acquire);

        let new_min = min_cores.unwrap_or(cur_min);
        let new_max = max_cores.unwrap_or(cur_max);
        let new_weight = priority_weight.unwrap_or(cur_weight).max(1);

        if new_max == 0 || new_min > new_max || new_max > self.inner.num_vcpus {
            return Err(Error::InvalidPolicyUpdate {
                vm_id: self.inner.vm_id,
                reason: format!(
                    "invalid core bounds min_cores={} max_cores={} num_vcpus={}",
                    new_min, new_max, self.inner.num_vcpus
                ),
            });
        }

        self.inner.min_cores.store(new_min, Ordering::Release);
        self.inner.max_cores.store(new_max, Ordering::Release);
        self.inner
            .priority_weight
            .store(new_weight, Ordering::Release);

        Ok((new_min, new_max, new_weight))
    }

    /// Returns a reference to the memory-mapped `PylonSharedPage`.
    #[inline]
    pub fn shared_page(&self) -> &PylonSharedPage {
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "mmap(2) returns page-aligned memory"
        )]
        // SAFETY: `shm_mmap` length was verified to be >= `PYLON_SHM_FILE_SIZE`
        // and `mmap` guarantees page alignment (4096 bytes), matching `PylonSharedPage`.
        unsafe {
            &*(self.inner.shm_mmap.as_ptr().cast::<PylonSharedPage>())
        }
    }

    /// Returns a reference to the memory-mapped `PylonHostPage`. The guest can write the shared
    /// page, so whether a vCPU parked, and the revocations and cores `pylond` gives it, are read
    /// from here.
    #[inline]
    pub fn host_page(&self) -> &PylonHostPage {
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "mmap(2) returns page-aligned memory"
        )]
        // SAFETY: `shm_mmap` length was verified to be >= `PYLON_SHM_FILE_SIZE`, and the host page
        // lies at `PYLON_HOST_PAGE_OFFSET`, a multiple of its 4096-byte alignment.
        unsafe {
            &*(self
                .inner
                .shm_mmap
                .as_ptr()
                .add(PYLON_HOST_PAGE_OFFSET)
                .cast::<PylonHostPage>())
        }
    }

    /// Sets the core assigned to, or reserved for, `vcpu_idx`, and the guest's copy.
    #[inline]
    pub fn set_assigned_pcpu(&self, vcpu_idx: usize, pcpu: u8) {
        self.host_page().vcpus[vcpu_idx]
            .assigned_pcpu
            .store(pcpu, Ordering::Release);
        self.shared_page().vcpus[vcpu_idx]
            .assigned_pcpu
            .store(pcpu, Ordering::Release);
    }

    /// Asks the VMM to park `vcpu_idx`, forcibly from `deadline_tsc` on: sets the deadline, then
    /// `revoke_requested`, in the host page and in the guest's copy, from which the guest
    /// learns of the request.
    #[inline]
    pub fn request_park(&self, vcpu_idx: usize, deadline_tsc: u64) {
        let host = &self.host_page().vcpus[vcpu_idx];
        host.revoke_deadline_tsc
            .store(deadline_tsc, Ordering::Relaxed);
        host.revoke_requested.store(1, Ordering::Release);
        let guest = &self.shared_page().vcpus[vcpu_idx];
        guest
            .revoke_deadline_tsc
            .store(deadline_tsc, Ordering::Relaxed);
        guest.revoke_requested.store(1, Ordering::Release);
    }

    /// Reads the host-side vDPA/virtio RX doorbell index for 0-core wakeups.
    #[inline]
    pub fn host_rx_doorbell(&self) -> u32 {
        self.inner.host_rx_doorbell.load(Ordering::Acquire)
    }

    /// Updates the host-side vDPA/virtio RX doorbell index for 0-core wakeups.
    #[inline]
    pub fn set_host_rx_doorbell(&self, value: u32) {
        self.inner.host_rx_doorbell.store(value, Ordering::Release);
    }

    /// Whether a write to one of the VM's `eventfd`s blocked (see `faulty`).
    #[inline]
    pub fn is_faulty(&self) -> bool {
        self.inner.faulty.load(Ordering::Relaxed)
    }

    /// Marks the VM as one whose `eventfd` writes block.
    pub fn mark_faulty(&self) {
        self.inner.faulty.store(true, Ordering::Relaxed);
    }

    /// Signals the KVM `irqfd` for `vcpu_idx` to inject an immediate revocation IPI.
    #[inline]
    pub fn kick_revoke_irqfd(&self, vcpu_idx: usize) -> Result<(), Error> {
        if self.is_faulty() {
            return Ok(());
        }
        let fd = self
            .inner
            .kick_irqfds
            .get(vcpu_idx)
            .ok_or(Error::VcpuOutOfBounds {
                vcpu: vcpu_idx,
                num_vcpus: self.inner.num_vcpus,
            })?;
        write_eventfd(fd.as_raw_fd())
    }

    /// Signals the wake `eventfd` for `vcpu_idx` to unpark a recipient vCPU thread.
    #[inline]
    pub fn wake_vcpu(&self, vcpu_idx: usize) -> Result<(), Error> {
        if self.is_faulty() {
            return Ok(());
        }
        let fd = self
            .inner
            .wake_eventfds
            .get(vcpu_idx)
            .ok_or(Error::VcpuOutOfBounds {
                vcpu: vcpu_idx,
                num_vcpus: self.inner.num_vcpus,
            })?;
        write_eventfd(fd.as_raw_fd())
    }
}

/// Number of finite buckets in [`LATENCY_BUCKETS_US`].
const LATENCY_BUCKET_COUNT: usize = 16 + 8 * 8;

/// Upper bounds, in microseconds, of the latency histogram buckets: every microsecond up to
/// 16 µs, around the 8 µs grace (`spec/06-guest-kick-path.md` §8 S6), then eight buckets per
/// doubling up to 4,096 µs, so a percentile read from the buckets is within 12.5%.
pub const LATENCY_BUCKETS_US: [u64; LATENCY_BUCKET_COUNT] = latency_buckets();

const fn latency_buckets() -> [u64; LATENCY_BUCKET_COUNT] {
    let mut bounds = [0u64; LATENCY_BUCKET_COUNT];
    let mut i = 0;
    while i < 16 {
        bounds[i] = i as u64 + 1;
        i += 1;
    }
    let mut base = 16u64;
    while i < LATENCY_BUCKET_COUNT {
        let mut step = 1;
        while step <= 8 {
            bounds[i] = base + base * step / 8;
            step += 1;
            i += 1;
        }
        base *= 2;
    }
    bounds
}

/// Allocation-free latency histogram, in microseconds. Written by Plane 2, read by the
/// Prometheus exporter.
#[derive(Debug)]
pub struct LatencyHistogram {
    /// Per-bucket counts (not cumulative); the last entry counts values above every bound.
    buckets: [AtomicU64; LATENCY_BUCKETS_US.len() + 1],
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: from_fn(|_| AtomicU64::new(0)),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl LatencyHistogram {
    /// Records one latency of `usec` microseconds.
    pub fn record(&self, usec: u64) {
        let idx = LATENCY_BUCKETS_US.partition_point(|&bound| bound < usec);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(usec, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Cumulative counts for each bound in `LATENCY_BUCKETS_US`, then for `+Inf`.
    pub fn cumulative_buckets(&self) -> [u64; LATENCY_BUCKETS_US.len() + 1] {
        let mut total = 0u64;
        from_fn(|i| {
            total += self.buckets[i].load(Ordering::Relaxed);
            total
        })
    }

    /// Sum of all recorded latencies, in microseconds.
    pub fn sum_us(&self) -> u64 {
        self.sum_us.load(Ordering::Relaxed)
    }

    /// Number of recorded latencies.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }
}

/// Lock-free snapshot container used by Plane 1 to publish active `VmHandle` lists to Plane 2.
pub type VmSnapshot = ArcSwap<Vec<VmHandle>>;

/// Per-pCPU arbitration state maintained by Plane 2 (`spec/02-pylond-arbiter.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreState {
    Idle,
    Assigned {
        vm_id: u64,
        vcpu: u8,
    },
    Revoking {
        donor_vm_id: u64,
        donor_vcpu: u8,
        recipient_vm_id: u64,
        recipient_vcpu: u8,
        deadline_tsc: u64,
    },
}

/// Creates a shared memory file as `pylond` accepts it, for VMMs simulated in tests and
/// benchmarks: a `memfd` named `name`, `PYLON_SHM_FILE_SIZE` bytes long, with its size sealed.
pub fn create_shared_memory_file(name: &str) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: `name` is a valid NUL-terminated string, and the flags are valid.
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new file descriptor that nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(PYLON_SHM_FILE_SIZE as u64)?;
    // SAFETY: `F_ADD_SEALS` takes an integer of seal flags on a valid file descriptor.
    if unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Checks that a registrant's shared memory file can't make `pylond` fault: a `memfd` on
/// shmem whose size is sealed (`F_SEAL_SHRINK`, `F_SEAL_GROW`) at `PYLON_SHM_FILE_SIZE` or more.
/// A file that could shrink would raise `SIGBUS` in `pylond` at its next access, and one on
/// another file system (FUSE, hugetlbfs) could stall or fail page faults.
fn check_shared_memory_file(file: &File, vm_id: u64) -> Result<(), Error> {
    let required = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    // SAFETY: `F_GET_SEALS` only reads the seals of a valid file descriptor.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & required != required {
        return Err(Error::InvalidRegistration {
            vm_id,
            reason: "the shared memory file isn't a memfd sealed with F_SEAL_SHRINK and \
                     F_SEAL_GROW"
                .to_owned(),
        });
    }
    // SAFETY: `fstatfs` writes only to `stat`, a valid `statfs`.
    let mut stat: libc::statfs = unsafe { zeroed() };
    // SAFETY: `file` is a valid file descriptor and `stat` is a valid `statfs`.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    if stat.f_type != libc::TMPFS_MAGIC {
        return Err(Error::InvalidRegistration {
            vm_id,
            reason: "the shared memory file isn't on shmem".to_owned(),
        });
    }
    // The seals fix the size, so it can't change after this check.
    let len = file.metadata().map_err(Error::Io)?.len();
    if len < PYLON_SHM_FILE_SIZE as u64 {
        return Err(Error::SharedMemoryTooSmall {
            actual: usize::try_from(len).unwrap_or(usize::MAX),
            expected: PYLON_SHM_FILE_SIZE,
        });
    }
    Ok(())
}

/// Checks that a registrant's kick or wake file descriptor is an `eventfd`. A pipe or a FIFO
/// could block `pylond`'s writes indefinitely.
fn check_eventfd(fd: &OwnedFd, vm_id: u64) -> Result<(), Error> {
    let link = fs::read_link(Path::new("/proc/self/fd").join(fd.as_raw_fd().to_string()))
        .map_err(Error::Io)?;
    if link != Path::new("anon_inode:[eventfd]") {
        return Err(Error::InvalidRegistration {
            vm_id,
            reason: format!("a kick or wake file descriptor isn't an eventfd ({link:?})"),
        });
    }
    Ok(())
}

/// How long a write to a registrant's `eventfd` may block before `WriteWatch` interrupts it.
/// A write blocks only when the counter is about to overflow, which takes a registrant that
/// fills it on purpose; a well-behaved write takes microseconds.
pub const BLOCKED_WRITE_LIMIT: Duration = Duration::from_millis(1);

/// Lets a watchdog thread interrupt the engine's writes to registrants' `eventfd`s. Registration
/// checks that each is an `eventfd`, but the registrant shares the open file and can make it
/// blocking and fill its counter, so that `pylond`'s next write would wait for ever.
pub struct WriteWatch {
    base: Instant,
    signal: i32,
    /// The watched thread's ID, or 0 before `bind_current_thread`.
    tid: AtomicI32,
    /// Nanoseconds since `base` when the current write started, or 0 outside one.
    started_ns: AtomicU64,
}

impl WriteWatch {
    /// A watch that interrupts writes with `signal`, which must have a handler installed
    /// without `SA_RESTART` (`install_interrupt_handler`).
    pub fn new(signal: i32) -> Self {
        Self {
            base: Instant::now(),
            signal,
            tid: AtomicI32::new(0),
            started_ns: AtomicU64::new(0),
        }
    }

    /// Installs a handler for `signal` that does nothing and doesn't restart the interrupted
    /// call, so a blocked `write` returns `EINTR`.
    pub fn install_interrupt_handler(signal: i32) -> io::Result<()> {
        extern "C" fn interrupt(_: libc::c_int) {}
        // SAFETY: `action` is zeroed and then filled in; `interrupt` is async-signal-safe, and
        // `sigaction` only reads `action`.
        unsafe {
            let mut action: libc::sigaction = zeroed();
            action.sa_sigaction = interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Makes the calling thread the one whose writes are watched.
    pub fn bind_current_thread(&self) {
        // SAFETY: `gettid` takes no arguments and can't fail.
        self.tid.store(unsafe { libc::gettid() }, Ordering::Relaxed);
    }

    /// Runs `write`, which `interrupt_blocked` may interrupt. If it returns `EINTR` after
    /// blocking for `BLOCKED_WRITE_LIMIT`, marks `vm` faulty. A shorter one was hit by an
    /// interrupt meant for an earlier write and is tried once more.
    pub fn write(&self, vm: &VmHandle, write: impl Fn() -> Result<(), Error>) -> Result<(), Error> {
        for _ in 0..2 {
            let start = self.now_ns();
            self.started_ns.store(start, Ordering::Release);
            let result = write();
            self.started_ns.store(0, Ordering::Release);
            match result {
                Err(Error::Io(err)) if err.kind() == io::ErrorKind::Interrupted => {
                    if Duration::from_nanos(self.now_ns() - start) >= BLOCKED_WRITE_LIMIT {
                        warn!(
                            "VM {}: a write to its eventfd blocked; dropping the VM",
                            vm.vm_id()
                        );
                        vm.mark_faulty();
                        return Ok(());
                    }
                }
                result => return result,
            }
        }
        Ok(())
    }

    /// Interrupts the watched thread if its current write has run for `limit`. Called by the
    /// watchdog thread every `limit` or so. Returns whether it sent the signal.
    pub fn interrupt_blocked(&self, limit: Duration) -> bool {
        let started = self.started_ns.load(Ordering::Acquire);
        let tid = self.tid.load(Ordering::Relaxed);
        if started == 0 || tid == 0 || Duration::from_nanos(self.now_ns() - started) < limit {
            return false;
        }
        // SAFETY: `tgkill` takes plain integers and only signals `tid` if it is a thread of
        // this process.
        unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, self.signal) };
        true
    }

    /// Nanoseconds since `base`, at least 1, so that 0 can mean "no write".
    fn now_ns(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .max(1)
    }
}

/// Writes `1u64` to an `eventfd` or `irqfd` file descriptor.
#[inline]
pub fn write_eventfd(fd: RawFd) -> Result<(), Error> {
    let val: u64 = 1;
    // SAFETY: `fd` is a valid file descriptor and `&val` points to an 8-byte buffer.
    let ret = unsafe {
        libc::write(
            fd,
            (&raw const val).cast::<libc::c_void>(),
            size_of::<u64>(),
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return Err(Error::Io(err));
    }
    Ok(())
}

/// Reads a `u64` counter from an `eventfd` file descriptor (useful in tests).
#[inline]
pub fn read_eventfd(fd: RawFd) -> Result<u64, Error> {
    let mut val: u64 = 0;
    // SAFETY: `fd` is a valid file descriptor and `&mut val` points to an 8-byte mutable buffer.
    let ret = unsafe { libc::read(fd, (&raw mut val).cast::<libc::c_void>(), size_of::<u64>()) };
    if ret < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    Ok(val)
}

/// Creates a non-blocking `eventfd` wrapped in `OwnedFd`.
pub fn create_eventfd() -> Result<OwnedFd, Error> {
    // SAFETY: `libc::eventfd` with `EFD_CLOEXEC | EFD_NONBLOCK` has no memory safety preconditions.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: `fd` is a freshly created valid file descriptor owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Pins the calling thread to `core_id` using `libc::sched_setaffinity`.
pub fn pin_current_thread_to_core(core_id: usize) -> Result<(), Error> {
    if core_id >= libc::CPU_SETSIZE as usize {
        return Err(Error::PcpuOutOfBounds {
            pcpu: core_id,
            max: libc::CPU_SETSIZE as usize,
        });
    }
    // SAFETY: `cpu_set_t` is plain-old-data and safe to zero-initialize; `CPU_SET` and
    // `sched_setaffinity(0, ...)` operate on the current thread with a valid mask pointer.
    unsafe {
        let mut cpuset: libc::cpu_set_t = zeroed();
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(core_id, &mut cpuset);
        let rc = libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &cpuset);
        if rc != 0 {
            return Err(Error::CpuAffinity {
                core_id,
                source: io::Error::last_os_error(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::{Command, id as process_id};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::time::{Duration, Instant};
    use std::{fs, thread};

    use pylon_abi::{PCPU_UNASSIGNED, VCPU_STATE_PARKED, VCPU_STATE_RUNNING};
    use tokio::sync::watch;
    use tokio::task::spawn_blocking;
    use tokio::time::sleep;

    use super::*;
    use crate::control::{ControlPlane, send_policy_update, send_registration};
    use crate::engine::{
        MatrixEngine, SPARE_CORE_TENURE_US, calibrate_tsc_khz, detect_timed_wait, pause_cycles,
        read_tsc,
    };

    /// Counter rate of the test VMs' pages and engines: tick values in the tests assume it, and
    /// the engine's own default is the host's rate, which differs on aarch64.
    const TEST_TSC_KHZ: u64 = 3_000_000;

    /// An engine that uses [`TEST_TSC_KHZ`] for every VM.
    fn test_engine(
        snapshot: Arc<VmSnapshot>,
        pcpu_owners: Arc<PcpuOwnerTable>,
        tenant_pcpus: &[usize],
    ) -> MatrixEngine {
        let mut engine = MatrixEngine::new(snapshot, pcpu_owners, tenant_pcpus);
        engine.set_host_tsc_khz(TEST_TSC_KHZ);
        engine
    }

    pub(crate) struct TestVmRig {
        pub(crate) req: VmRegistrationRequest,
        pub(crate) shm_fd: OwnedFd,
        pub(crate) kick_fds: Vec<OwnedFd>,
        pub(crate) wake_fds: Vec<OwnedFd>,
        pub(crate) handle: VmHandle,
    }

    fn create_memfd_shared_page(vm_id: u64, num_vcpus: u32, tsc_khz: u64) -> OwnedFd {
        let file = create_shared_memory_file(&format!("pylon-test-vm-{vm_id}")).unwrap();
        let owned = OwnedFd::from(file.try_clone().unwrap());
        // SAFETY: `file` is truncated to `PYLON_SHM_FILE_SIZE`.
        let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "mmap(2) returns page-aligned memory"
        )]
        // SAFETY: `mmap` is page-aligned and `PYLON_SHM_FILE_SIZE` bytes, and the host page lies
        // at a page-aligned offset in it.
        let (page, host_page) = unsafe {
            (
                &mut *(mmap.as_mut_ptr().cast::<PylonSharedPage>()),
                &mut *(mmap
                    .as_mut_ptr()
                    .add(PYLON_HOST_PAGE_OFFSET)
                    .cast::<PylonHostPage>()),
            )
        };
        page.init(vm_id, num_vcpus, tsc_khz);
        host_page.init();
        for (vcpu_idx, (slot, host)) in page
            .vcpus
            .iter()
            .zip(&host_page.vcpus)
            .enumerate()
            .take(num_vcpus as usize)
        {
            let tid = (vm_id as u32) * 1000 + (vcpu_idx as u32) + 1;
            host.host_tid.store(tid, Ordering::Release);
            slot.host_tid.store(tid, Ordering::Release);
        }
        owned
    }

    pub(crate) fn build_test_vm(
        vm_id: u64,
        num_vcpus: u32,
        min_cores: u32,
        max_cores: u32,
        priority_weight: u32,
    ) -> TestVmRig {
        let req = VmRegistrationRequest {
            vm_id,
            num_vcpus,
            min_cores,
            max_cores,
            priority_weight,
            kick_signal: None,
            wake_by_signal: false,
            session: false,
        };
        let shm_fd = create_memfd_shared_page(vm_id, num_vcpus, TEST_TSC_KHZ);
        let mut kick_fds = Vec::with_capacity(num_vcpus as usize);
        let mut wake_fds = Vec::with_capacity(num_vcpus as usize);
        let mut handle_kick_fds = Vec::with_capacity(num_vcpus as usize);
        let mut handle_wake_fds = Vec::with_capacity(num_vcpus as usize);

        for _ in 0..num_vcpus {
            let k = create_eventfd().unwrap();
            let w = create_eventfd().unwrap();
            handle_kick_fds.push(k.try_clone().unwrap());
            handle_wake_fds.push(w.try_clone().unwrap());
            kick_fds.push(k);
            wake_fds.push(w);
        }

        let handle = VmHandle::new(
            &req,
            shm_fd.try_clone().unwrap(),
            handle_kick_fds,
            handle_wake_fds,
        )
        .unwrap();

        TestVmRig {
            req,
            shm_fd,
            kick_fds,
            wake_fds,
            handle,
        }
    }

    fn unique_socket_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = process_id();
        PathBuf::from(format!("/tmp/pylond-test-{pid}-{label}-{id}.sock"))
    }

    #[tokio::test]
    async fn test_plane1_unix_socket_registration_scm_rights_and_arcswap() {
        let sock_path = unique_socket_path("reg");
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let control = Arc::new(ControlPlane::new(sock_path.clone(), Arc::clone(&snapshot)));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let control_task = {
            let control = Arc::clone(&control);
            tokio::spawn(async move { control.run(shutdown_rx).await })
        };

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(sock_path.exists(), "control socket did not appear");

        let rig = build_test_vm(101, 4, 1, 4, 10);
        let sock_for_client = sock_path.clone();
        let req_clone = rig.req.clone();
        let shm_raw = rig.shm_fd.as_raw_fd();
        let kick_raws: Vec<RawFd> = rig.kick_fds.iter().map(AsRawFd::as_raw_fd).collect();
        let wake_raws: Vec<RawFd> = rig.wake_fds.iter().map(AsRawFd::as_raw_fd).collect();

        let resp = spawn_blocking(move || {
            send_registration(
                &sock_for_client,
                &req_clone,
                shm_raw,
                &kick_raws,
                &wake_raws,
            )
        })
        .await
        .unwrap()
        .unwrap();

        assert!(resp.accepted, "registration rejected: {}", resp.message);

        let published = snapshot.load();
        assert_eq!(published.len(), 1);
        let vm = &published[0];
        assert_eq!(vm.vm_id(), 101);
        assert_eq!(vm.num_vcpus(), 4);
        assert_eq!(vm.min_cores(), 1);
        assert_eq!(vm.max_cores(), 4);
        assert_eq!(vm.priority_weight(), 10);
        assert!(vm.shared_page().header.is_valid());
        assert_eq!(
            vm.host_page().vcpus[2].host_tid.load(Ordering::Acquire),
            101_003
        );

        // Verify SCM_RIGHTS kick_irqfds and wake_eventfds signal the client's original eventfds.
        vm.kick_revoke_irqfd(2).unwrap();
        assert_eq!(read_eventfd(rig.kick_fds[2].as_raw_fd()).unwrap(), 1);

        vm.wake_vcpu(3).unwrap();
        assert_eq!(read_eventfd(rig.wake_fds[3].as_raw_fd()).unwrap(), 1);

        // Verify live policy update over Plane 1 socket without SCM_RIGHTS.
        let update_req = VmPolicyUpdateRequest {
            vm_id: 101,
            min_cores: Some(2),
            max_cores: Some(3),
            priority_weight: Some(250),
        };
        let sock_for_update = sock_path.clone();
        let update_resp = spawn_blocking(move || send_policy_update(&sock_for_update, &update_req))
            .await
            .unwrap()
            .unwrap();

        assert!(
            update_resp.accepted,
            "policy update rejected: {}",
            update_resp.message
        );
        assert_eq!(update_resp.min_cores, Some(2));
        assert_eq!(update_resp.max_cores, Some(3));
        assert_eq!(update_resp.priority_weight, Some(250));

        assert_eq!(vm.min_cores(), 2);
        assert_eq!(vm.max_cores(), 3);
        assert_eq!(vm.priority_weight(), 250);

        let _ = shutdown_tx.send(true);
        let _ = control_task.await.unwrap();
    }

    #[test]
    fn test_plane2_phase1_zero_core_wakeup_and_stranded_work_tzcnt() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[2, 3]);

        let vm_timer = build_test_vm(1, 4, 0, 4, 1);
        let vm_rx = build_test_vm(2, 4, 0, 4, 1);

        // Set stranded_work_bitmap on vm_timer to bit 3 (vCPU 3) and bit 2 (vCPU 2) so tzcnt picks vCPU 2 first.
        vm_timer
            .handle
            .shared_page()
            .telemetry
            .stranded_work_bitmap
            .store((1 << 2) | (1 << 3), Ordering::Release);
        // Set next_timer_tsc to 5,000 while runnable_threads == 0 (0-core timer wakeup).
        vm_timer
            .handle
            .shared_page()
            .telemetry
            .next_timer_tsc
            .store(5_000, Ordering::Release);

        // Configure vm_rx for 0-core RX doorbell wakeup: rx_ring_cons_idx = 10, host_rx_doorbell = 11.
        vm_rx
            .handle
            .shared_page()
            .telemetry
            .rx_ring_cons_idx
            .store(10, Ordering::Release);
        vm_rx.handle.set_host_rx_doorbell(11);
        // Mark vCPU 1 in stranded_work_bitmap for vm_rx.
        vm_rx
            .handle
            .shared_page()
            .telemetry
            .stranded_work_bitmap
            .store(1 << 1, Ordering::Release);
        // vm_timer's guest names vm_rx's vCPU 1 thread in its own page; the pCPU still goes to
        // the thread the VMM recorded in the host page.
        vm_timer.handle.shared_page().vcpus[2]
            .host_tid
            .store(2002, Ordering::Release);

        ControlPlane::publish_vm_handle(&snapshot, vm_timer.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, vm_rx.handle.clone());

        // Poll at now_tsc = 6,000 (>= next_timer_tsc 5,000).
        engine.poll_once(6_000);

        // vm_timer should have woken vCPU 2 (lowest set bit in stranded_work_bitmap) on pCPU 2.
        assert_eq!(
            engine.core_state(2),
            CoreState::Assigned { vm_id: 1, vcpu: 2 }
        );
        // `pylond` doesn't write `state`; the vCPU stays PARKED until its thread wakes.
        assert_eq!(
            vm_timer.handle.host_page().vcpus[2]
                .state
                .load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(
            vm_timer.handle.host_page().vcpus[2]
                .assigned_pcpu
                .load(Ordering::Acquire),
            2
        );
        assert_eq!(pcpu_owners.read_owner(2).0, 1003);
        assert_eq!(read_eventfd(vm_timer.wake_fds[2].as_raw_fd()).unwrap(), 1);

        // vm_rx should have woken vCPU 1 (from stranded_work_bitmap) on pCPU 3.
        assert_eq!(
            engine.core_state(3),
            CoreState::Assigned { vm_id: 2, vcpu: 1 }
        );
        assert_eq!(
            vm_rx.handle.host_page().vcpus[1]
                .state
                .load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(pcpu_owners.read_owner(3).0, 2002);
        assert_eq!(read_eventfd(vm_rx.wake_fds[1].as_raw_fd()).unwrap(), 1);
    }

    /// A vCPU whose thread hasn't started (one past `boot_vcpus`, never hot-plugged) gets no
    /// core, however much demand its VM shows (spec 06 §7).
    #[test]
    fn test_plane2_vcpu_without_a_thread_gets_no_core() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(
            Arc::clone(&snapshot),
            Arc::clone(&pcpu_owners),
            &[2, 3, 4, 5],
        );

        let vm = build_test_vm(1, 4, 0, 4, 1);
        for vcpu in 2..4 {
            vm.handle.host_page().vcpus[vcpu]
                .host_tid
                .store(0, Ordering::Release);
        }
        ControlPlane::publish_vm_handle(&snapshot, vm.handle.clone());
        vm.handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(4, Ordering::Release);

        engine.poll_once(1_000);
        let assigned: Vec<u8> = [2, 3, 4, 5]
            .into_iter()
            .filter_map(|pcpu| match engine.core_state(pcpu) {
                CoreState::Assigned { vm_id: 1, vcpu } => Some(vcpu),
                _ => None,
            })
            .collect();
        assert_eq!(assigned, vec![0, 1]);
    }

    #[test]
    fn test_plane2_min_cores_is_a_floor_under_demand_only() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(
            Arc::clone(&snapshot),
            Arc::clone(&pcpu_owners),
            &[2, 3, 4, 5],
        );

        let vm = build_test_vm(1, 4, 2, 4, 1);
        let page = vm.handle.shared_page();
        page.telemetry
            .stranded_work_bitmap
            .store(0b1111, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, vm.handle.clone());

        // Idle: no runnable threads, no timer due, no packets. `min_cores` doesn't hold a core
        // for an idle VM, so it can scale to zero.
        engine.poll_once(6_000);
        let held = |engine: &MatrixEngine| {
            [2, 3, 4, 5]
                .iter()
                .filter(|&&pcpu| {
                    matches!(
                        engine.core_state(pcpu),
                        CoreState::Assigned { vm_id: 1, .. }
                    )
                })
                .count()
        };
        assert_eq!(held(&engine), 0);

        // One runnable thread: the VM gets `min_cores` cores, not one.
        page.telemetry.runnable_threads.store(1, Ordering::Release);
        engine.poll_once(7_000);
        assert_eq!(held(&engine), 2);
    }

    /// A zero-core wakeup may take a core from a donor at its `min_cores` at most once per
    /// `FLOOR_PREEMPT_INTERVAL_US`: its urgency comes from timers the guest controls.
    #[test]
    fn test_plane2_zero_core_wakeup_takes_a_floor_core_at_a_limited_rate() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[4]);

        // The donor holds its only core, which is also its `min_cores`.
        let donor = build_test_vm(10, 1, 1, 1, 1);
        let recipient = build_test_vm(20, 1, 0, 1, 1);
        donor
            .handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, donor.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, recipient.handle.clone());
        engine.poll_once(1_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 10, vcpu: 0 }
        );
        donor.handle.host_page().vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);

        // The recipient's guest says a timer is due, and it took a floor core just before.
        recipient
            .handle
            .shared_page()
            .telemetry
            .next_timer_tsc
            .store(1, Ordering::Release);
        recipient.handle.set_floor_preempt_tsc(1_500);
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 10, vcpu: 0 }
        );

        // Once the interval (10 ms, 30,000,000 cycles at 3 GHz) has passed, it may again.
        let later = 1_500 + 30_000_000;
        engine.poll_once(later);
        assert!(matches!(
            engine.core_state(4),
            CoreState::Revoking {
                donor_vm_id: 10,
                recipient_vm_id: 20,
                ..
            }
        ));
        assert_eq!(recipient.handle.floor_preempt_tsc(), later);
    }

    #[test]
    fn test_plane2_park_timer_wakes_zero_core_vm_on_due_vcpu() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[2]);

        // An idle VM whose vCPUs 1 and 3 parked with guest timers armed, published by the VMM
        // in host TSC: vCPU 3's is due first.
        let vm = build_test_vm(1, 4, 0, 4, 1);
        let host = vm.handle.host_page();
        host.vcpus[1]
            .park_timer_deadline_tsc
            .store(50_000, Ordering::Release);
        host.vcpus[3]
            .park_timer_deadline_tsc
            .store(5_000, Ordering::Release);
        // A deadline the guest writes into its own page counts for nothing.
        vm.handle.shared_page().vcpus[0]
            .park_timer_deadline_tsc
            .store(1, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, vm.handle.clone());

        // Nothing is due yet, and the guest has nothing to run.
        engine.poll_once(4_000);
        assert_eq!(engine.core_state(2), CoreState::Idle);

        // vCPU 3's timer is due: it gets a core so the interrupt can fire.
        engine.poll_once(6_000);
        assert_eq!(
            engine.core_state(2),
            CoreState::Assigned { vm_id: 1, vcpu: 3 }
        );
        assert_eq!(read_eventfd(vm.wake_fds[3].as_raw_fd()).unwrap(), 1);
    }

    /// A VM that parks inside KVM gets no zero-core wakeup for a due timer: KVM fires it, and
    /// the deadline recorded at the park stays due until the vCPU has a core. Waking the VM
    /// for it traded a core with a busy VM about ten times a second (spec 08 §4.4).
    #[test]
    fn test_plane2_park_timer_does_not_wake_vm_parked_in_kvm() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[2]);

        let mut vm = build_test_vm(1, 4, 0, 4, 1);
        vm.req.wake_by_signal = true;
        let handle = VmHandle::new(
            &vm.req,
            vm.shm_fd.try_clone().unwrap(),
            vm.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            vm.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap();
        let page = handle.shared_page();
        handle.host_page().vcpus[3]
            .park_timer_deadline_tsc
            .store(5_000, Ordering::Release);
        page.telemetry
            .next_timer_tsc
            .store(5_000, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());

        engine.poll_once(6_000);
        assert_eq!(engine.core_state(2), CoreState::Idle);

        // Demand still gets it a core, and the vCPU with the due timer goes first.
        page.telemetry.runnable_threads.store(1, Ordering::Release);
        engine.poll_once(7_000);
        assert_eq!(
            engine.core_state(2),
            CoreState::Assigned { vm_id: 1, vcpu: 3 }
        );
    }

    #[test]
    fn test_plane2_head_wait_uses_guest_tsc_offset() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[2]);

        // Two VMs each want one core and there is one. VM 1 has been waiting since host TSC
        // 900,000; VM 2 since host TSC 100, but its guest TSC runs 10,000,000 cycles ahead of
        // the host's, so it wrote 10,000,100. Read as host TSC, that lies in the future and
        // VM 2 would look like it had just started waiting.
        let vm1 = build_test_vm(1, 1, 0, 1, 1);
        let vm2 = build_test_vm(2, 1, 0, 1, 1);
        for (vm, head_wait, offset) in [(&vm1, 900_000u64, 0u64), (&vm2, 10_000_100, 10_000_000)] {
            let page = vm.handle.shared_page();
            page.vcpus[0]
                .guest_tsc_offset
                .store(offset, Ordering::Release);
            page.telemetry
                .head_wait_tsc
                .store(head_wait, Ordering::Release);
            page.telemetry.runnable_threads.store(1, Ordering::Release);
        }
        ControlPlane::publish_vm_handle(&snapshot, vm1.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, vm2.handle.clone());

        engine.poll_once(1_000_000);
        assert_eq!(
            engine.core_state(2),
            CoreState::Assigned { vm_id: 2, vcpu: 0 }
        );
    }

    #[test]
    fn test_plane2_phase2_and_phase3_cooperative_and_forced_preemption() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        // Manage 2 physical cores: pCPU 4 and pCPU 5.
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[4, 5]);

        let donor = build_test_vm(10, 2, 1, 2, 1);
        let recipient = build_test_vm(20, 2, 1, 2, 10);

        // Step 1: Donor wants 2 cores (`runnable_threads = 2`); Recipient is idle (`runnable_threads = 0`).
        donor
            .handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(2, Ordering::Release);
        donor
            .handle
            .shared_page()
            .telemetry
            .head_wait_tsc
            .store(900, Ordering::Release);

        ControlPlane::publish_vm_handle(&snapshot, donor.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, recipient.handle.clone());

        // Tick 1 (now_tsc = 1,000): Fast-path assigns both idle cores (pCPU 4 and pCPU 5) to Donor.
        engine.poll_once(1_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 10, vcpu: 0 }
        );
        assert_eq!(
            engine.core_state(5),
            CoreState::Assigned { vm_id: 10, vcpu: 1 }
        );
        assert_eq!(pcpu_owners.read_owner(4).0, 10_001);
        assert_eq!(pcpu_owners.read_owner(5).0, 10_002);

        // Simulate Donor vCPU 0 and vCPU 1 entering VCPU_STATE_RUNNING.
        // Mark Donor vCPU 0 in a critical section (`in_critical_section = 1`) and vCPU 1 not in critical section (`0`).
        let donor_page = donor.handle.shared_page();
        let donor_host = donor.handle.host_page();
        donor_host.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        donor_page.vcpus[0]
            .in_critical_section
            .store(1, Ordering::Release);
        donor_host.vcpus[1]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        donor_page.vcpus[1]
            .in_critical_section
            .store(0, Ordering::Release);
        // Donor's head_wait_tsc is cleared once its threads are running.
        donor_page
            .telemetry
            .head_wait_tsc
            .store(0, Ordering::Release);

        // Step 2: Recipient becomes hungry for 1 core with high urgency.
        let recipient_page = recipient.handle.shared_page();
        let recipient_host = recipient.handle.host_page();
        recipient_page
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        recipient_page
            .telemetry
            .head_wait_tsc
            .store(1_500, Ordering::Release);

        // Tick 2 (now_tsc = 2,000): Phase 3 must select Donor vCPU 1 on pCPU 5 (since vCPU 0 has in_critical_section == 1).
        // 8 us at 3,000,000 kHz = 24,000 cycles -> deadline = 2,000 + 24,000 = 26,000.
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 10, vcpu: 0 }
        );
        assert_eq!(
            engine.core_state(5),
            CoreState::Revoking {
                donor_vm_id: 10,
                donor_vcpu: 1,
                recipient_vm_id: 20,
                recipient_vcpu: 0,
                deadline_tsc: 26_000,
            }
        );
        assert_eq!(
            donor_host.vcpus[1].revoke_requested.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            donor_host.vcpus[1]
                .revoke_deadline_tsc
                .load(Ordering::Acquire),
            26_000
        );
        assert_eq!(read_eventfd(donor.kick_fds[1].as_raw_fd()).unwrap(), 1);

        // Step 3: Cooperative handoff before deadline (now_tsc = 5,000 < 26,000).
        // Donor vCPU 1 parks cooperatively; the VMM bumps `yield_count` before `PARKED`.
        donor_host.vcpus[1]
            .yield_count
            .fetch_add(1, Ordering::Relaxed);
        donor_host.vcpus[1]
            .state
            .store(VCPU_STATE_PARKED, Ordering::Release);
        engine.poll_once(5_000);

        assert_eq!(
            engine.core_state(5),
            CoreState::Assigned { vm_id: 20, vcpu: 0 }
        );
        assert_eq!(donor.handle.cooperative_yield_count(), 1);
        assert_eq!(donor.handle.forced_preempt_count(), 0);
        // 3,000 cycles at 3 GHz from the request (2,000) to the park (5,000) is 1 us.
        assert_eq!(donor.handle.park_latency().count(), 1);
        assert_eq!(donor.handle.park_latency().sum_us(), 1);
        assert_eq!(
            donor_host.vcpus[1].assigned_pcpu.load(Ordering::Acquire),
            PCPU_UNASSIGNED
        );
        assert_eq!(pcpu_owners.read_owner(5).0, 20_001);
        assert_eq!(
            recipient_host.vcpus[0]
                .assigned_pcpu
                .load(Ordering::Acquire),
            5
        );
        assert_eq!(
            recipient_host.vcpus[0].state.load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(read_eventfd(recipient.wake_fds[0].as_raw_fd()).unwrap(), 1);

        // The recipient hasn't run yet: no wake or handoff latency is recorded.
        assert_eq!(recipient.handle.wake_latency().count(), 0);
        assert_eq!(recipient.handle.handoff_latency().count(), 0);
        // Its thread takes the core: the next sweep records 2 us since the handover (5,000)
        // and 3 us since the request (2,000), once each.
        recipient_host.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        engine.poll_once(11_000);
        engine.poll_once(12_000);
        assert_eq!(recipient.handle.wake_latency().count(), 1);
        assert_eq!(recipient.handle.wake_latency().sum_us(), 2);
        assert_eq!(recipient.handle.handoff_latency().count(), 1);
        assert_eq!(recipient.handle.handoff_latency().sum_us(), 3);
        recipient_host.vcpus[0]
            .state
            .store(VCPU_STATE_PARKED, Ordering::Release);

        // Step 4: Test Forced Preemption on deadline expiry!
        // Suppose Donor's min_cores is lowered or Donor drops runnable_threads to 0 so Donor vCPU 0 is over-allocated,
        // and Recipient wants a 2nd core (`runnable_threads = 2`).
        donor_page
            .telemetry
            .runnable_threads
            .store(0, Ordering::Release);
        recipient_host.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        recipient_page
            .telemetry
            .runnable_threads
            .store(2, Ordering::Release);
        recipient_page
            .telemetry
            .head_wait_tsc
            .store(5_500, Ordering::Release);

        // Manually set donor's min_cores scenario by testing a Revoking state on pCPU 4 where Donor vCPU 0 ignores revocation.
        engine.set_core_state(
            4,
            CoreState::Revoking {
                donor_vm_id: 10,
                donor_vcpu: 0,
                recipient_vm_id: 20,
                recipient_vcpu: 1,
                deadline_tsc: 30_000,
            },
        );
        donor_host.vcpus[0]
            .revoke_requested
            .store(1, Ordering::Release);

        // Poll before deadline (now_tsc = 29,999): Donor vCPU 0 is still RUNNING, so revocation stays in-flight.
        engine.poll_once(29_999);
        assert_eq!(
            engine.core_state(4),
            CoreState::Revoking {
                donor_vm_id: 10,
                donor_vcpu: 0,
                recipient_vm_id: 20,
                recipient_vcpu: 1,
                deadline_tsc: 30_000,
            }
        );

        // Poll at the deadline (now_tsc = 30,000): the revocation is forced, but the core stays
        // with the donor until the VMM has parked it. `pylond` leaves the donor's slot alone.
        engine.poll_once(30_000);
        assert!(matches!(engine.core_state(4), CoreState::Revoking { .. }));
        assert_eq!(donor.handle.forced_preempt_count(), 1);
        assert_eq!(
            donor_host.vcpus[0].state.load(Ordering::Acquire),
            VCPU_STATE_RUNNING
        );
        assert_eq!(
            donor_host.vcpus[0].revoke_requested.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            donor_page.vcpus[0]
                .forced_preempt_count
                .load(Ordering::Acquire),
            0
        );

        // The VMM force-parks the donor at its next exit; the core then moves.
        donor_host.vcpus[0]
            .revoke_requested
            .store(0, Ordering::Release);
        donor_host.vcpus[0]
            .yield_count
            .fetch_add(1, Ordering::Relaxed);
        donor_host.vcpus[0]
            .state
            .store(VCPU_STATE_PARKED, Ordering::Release);
        engine.poll_once(40_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 20, vcpu: 1 }
        );
        assert_eq!(donor.handle.forced_preempt_count(), 1);
        assert_eq!(donor.handle.cooperative_yield_count(), 1);
        assert_eq!(donor.handle.handoff_without_park_count(), 0);
        assert_eq!(pcpu_owners.read_owner(4).0, 20_002);
        assert_eq!(
            recipient_host.vcpus[1]
                .assigned_pcpu
                .load(Ordering::Acquire),
            4
        );
        assert_eq!(
            recipient_host.vcpus[1].state.load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(read_eventfd(recipient.wake_fds[1].as_raw_fd()).unwrap(), 1);
    }

    /// Spec 05 K7: a vCPU that was just given a core stays `PARKED` until its thread wakes. It
    /// must not be revoked meanwhile, and a `PARKED` donor counts as parked only once its
    /// `yield_count` has moved since the revocation began.
    #[test]
    fn test_plane2_waking_vcpu_is_not_taken_for_a_parked_donor() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[7]);

        let donor = build_test_vm(60, 1, 0, 1, 1);
        let recipient = build_test_vm(70, 1, 0, 1, 100);
        let donor_slot = &donor.handle.host_page().vcpus[0];
        let recipient_page = recipient.handle.shared_page();
        donor
            .handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, donor.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, recipient.handle.clone());

        engine.poll_once(1_000);
        assert_eq!(
            engine.core_state(7),
            CoreState::Assigned { vm_id: 60, vcpu: 0 }
        );

        // The recipient becomes hungry before the donor's thread has woken.
        recipient_page
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        recipient_page
            .telemetry
            .head_wait_tsc
            .store(1_500, Ordering::Release);
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(7),
            CoreState::Assigned { vm_id: 60, vcpu: 0 }
        );
        assert_eq!(donor_slot.revoke_requested.load(Ordering::Acquire), 0);

        // Once it runs, it can be revoked.
        donor_slot
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        engine.poll_once(3_000);
        assert!(matches!(engine.core_state(7), CoreState::Revoking { .. }));
        assert_eq!(donor_slot.revoke_requested.load(Ordering::Acquire), 1);

        // `PARKED` with the count from before the revocation isn't a park for it.
        donor_slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
        engine.poll_once(4_000);
        assert!(matches!(engine.core_state(7), CoreState::Revoking { .. }));
        assert_eq!(donor.handle.cooperative_yield_count(), 0);

        // The VMM's park bumps `yield_count` before publishing `PARKED`.
        donor_slot.revoke_requested.store(0, Ordering::Release);
        donor_slot.yield_count.fetch_add(1, Ordering::Relaxed);
        engine.poll_once(5_000);
        assert_eq!(
            engine.core_state(7),
            CoreState::Assigned { vm_id: 70, vcpu: 0 }
        );
        assert_eq!(donor.handle.cooperative_yield_count(), 1);
        assert_eq!(
            donor_slot.assigned_pcpu.load(Ordering::Acquire),
            PCPU_UNASSIGNED
        );
    }

    /// Two VMs with 2 cores each both want 4. Returns how many of the 4 cores the engine starts
    /// revoking when VM 80, whose backlog is older, has weight `weight_a` and VM 90 `weight_b`.
    fn revocations_between_two_hungry_vms(weight_a: u32, weight_b: u32) -> usize {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let cores = [20, 21, 22, 23];
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &cores);
        let a = build_test_vm(80, 4, 1, 4, weight_a);
        let b = build_test_vm(90, 4, 1, 4, weight_b);
        ControlPlane::publish_vm_handle(&snapshot, a.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, b.handle.clone());

        for (rig, now) in [(&a, 1_000), (&b, 2_000)] {
            let page = rig.handle.shared_page();
            let host = rig.handle.host_page();
            page.telemetry.runnable_threads.store(2, Ordering::Release);
            engine.poll_once(now);
            for slot in &host.vcpus[..2] {
                slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
            }
            page.telemetry.runnable_threads.store(0, Ordering::Release);
        }
        for rig in [&a, &b] {
            assert_eq!(
                rig.handle.host_page().vcpus[..2]
                    .iter()
                    .filter(|slot| slot.assigned_pcpu.load(Ordering::Acquire) != PCPU_UNASSIGNED)
                    .count(),
                2
            );
        }

        for (rig, head_wait) in [(&a, 1_000), (&b, 2_500)] {
            let telemetry = &rig.handle.shared_page().telemetry;
            telemetry.runnable_threads.store(4, Ordering::Release);
            telemetry.head_wait_tsc.store(head_wait, Ordering::Release);
        }
        engine.poll_once(3_000);
        cores
            .iter()
            .filter(|&&pcpu| matches!(engine.core_state(pcpu), CoreState::Revoking { .. }))
            .count()
    }

    /// Spec 05 K7: with equal weights, an even split is final. The more urgent VM used to take
    /// a core, the other took it back as over its fair share, and so on every sweep.
    #[test]
    fn test_plane2_urgency_does_not_take_cores_below_the_weighted_share() {
        assert_eq!(revocations_between_two_hungry_vms(100, 100), 0);
        // Twice the weight is worth a third core.
        assert_eq!(revocations_between_two_hungry_vms(200, 100), 1);
    }

    /// Clones `fds`, for a second `VmHandle::new` on a rig's file descriptors.
    fn clone_fds(fds: &[OwnedFd]) -> Vec<OwnedFd> {
        fds.iter().map(|fd| fd.try_clone().unwrap()).collect()
    }

    #[test]
    fn test_registration_rejects_a_shared_memory_file_that_can_change_size() {
        let rig = build_test_vm(70, 1, 0, 1, 1);
        // The same page in a memfd whose size isn't sealed: it could be truncated later.
        let name = CString::new("pylon-test-unsealed").unwrap();
        // SAFETY: `name` is a valid NUL-terminated string, and the flags are valid.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a new file descriptor that nothing else owns.
        let unsealed = unsafe { File::from_raw_fd(fd) };
        unsealed.set_len(PYLON_SHM_FILE_SIZE as u64).unwrap();
        let Err(err) = VmHandle::new(
            &rig.req,
            OwnedFd::from(unsealed),
            clone_fds(&rig.kick_fds),
            clone_fds(&rig.wake_fds),
        ) else {
            panic!("an unsealed file was accepted");
        };
        assert!(matches!(err, Error::InvalidRegistration { vm_id: 70, .. }));
    }

    #[test]
    fn test_registration_rejects_a_wake_fd_that_is_not_an_eventfd() {
        let rig = build_test_vm(71, 1, 0, 1, 1);
        let mut pipe = [0; 2];
        // SAFETY: `pipe2` writes two file descriptors into `pipe`.
        let ret = unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(ret, 0);
        // SAFETY: `pipe2` returned two new file descriptors that nothing else owns.
        let (read_end, write_end) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        drop(read_end);
        let Err(err) = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            clone_fds(&rig.kick_fds),
            vec![write_end],
        ) else {
            panic!("a pipe was accepted as a wake fd");
        };
        assert!(matches!(err, Error::InvalidRegistration { vm_id: 71, .. }));
    }

    /// A registrant that makes its wake `eventfd` blocking and fills its counter can't stop
    /// the engine: the watchdog interrupts the write, the VM is marked faulty and Plane 1
    /// drops it.
    #[test]
    fn test_write_watch_interrupts_a_write_to_a_full_eventfd() {
        let rig = build_test_vm(72, 1, 0, 1, 1);
        // SAFETY: `eventfd` has no memory safety preconditions.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: `raw` is a new file descriptor that nothing else owns.
        let blocking = unsafe { OwnedFd::from_raw_fd(raw) };
        let vm = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            clone_fds(&rig.kick_fds),
            vec![blocking.try_clone().unwrap()],
        )
        .unwrap();
        // One below the maximum: any further write blocks.
        let full: u64 = u64::MAX - 1;
        // SAFETY: `blocking` is a valid eventfd and `full` an 8-byte buffer.
        let written = unsafe {
            libc::write(
                blocking.as_raw_fd(),
                (&raw const full).cast::<libc::c_void>(),
                size_of::<u64>(),
            )
        };
        assert_eq!(written, 8);

        WriteWatch::install_interrupt_handler(libc::SIGUSR1).unwrap();
        let watch = Arc::new(WriteWatch::new(libc::SIGUSR1));
        watch.bind_current_thread();
        let done = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let watch = Arc::clone(&watch);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    thread::sleep(BLOCKED_WRITE_LIMIT);
                    watch.interrupt_blocked(BLOCKED_WRITE_LIMIT);
                }
            })
        };

        let start = Instant::now();
        watch.write(&vm, || vm.wake_vcpu(0)).unwrap();
        done.store(true, Ordering::Relaxed);
        watchdog.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(vm.is_faulty());
        // No further writes, so nothing blocks again.
        vm.wake_vcpu(0).unwrap();

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        ControlPlane::publish_vm_handle(&snapshot, vm);
        assert_eq!(ControlPlane::remove_exited_vms(&snapshot), 1);
        assert!(snapshot.load().is_empty());
    }

    #[test]
    fn test_only_root_pylond_and_the_registering_uid_manage_a_vm() {
        // SAFETY: `geteuid` takes no arguments and can't fail.
        let own_uid = unsafe { libc::geteuid() };
        let owner = own_uid.wrapping_add(1000).max(1);
        let stranger = own_uid.wrapping_add(2000).max(1);

        let rig = build_test_vm(60, 1, 0, 1, 1);
        // Without a known owner, only root and `pylond`'s own UID manage the VM.
        assert!(rig.handle.is_managed_by(Some(0)));
        assert!(rig.handle.is_managed_by(Some(own_uid)));
        assert!(!rig.handle.is_managed_by(Some(owner)));
        assert!(!rig.handle.is_managed_by(None));

        let rig = build_test_vm(61, 1, 0, 1, 1);
        let handle = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            rig.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            rig.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
        .with_owner_uid(owner)
        .unwrap();
        assert_eq!(handle.owner_uid(), Some(owner));
        assert!(handle.is_managed_by(Some(owner)));
        assert!(handle.is_managed_by(Some(0)));
        assert!(!handle.is_managed_by(Some(stranger)));
        assert!(!handle.is_managed_by(None));
    }

    #[test]
    fn test_vcpu_threads_lists_started_vcpus_with_the_vmm_pid_and_slot() {
        let rig = build_test_vm(50, 3, 0, 1, 1);
        // Only vCPU 1's thread has started.
        for (idx, slot) in rig.handle.host_page().vcpus[..3].iter().enumerate() {
            let tid = if idx == 1 { 4242 } else { 0 };
            slot.host_tid.store(tid, Ordering::Release);
        }
        // A thread ID the guest writes into its own page isn't listed.
        rig.handle.shared_page().vcpus[0]
            .host_tid
            .store(1, Ordering::Release);
        // Without a PID, the threads can't be tied to a process.
        assert_eq!(rig.handle.vcpu_threads().count(), 0);

        let handle = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            rig.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            rig.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
        .with_vmm_pid(process_id() as i32)
        .unwrap();
        handle.set_vm_slot(7);
        assert_eq!(
            handle.vcpu_threads().collect::<Vec<_>>(),
            [(
                4242,
                VcpuThread {
                    pid: process_id(),
                    vm_slot: 7,
                    vcpu: 1
                }
            )]
        );
    }

    static KICK_SIGNALS_SEEN: AtomicU64 = AtomicU64::new(0);

    extern "C" fn count_kick_signal(_: libc::c_int) {
        KICK_SIGNALS_SEEN.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_for_kick_signals(at_least: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while KICK_SIGNALS_SEEN.load(Ordering::SeqCst) < at_least {
            assert!(Instant::now() < deadline, "vCPU thread wasn't signalled");
            thread::sleep(Duration::from_millis(1));
        }
    }

    static BOOT_KICK_SIGNALS_SEEN: AtomicU64 = AtomicU64::new(0);

    extern "C" fn count_boot_kick_signal(_: libc::c_int) {
        BOOT_KICK_SIGNALS_SEEN.fetch_add(1, Ordering::SeqCst);
    }

    /// Once the guest is up (a heartbeat), `pylond` asks the VMM to park exactly the vCPUs it
    /// tagged `park_on_boot` that are still running, and signals their threads; it asks again
    /// until the VMM has parked them, and never touches the others.
    #[test]
    fn test_park_booted_vcpus_asks_tagged_running_vcpus_after_the_heartbeat() {
        let kick_signal = libc::SIGRTMIN() + 4;
        // SAFETY: installs a handler that only increments an atomic, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = zeroed();
            action.sa_sigaction = count_boot_kick_signal as *const () as usize;
            assert_eq!(libc::sigaction(kick_signal, &action, ptr::null_mut()), 0);
        }

        let mut rig = build_test_vm(50, 3, 0, 3, 1);
        rig.req.kick_signal = Some(kick_signal);
        let handle = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            rig.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            rig.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
        .with_vmm_pid(process_id() as i32)
        .unwrap();

        // vCPU threads: threads of this process that just sleep.
        let stop = Arc::new(AtomicBool::new(false));
        let tids = Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]);
        let threads: Vec<_> = (0..3)
            .map(|i| {
                let (stop, tids) = (Arc::clone(&stop), Arc::clone(&tids));
                thread::spawn(move || {
                    // SAFETY: `gettid` has no preconditions.
                    tids[i].store(unsafe { libc::gettid() } as u64, Ordering::SeqCst);
                    while !stop.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(1));
                    }
                })
            })
            .collect();
        while tids.iter().any(|t| t.load(Ordering::SeqCst) == 0) {
            thread::yield_now();
        }
        let page = handle.shared_page();
        let host = handle.host_page();
        for (i, (slot, host_slot)) in page.vcpus[..3].iter().zip(&host.vcpus).enumerate() {
            let tid = tids[i].load(Ordering::SeqCst) as u32;
            host_slot.host_tid.store(tid, Ordering::Release);
            slot.host_tid.store(tid, Ordering::Release);
            host_slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
        }
        // vCPU 0 is an initial vCPU (no tag); 1 is tagged and running; 2 is tagged, already parked.
        host.vcpus[1].park_on_boot.store(1, Ordering::Release);
        host.vcpus[2].park_on_boot.store(1, Ordering::Release);
        host.vcpus[2]
            .state
            .store(VCPU_STATE_PARKED, Ordering::Release);

        // The guest hasn't reported yet: leave everything alone.
        assert_eq!(handle.park_booted_vcpus(), 0);
        assert_eq!(host.vcpus[1].revoke_requested.load(Ordering::Acquire), 0);

        page.telemetry.guest_heartbeat.store(1, Ordering::Release);
        assert_eq!(handle.park_booted_vcpus(), 1);
        for (i, expected) in [(0, 0), (1, 1), (2, 0)] {
            assert_eq!(
                host.vcpus[i].revoke_requested.load(Ordering::Acquire),
                expected,
                "vCPU {i}"
            );
        }
        assert_eq!(host.vcpus[1].revoke_deadline_tsc.load(Ordering::Acquire), 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while BOOT_KICK_SIGNALS_SEEN.load(Ordering::SeqCst) < 1 {
            assert!(Instant::now() < deadline, "vCPU thread wasn't signalled");
            thread::sleep(Duration::from_millis(1));
        }

        // A signal can be lost: while the VMM hasn't parked it, the next call asks again.
        assert_eq!(handle.park_booted_vcpus(), 1);
        // The VMM parked it and cleared the tag: nothing left to ask.
        host.vcpus[1].park_on_boot.store(0, Ordering::Release);
        host.vcpus[1].revoke_requested.store(0, Ordering::Release);
        assert_eq!(handle.park_booted_vcpus(), 0);

        stop.store(true, Ordering::SeqCst);
        for t in threads {
            t.join().unwrap();
        }
    }

    /// The engine publishes the parked, unassigned vCPUs of VMs that park inside KVM, and
    /// marks the cores it gives them with the VM's slot (spec 08 §4.3).
    #[test]
    fn test_plane2_publishes_parked_vcpus_and_owner_slots() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        // VM 70 parks inside KVM; VM 71 parks in the VMM. All their vCPUs start PARKED.
        let mut kvm = build_test_vm(70, 3, 0, 3, 1);
        kvm.req.wake_by_signal = true;
        let kvm_handle = VmHandle::new(
            &kvm.req,
            kvm.shm_fd.try_clone().unwrap(),
            kvm.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            kvm.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap();
        let vmm = build_test_vm(71, 2, 0, 2, 1);
        ControlPlane::publish_vm_handle(&snapshot, kvm_handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, vmm.handle.clone());
        let (kvm_slot, vmm_slot) = (kvm_handle.vm_slot(), vmm.handle.vm_slot());
        let parked = |slot: u16| vm_vcpus.entry(slot).unwrap().parked.load(Ordering::Acquire);
        kvm_handle.host_page().vcpus[1]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);

        engine.poll_once(1_000);
        assert_eq!(parked(kvm_slot), 0b101);
        assert_eq!(parked(vmm_slot), 0);

        // A core for VM 70's vCPU 0: its bit clears, and the owner entry names the slot.
        kvm_handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(6),
            CoreState::Assigned { vm_id: 70, vcpu: 0 }
        );
        assert_eq!(
            pcpu_owners.read_owner(6).1 >> scx_pylon::PCPU_OWNER_SLOT_SHIFT,
            u32::from(kvm_slot) + 1
        );
        assert_eq!(parked(kvm_slot), 0b100);

        // VM 70 leaves: its mask is cleared.
        snapshot.store(Arc::new(vec![vmm.handle.clone()]));
        engine.poll_once(3_000);
        assert_eq!(parked(kvm_slot), 0);
    }

    /// A handle for `rig`'s VM that registered with `wake_by_signal`: it parks inside KVM.
    fn parks_in_kvm(rig: &mut TestVmRig) -> VmHandle {
        rig.req.wake_by_signal = true;
        VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            rig.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            rig.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
    }

    /// A VM with no core whose parked vCPU KVM woke gets a core once that wake has lasted
    /// `WOKEN_DEMAND_US`; a short wake runs on the shared vCPU queue. The core goes to the vCPU
    /// in `stranded_work_bitmap`: its tasks can't run until it has one, while the woken vCPU
    /// already runs (spec 08 §4.2).
    #[test]
    fn test_plane2_long_woken_vcpu_of_zero_core_vm_gets_a_core() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        let mut rig = build_test_vm(80, 4, 0, 4, 1);
        let handle = parks_in_kvm(&mut rig);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());
        let entry = vm_vcpus.entry(handle.vm_slot()).unwrap();
        let page = handle.shared_page();
        let host = handle.host_page();
        page.telemetry
            .stranded_work_bitmap
            .store(1 << 1, Ordering::Release);

        // Nothing is due and nothing woke: no core.
        engine.poll_once(1_000);
        assert_eq!(engine.core_state(6), CoreState::Idle);

        // A short wake isn't enough. At 3 GHz, 4 ms is 12,000,000 cycles.
        entry.woken.store(1 << 2, Ordering::Release);
        engine.poll_once(2_000);
        engine.poll_once(6_002_000);
        assert_eq!(engine.core_state(6), CoreState::Idle);

        engine.poll_once(12_002_000);
        assert_eq!(
            engine.core_state(6),
            CoreState::Assigned { vm_id: 80, vcpu: 1 }
        );
        assert_eq!(host.vcpus[1].assigned_pcpu.load(Ordering::Acquire), 6);
        assert_eq!(
            page.telemetry.stranded_work_bitmap.load(Ordering::Acquire),
            0
        );
    }

    /// A core taken for a woken vCPU of a VM with no core stays with it until the guest agent
    /// reports again (`agent_reports`), or for `WOKEN_GRANT_HOLD_US` without a report. Once it
    /// has the core, the vCPU no longer counts as woken, and the guest reports no runnable
    /// threads before that. Before, the donor took the core back as over the VM's demand in
    /// the next sweep, and the two traded it every `WOKEN_DEMAND_US` (spec 08 §4.2). Returns
    /// whether the donor starts taking the core back once the hold ends.
    fn woken_grant_hold(agent_reports: bool) -> bool {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6, 7])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        // A busy donor that always wants both cores.
        let donor = build_test_vm(60, 2, 0, 2, 1);
        let donor_page = donor.handle.shared_page();
        let donor_host = donor.handle.host_page();
        donor_page
            .telemetry
            .runnable_threads
            .store(2, Ordering::Release);
        let mut rig = build_test_vm(80, 4, 0, 4, 1);
        let handle = parks_in_kvm(&mut rig);
        let page = handle.shared_page();
        let host = handle.host_page();
        ControlPlane::publish_vm_handle(&snapshot, donor.handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());
        let entry = vm_vcpus.entry(handle.vm_slot()).unwrap();

        engine.poll_once(1_000);
        for slot in &donor_host.vcpus[..2] {
            assert_ne!(slot.assigned_pcpu.load(Ordering::Acquire), PCPU_UNASSIGNED);
            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
        }

        // vCPU 2 of VM 80 stays woken for 4 ms (12,000,000 cycles at 3 GHz), so VM 80 takes
        // a core, and the donor vCPU on it parks.
        entry.woken.store(1 << 2, Ordering::Release);
        engine.poll_once(2_000);
        engine.poll_once(12_002_000);
        let (pcpu, donor_vcpu) = [6, 7]
            .into_iter()
            .find_map(|pcpu| match engine.core_state(pcpu) {
                CoreState::Revoking {
                    donor_vcpu,
                    recipient_vm_id: 80,
                    recipient_vcpu: 2,
                    ..
                } => Some((pcpu, donor_vcpu)),
                _ => None,
            })
            .expect("VM 80 takes a core");
        let donor_slot = &donor_host.vcpus[donor_vcpu as usize];
        donor_slot.revoke_requested.store(0, Ordering::Release);
        donor_slot.yield_count.fetch_add(1, Ordering::Relaxed);
        donor_slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
        engine.poll_once(12_003_000);
        assert_eq!(
            engine.core_state(pcpu),
            CoreState::Assigned { vm_id: 80, vcpu: 2 }
        );

        // vCPU 2 runs on its core, so it no longer counts as woken, and the guest reports no
        // runnable threads. Until the agent reports again, the donor, which still wants both
        // cores, doesn't take it back. 50 ms is 150,000,000 cycles.
        host.vcpus[2]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        entry.woken.store(0, Ordering::Release);
        engine.poll_once(12_004_000);
        engine.poll_once(162_001_000);
        assert_eq!(
            engine.core_state(pcpu),
            CoreState::Assigned { vm_id: 80, vcpu: 2 }
        );
        assert_eq!(donor.handle.forced_preempt_count(), 0);

        let now = if agent_reports {
            page.telemetry
                .guest_heartbeat
                .fetch_add(1, Ordering::Release);
            162_001_500
        } else {
            162_002_000
        };
        engine.poll_once(now);
        matches!(
            engine.core_state(pcpu),
            CoreState::Revoking {
                donor_vm_id: 80,
                recipient_vm_id: 60,
                ..
            }
        )
    }

    #[test]
    fn test_plane2_core_for_a_woken_vcpu_stays_until_the_agent_reports() {
        assert!(woken_grant_hold(true));
    }

    #[test]
    fn test_plane2_core_for_a_woken_vcpu_stays_at_most_the_hold() {
        assert!(woken_grant_hold(false));
    }

    /// A woken vCPU of a VM that owns a core takes an idle core at once, however short its wake
    /// (#52): sharing would make it wait for turns on every wake.
    #[test]
    fn test_plane2_woken_vcpu_takes_an_idle_core_at_once() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6, 7])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        // The guest reports no runnable threads after the first core, so only wakes show
        // demand.
        let mut rig = build_test_vm(81, 4, 1, 4, 1);
        let handle = parks_in_kvm(&mut rig);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());
        let entry = vm_vcpus.entry(handle.vm_slot()).unwrap();
        let page = handle.shared_page();
        let host = handle.host_page();

        // vCPU 0 gets a core for one runnable thread, and runs.
        page.telemetry.runnable_threads.store(1, Ordering::Release);
        engine.poll_once(1_000);
        assert_eq!(
            engine.core_state(6),
            CoreState::Assigned { vm_id: 81, vcpu: 0 }
        );
        host.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        page.telemetry.runnable_threads.store(0, Ordering::Release);

        // vCPU 3 wakes, and gets the idle core in the same sweep.
        entry.woken.store(1 << 3, Ordering::Release);
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(7),
            CoreState::Assigned { vm_id: 81, vcpu: 3 }
        );

        // Once it has a core, its bit no longer counts.
        engine.poll_once(3_002_000);
        assert_eq!(handle.woken_shared_usec(), 0);
        assert_eq!(handle.woken_unowned_usec(), 0);
    }

    /// With no idle core, a woken vCPU shares its VM's core until it has been woken for
    /// `WOKEN_DEMAND_US`: a short wake takes no core from another VM. Its time is metered as
    /// shared (spec 08 §4.2).
    #[test]
    fn test_plane2_short_woken_vcpu_takes_no_core_from_another_vm() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6, 7])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        let mut rig = build_test_vm(81, 4, 1, 4, 1);
        let handle = parks_in_kvm(&mut rig);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());
        let mut other_rig = build_test_vm(82, 1, 1, 1, 1);
        let other = parks_in_kvm(&mut other_rig);
        ControlPlane::publish_vm_handle(&snapshot, other.clone());
        let entry = vm_vcpus.entry(handle.vm_slot()).unwrap();

        // Each VM gets a core for one runnable thread, and runs.
        handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        other
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        engine.poll_once(1_000);
        let core_of = |engine: &MatrixEngine, vm_id| {
            [6, 7].into_iter().find(|&pcpu| {
                matches!(engine.core_state(pcpu), CoreState::Assigned { vm_id: v, .. } if v == vm_id)
            })
        };
        let other_core = core_of(&engine, 82).expect("VM 82 has a core");
        assert!(core_of(&engine, 81).is_some());
        handle.host_page().vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        other.host_page().vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);

        // vCPU 3 of VM 81 wakes. At 3 GHz, 4 ms is 12,000,000 cycles.
        entry.woken.store(1 << 3, Ordering::Release);
        engine.poll_once(2_000);
        engine.poll_once(6_002_000);
        assert_eq!(
            engine.core_state(other_core),
            CoreState::Assigned { vm_id: 82, vcpu: 0 }
        );
        assert_eq!(handle.woken_shared_usec(), 2_000);
        assert_eq!(handle.woken_unowned_usec(), 0);
    }

    /// A woken vCPU takes a core from a VM that holds it above its floor and wants no core (#52).
    #[test]
    fn test_plane2_woken_vcpu_takes_a_core_another_vm_no_longer_wants() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let vm_vcpus = Arc::new(VmVcpusTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6, 7])
            .with_vm_vcpus(Arc::clone(&vm_vcpus));

        let mut rig = build_test_vm(81, 4, 1, 4, 1);
        let handle = parks_in_kvm(&mut rig);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());
        let mut other_rig = build_test_vm(82, 1, 0, 1, 1);
        let other = parks_in_kvm(&mut other_rig);
        ControlPlane::publish_vm_handle(&snapshot, other.clone());
        let entry = vm_vcpus.entry(handle.vm_slot()).unwrap();

        handle
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        other
            .shared_page()
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        engine.poll_once(1_000);
        let other_core = [6, 7]
            .into_iter()
            .find(|&pcpu| engine.core_state(pcpu) == CoreState::Assigned { vm_id: 82, vcpu: 0 })
            .expect("VM 82 has a core");
        handle.host_page().vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        other.host_page().vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);

        // VM 82 goes idle but keeps its core, then vCPU 3 of VM 81 wakes.
        other
            .shared_page()
            .telemetry
            .runnable_threads
            .store(0, Ordering::Release);
        entry.woken.store(1 << 3, Ordering::Release);
        // Not before VM 82 has held the core for `SPARE_CORE_TENURE_US`.
        engine.poll_once(2_000);
        assert_eq!(
            engine.core_state(other_core),
            CoreState::Assigned { vm_id: 82, vcpu: 0 }
        );
        let tenure = SPARE_CORE_TENURE_US * TEST_TSC_KHZ / 1000;
        engine.poll_once(1_000 + tenure);
        assert!(matches!(
            engine.core_state(other_core),
            CoreState::Revoking {
                donor_vm_id: 82,
                recipient_vm_id: 81,
                recipient_vcpu: 3,
                ..
            }
        ));
    }

    static WAKE_SIGNALS_SEEN: AtomicU64 = AtomicU64::new(0);

    extern "C" fn count_wake_signal(_: libc::c_int) {
        WAKE_SIGNALS_SEEN.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_for_wake_signals(at_least: u64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while WAKE_SIGNALS_SEEN.load(Ordering::SeqCst) < at_least {
            assert!(Instant::now() < deadline, "vCPU thread wasn't signalled");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A VM that parks vCPUs inside KVM gets a signal with each assignment, and another every
    /// `REVOKE_RESIGNAL_US` until the vCPU leaves `PARKED`.
    #[test]
    fn test_plane2_signals_a_recipient_parked_in_kvm_until_it_runs() {
        let kick_signal = libc::SIGRTMIN() + 5;
        // SAFETY: installs a handler that only increments an atomic, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = zeroed();
            action.sa_sigaction = count_wake_signal as *const () as usize;
            assert_eq!(libc::sigaction(kick_signal, &action, ptr::null_mut()), 0);
        }

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6]);

        let mut vm = build_test_vm(60, 1, 0, 1, 1);
        vm.req.kick_signal = Some(kick_signal);
        vm.req.wake_by_signal = true;
        let handle = VmHandle::new(
            &vm.req,
            vm.shm_fd.try_clone().unwrap(),
            vm.kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            vm.wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
        .with_vmm_pid(process_id() as i32)
        .unwrap();

        // The parked vCPU's thread: a thread of this process that just sleeps.
        let stop = Arc::new(AtomicBool::new(false));
        let tid = Arc::new(AtomicU64::new(0));
        let vcpu_thread = {
            let stop = Arc::clone(&stop);
            let tid = Arc::clone(&tid);
            thread::spawn(move || {
                // SAFETY: `gettid` has no preconditions.
                tid.store(unsafe { libc::gettid() } as u64, Ordering::SeqCst);
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        while tid.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        let page = handle.shared_page();
        let host = handle.host_page();
        host.vcpus[0]
            .host_tid
            .store(tid.load(Ordering::SeqCst) as u32, Ordering::Release);
        page.telemetry.runnable_threads.store(1, Ordering::Release);
        ControlPlane::publish_vm_handle(&snapshot, handle.clone());

        // At 3 GHz, the signal is first resent after `WAKE_RESIGNAL_US` (100 us, 300,000
        // cycles), not while the VMM may still be waking the vCPU.
        let before = WAKE_SIGNALS_SEEN.load(Ordering::SeqCst);
        engine.poll_once(1_000_000);
        assert_eq!(
            engine.core_state(6),
            CoreState::Assigned { vm_id: 60, vcpu: 0 }
        );
        wait_for_wake_signals(before + 1);

        engine.poll_once(1_030_000);
        engine.poll_once(1_150_000);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(WAKE_SIGNALS_SEEN.load(Ordering::SeqCst), before + 1);
        engine.poll_once(1_300_000);
        wait_for_wake_signals(before + 2);

        // Running again: no more signals.
        host.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        engine.poll_once(2_000_000);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(WAKE_SIGNALS_SEEN.load(Ordering::SeqCst), before + 2);

        stop.store(true, Ordering::SeqCst);
        vcpu_thread.join().unwrap();
    }

    #[test]
    fn test_plane2_deadline_signals_donor_and_hands_over_after_bound() {
        // A real-time signal no other test uses, with a counting handler.
        let kick_signal = libc::SIGRTMIN() + 3;
        // SAFETY: installs a handler that only increments an atomic, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = zeroed();
            action.sa_sigaction = count_kick_signal as *const () as usize;
            assert_eq!(libc::sigaction(kick_signal, &action, ptr::null_mut()), 0);
        }

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6]);

        let mut donor = build_test_vm(30, 1, 0, 1, 1);
        donor.req.kick_signal = Some(kick_signal);
        let donor_handle = VmHandle::new(
            &donor.req,
            donor.shm_fd.try_clone().unwrap(),
            donor
                .kick_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
            donor
                .wake_fds
                .iter()
                .map(|fd| fd.try_clone().unwrap())
                .collect(),
        )
        .unwrap()
        .with_vmm_pid(process_id() as i32)
        .unwrap();
        let recipient = build_test_vm(40, 1, 0, 1, 1);

        // The donor's vCPU thread: a thread of this process that just sleeps.
        let stop = Arc::new(AtomicBool::new(false));
        let tid = Arc::new(AtomicU64::new(0));
        let vcpu_thread = {
            let stop = Arc::clone(&stop);
            let tid = Arc::clone(&tid);
            thread::spawn(move || {
                // SAFETY: `gettid` has no preconditions.
                tid.store(unsafe { libc::gettid() } as u64, Ordering::SeqCst);
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        while tid.load(Ordering::SeqCst) == 0 {
            thread::yield_now();
        }
        let donor_slot = &donor_handle.host_page().vcpus[0];
        donor_slot
            .host_tid
            .store(tid.load(Ordering::SeqCst) as u32, Ordering::Release);
        donor_slot
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        donor_slot
            .revoke_deadline_tsc
            .store(1_000, Ordering::Relaxed);
        donor_slot.revoke_requested.store(1, Ordering::Release);

        ControlPlane::publish_vm_handle(&snapshot, donor_handle.clone());
        ControlPlane::publish_vm_handle(&snapshot, recipient.handle.clone());
        engine.set_core_state(
            6,
            CoreState::Revoking {
                donor_vm_id: 30,
                donor_vcpu: 0,
                recipient_vm_id: 40,
                recipient_vcpu: 0,
                deadline_tsc: 1_000,
            },
        );

        // At 3 GHz: the signal is resent every 10 us (30,000 cycles) and the core is handed
        // over 50 us (150,000 cycles) after the deadline.
        let before = KICK_SIGNALS_SEEN.load(Ordering::SeqCst);
        engine.poll_once(1_000);
        wait_for_kick_signals(before + 1);
        assert_eq!(donor_handle.forced_preempt_count(), 1);
        assert!(matches!(engine.core_state(6), CoreState::Revoking { .. }));

        engine.poll_once(1_000 + 29_999);
        engine.poll_once(1_000 + 30_000);
        wait_for_kick_signals(before + 2);
        assert_eq!(donor_handle.forced_preempt_count(), 1);
        assert!(matches!(engine.core_state(6), CoreState::Revoking { .. }));

        // The donor never parks: the core moves anyway, counted separately. The bound counts
        // from the first signal, sent at the deadline here. Count from before this poll: it
        // signals the donor, and the handoff poll 1 cycle later doesn't signal again.
        let before_handoff = KICK_SIGNALS_SEEN.load(Ordering::SeqCst);
        engine.poll_once(1_000 + 149_999);
        assert!(matches!(engine.core_state(6), CoreState::Revoking { .. }));
        engine.poll_once(1_000 + 150_000);
        assert_eq!(
            engine.core_state(6),
            CoreState::Assigned { vm_id: 40, vcpu: 0 }
        );
        assert_eq!(donor_handle.handoff_without_park_count(), 1);
        assert_eq!(donor_handle.park_latency().count(), 0);
        assert_eq!(donor_slot.revoke_requested.load(Ordering::Acquire), 1);
        assert_eq!(
            donor_slot.assigned_pcpu.load(Ordering::Acquire),
            PCPU_UNASSIGNED
        );

        // The donor keeps running without a core, so it is signalled until the VMM parks it
        // (spec 05 K7), less often each time: the interval is the time since the deadline.
        wait_for_kick_signals(before_handoff + 1);
        engine.poll_once(400_000);
        wait_for_kick_signals(before_handoff + 2);
        thread::sleep(Duration::from_millis(20));
        let seen = KICK_SIGNALS_SEEN.load(Ordering::SeqCst);
        engine.poll_once(400_000 + 398_999);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(KICK_SIGNALS_SEEN.load(Ordering::SeqCst), seen);
        engine.poll_once(400_000 + 399_000);
        wait_for_kick_signals(seen + 1);

        donor_slot.revoke_requested.store(0, Ordering::Release);
        donor_slot.yield_count.fetch_add(1, Ordering::Relaxed);
        donor_slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
        // Let signals already sent arrive before counting.
        thread::sleep(Duration::from_millis(20));
        let parked = KICK_SIGNALS_SEEN.load(Ordering::SeqCst);
        engine.poll_once(10_000_000);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(KICK_SIGNALS_SEEN.load(Ordering::SeqCst), parked);

        stop.store(true, Ordering::SeqCst);
        vcpu_thread.join().unwrap();
    }

    #[tokio::test]
    async fn test_plane1_rejects_duplicate_vm_id_while_vmm_runs() {
        let sock_path = unique_socket_path("dup");
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        // SAFETY: `getegid` has no preconditions.
        let gid = unsafe { libc::getegid() };
        let control = Arc::new(
            ControlPlane::new(sock_path.clone(), Arc::clone(&snapshot))
                .with_socket_permissions(Some(0o660), Some(gid)),
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let control_task = {
            let control = Arc::clone(&control);
            tokio::spawn(async move { control.run(shutdown_rx).await })
        };
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // The socket gets the requested mode and group.
        let meta = fs::metadata(&sock_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o660);
        assert_eq!(meta.gid(), gid);

        // Both registrations come from this process, so the first VMM is still running.
        let register = |vm_id: u64| {
            let sock = sock_path.clone();
            async move {
                let rig = build_test_vm(vm_id, 1, 0, 1, 1);
                spawn_blocking(move || {
                    send_registration(
                        &sock,
                        &rig.req,
                        rig.shm_fd.as_raw_fd(),
                        &[rig.kick_fds[0].as_raw_fd()],
                        &[rig.wake_fds[0].as_raw_fd()],
                    )
                })
                .await
                .unwrap()
                .unwrap()
            }
        };
        assert!(register(80).await.accepted);
        let second = register(80).await;
        assert!(!second.accepted);
        assert!(
            second.message.contains("already registered"),
            "{}",
            second.message
        );
        assert_eq!(snapshot.load().len(), 1);
        assert_eq!(snapshot.load()[0].vmm_exited(), Some(false));

        let _ = shutdown_tx.send(true);
        control_task.await.unwrap().unwrap();
    }

    #[test]
    fn test_exited_vmm_is_removed_and_its_cores_freed() {
        // A VMM that has exited: a child process, reaped after its pidfd is open.
        let mut child = Command::new("true").spawn().unwrap();
        let exited_rig = build_test_vm(90, 1, 0, 1, 1);
        let exited = VmHandle::new(
            &exited_rig.req,
            exited_rig.shm_fd.try_clone().unwrap(),
            vec![exited_rig.kick_fds[0].try_clone().unwrap()],
            vec![exited_rig.wake_fds[0].try_clone().unwrap()],
        )
        .unwrap()
        .with_vmm_pid(child.id() as i32)
        .unwrap();
        child.wait().unwrap();
        assert_eq!(exited.vmm_exited(), Some(true));
        // It is never signalled.
        exited.host_page().vcpus[0]
            .host_tid
            .store(child.id(), Ordering::Release);
        assert!(!exited.signal_vcpu(0).unwrap());

        let unknown = build_test_vm(91, 1, 0, 1, 1).handle;
        assert_eq!(unknown.vmm_exited(), None);

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        ControlPlane::publish_vm_handle(&snapshot, exited);
        ControlPlane::publish_vm_handle(&snapshot, unknown);
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[7]);
        engine.set_core_state(7, CoreState::Assigned { vm_id: 90, vcpu: 0 });
        pcpu_owners.write_owner(7, 90_001, 0);

        assert_eq!(ControlPlane::remove_exited_vms(&snapshot), 1);
        let ids: Vec<u64> = snapshot.load().iter().map(VmHandle::vm_id).collect();
        assert_eq!(ids, vec![91]);

        // Plane 2 frees the core the removed VM held, even with no VMs left at all.
        snapshot.store(Arc::new(Vec::new()));
        engine.poll_once(1_000);
        assert_eq!(engine.core_state(7), CoreState::Idle);
        assert_eq!(pcpu_owners.read_owner(7).0, 0);
    }

    #[test]
    fn test_engine_owns_its_cpu_while_it_runs() {
        // SAFETY: `sched_getcpu` has no preconditions.
        let cpu = unsafe { libc::sched_getcpu() };
        assert!(cpu >= 0, "sched_getcpu failed");
        let cpu = cpu as usize;
        if cpu >= MAX_PCPUS {
            return;
        }

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = test_engine(snapshot, Arc::clone(&pcpu_owners), &[]);
        engine.set_engine_cpu(cpu);
        let stop = Arc::new(AtomicBool::new(false));
        let engine_stop = Arc::clone(&stop);
        let engine_thread = thread::spawn(move || {
            // SAFETY: `gettid` takes no arguments and can't fail.
            let tid = unsafe { libc::gettid() } as u32;
            engine.run(&engine_stop).map(|()| tid)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while pcpu_owners.read_owner(cpu).0 == 0 {
            assert!(Instant::now() < deadline, "the engine never took CPU {cpu}");
            thread::yield_now();
        }
        let (owner, flags) = pcpu_owners.read_owner(cpu);
        assert_eq!(flags, scx_pylon::PcpuOwnerFlags::HOST.bits());

        stop.store(true, Ordering::Relaxed);
        let tid = engine_thread.join().unwrap().unwrap();
        assert_eq!(owner, tid);
        assert_eq!(pcpu_owners.read_owner(cpu), (0, 0));
    }

    #[test]
    fn test_calibrated_tsc_rate_is_plausible() {
        let tsc_khz =
            calibrate_tsc_khz(Duration::from_millis(20)).expect("this architecture has a counter");
        // Any x86_64 TSC from the last two decades, and any Arm generic counter (1 GHz since
        // Armv8.6, 10 to 100 MHz before), runs between 10 MHz and 10 GHz.
        assert!((10_000..10_000_000).contains(&tsc_khz), "{tsc_khz} kHz");
    }

    #[test]
    fn test_kick_signal_allows_user_and_realtime_signals() {
        for signal in [
            libc::SIGUSR1,
            libc::SIGUSR2,
            libc::SIGRTMIN(),
            libc::SIGRTMAX(),
        ] {
            assert!(is_kick_signal(signal), "{signal}");
        }
        for signal in [
            0,
            libc::SIGKILL,
            libc::SIGSTOP,
            libc::SIGTERM,
            libc::SIGSEGV,
        ] {
            assert!(!is_kick_signal(signal), "{signal}");
        }
    }

    #[test]
    fn test_registration_rejects_other_kick_signals() {
        let mut rig = build_test_vm(50, 1, 0, 1, 1);
        rig.req.kick_signal = Some(libc::SIGKILL);
        let err = VmHandle::new(
            &rig.req,
            rig.shm_fd.try_clone().unwrap(),
            vec![rig.kick_fds[0].try_clone().unwrap()],
            vec![rig.wake_fds[0].try_clone().unwrap()],
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidRegistration { vm_id: 50, .. }));
    }

    #[test]
    fn test_latency_histogram_bounds() {
        assert_eq!(
            &LATENCY_BUCKETS_US[..17],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18]
        );
        assert_eq!(&LATENCY_BUCKETS_US[23..25], &[32, 36]);
        assert_eq!(LATENCY_BUCKETS_US[LATENCY_BUCKETS_US.len() - 1], 4096);
        assert!(LATENCY_BUCKETS_US.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn test_latency_histogram_buckets() {
        let histogram = LatencyHistogram::default();
        for usec in [0, 1, 3, 8, 9, 17, 5_000] {
            histogram.record(usec);
        }
        let cumulative = histogram.cumulative_buckets();
        // le=1: {0, 1}; le=3: +3; le=8: +8; le=9: +9; le=18: +17; +Inf: +5,000.
        assert_eq!(cumulative[0], 2);
        assert_eq!(cumulative[1], 2);
        assert_eq!(cumulative[2], 3);
        assert_eq!(cumulative[7], 4);
        assert_eq!(cumulative[8], 5);
        assert_eq!(cumulative[15], 5);
        assert_eq!(cumulative[16], 6);
        assert_eq!(cumulative[LATENCY_BUCKETS_US.len() - 1], 6);
        assert_eq!(cumulative[LATENCY_BUCKETS_US.len()], 7);
        assert_eq!(histogram.count(), 7);
        assert_eq!(histogram.sum_us(), 5_038);
    }

    #[test]
    fn test_rdtsc_and_pause_primitives() {
        let t1 = read_tsc();
        pause_cycles(t1, 100, false);
        let t2 = read_tsc();
        assert!(t2 >= t1);
    }

    /// With and without the timed wait, a pause neither hangs nor (over many calls) returns much
    /// sooner than asked. A single `WFET` may wake early, so the check is on the total.
    #[test]
    fn test_pause_cycles_waits_about_the_interval() {
        let khz =
            calibrate_tsc_khz(Duration::from_millis(20)).expect("this architecture has a counter");
        let interval = khz / 1000; // 1 µs
        for timed in [false, detect_timed_wait()] {
            let calls = 200;
            let start = read_tsc();
            for _ in 0..calls {
                let now = read_tsc();
                pause_cycles(now, interval, timed);
                assert!(read_tsc() - now < khz * 50, "a pause took over 50 ms");
            }
            let total = read_tsc() - start;
            // Without the timed wait the pause is only a spin hint and may be far shorter.
            if timed {
                assert!(
                    total >= calls * interval / 2,
                    "timed={timed}: {total} ticks"
                );
            }
        }
    }
}
