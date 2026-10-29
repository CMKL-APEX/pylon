// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

pub mod control;
pub mod engine;

use core::array::from_fn;
use std::fs::File;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::{io, ptr};

use arc_swap::ArcSwap;
use memmap2::MmapMut;
use pylon_abi::{MAX_VCPUS, PYLON_SHARED_PAGE_SIZE, PylonSharedPage};
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
                // SAFETY: `mmap` was verified to be at least `MAX_PCPUS * 8` bytes and page-aligned.
                unsafe { &*mmap.as_ptr().cast::<PcpuOwnerEntry>().add(pcpu_id) }
            }
            Self::Heap(entries) => &entries[pcpu_id],
        }
    }
}

/// Memory-mapped or heap-backed table of `PcpuOwnerEntry` records indexed by physical CPU ID.
#[derive(Debug)]
pub struct PcpuOwnerTable {
    storage: PcpuOwnerStorage,
}

impl PcpuOwnerTable {
    /// Creates an in-memory `PcpuOwnerTable` suitable for unprivileged execution and testing.
    pub fn new_in_memory() -> Self {
        let entries: Box<[PcpuOwnerEntry; MAX_PCPUS]> =
            Box::new(from_fn(|_| PcpuOwnerEntry::new()));
        Self {
            storage: PcpuOwnerStorage::Heap(entries),
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
        })
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

/// JSON payload sent by Cloud Hypervisor over `/run/pylon/pylond.sock` during VM registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationRequest {
    pub vm_id: u64,
    pub num_vcpus: u32,
    pub min_cores: u32,
    pub max_cores: u32,
    pub priority_weight: u32,
    /// Signal that interrupts a vCPU's `KVM_RUN` in the registering VMM (its `SIGRTMIN`, which
    /// depends on the VMM's libc). `pylond` sends it when a revocation's deadline passes
    /// (`spec/06-guest-kick-path.md` §4.4). Without it, `pylond` never signals the VMM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kick_signal: Option<i32>,
}

/// JSON response returned by `pylond` after processing a `VmRegistrationRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationResponse {
    pub accepted: bool,
    pub message: String,
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
    park_latency: ParkLatencyHistogram,
    kick_signal: Option<i32>,
    /// The registering VMM's PID from `SO_PEERCRED` (0 if unknown), and a pidfd that keeps
    /// referring to that process, so a reused PID is never signalled.
    vmm_pid: i32,
    vmm_pidfd: Option<OwnedFd>,
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
            && !(libc::SIGRTMIN()..=libc::SIGRTMAX()).contains(&signal)
        {
            return Err(Error::InvalidRegistration {
                vm_id: req.vm_id,
                reason: format!("kick_signal {signal} is not a real-time signal"),
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
        // SAFETY: `shm_file` refers to the 8,192-byte Pylon shared memory page whose contents
        // are accessed solely via atomic operations defined in `pylon-abi`.
        let shm_mmap = unsafe { MmapMut::map_mut(&shm_file).map_err(Error::Io)? };
        if shm_mmap.len() < PYLON_SHARED_PAGE_SIZE {
            return Err(Error::SharedMemoryTooSmall {
                actual: shm_mmap.len(),
                expected: PYLON_SHARED_PAGE_SIZE,
            });
        }

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
                park_latency: ParkLatencyHistogram::default(),
                kick_signal: req.kick_signal,
                vmm_pid: 0,
                vmm_pidfd: None,
                shm_mmap,
                kick_irqfds,
                wake_eventfds,
                host_rx_doorbell: AtomicU32::new(0),
            }),
        };

        if !handle.shared_page().header.is_valid() {
            return Err(Error::InvalidSharedHeader { vm_id: req.vm_id });
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

    /// Records the time from a revocation request to the donor vCPU parking.
    #[inline]
    pub fn record_park_latency_us(&self, usec: u64) {
        self.inner.park_latency.record(usec);
    }

    /// Returns the revocation-to-park latency histogram for this VM.
    #[inline]
    pub fn park_latency(&self) -> &ParkLatencyHistogram {
        &self.inner.park_latency
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

    /// Sends the VMM's `kick_signal` to the thread of `vcpu_idx`, interrupting its `KVM_RUN`
    /// (`spec/06-guest-kick-path.md` §4.4). Returns `Ok(false)` without sending when the VMM gave
    /// no signal, its PID is unknown, the vCPU's thread ID isn't known yet, or the VMM has exited.
    pub fn signal_vcpu(&self, vcpu_idx: usize) -> Result<bool, Error> {
        let slot = self
            .shared_page()
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
        // SAFETY: `shm_mmap` length was verified to be >= `PYLON_SHARED_PAGE_SIZE` (8192)
        // and `mmap` guarantees page alignment (4096 bytes), matching `PylonSharedPage`.
        unsafe { &*(self.inner.shm_mmap.as_ptr().cast::<PylonSharedPage>()) }
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

    /// Signals the KVM `irqfd` for `vcpu_idx` to inject an immediate revocation IPI.
    #[inline]
    pub fn kick_revoke_irqfd(&self, vcpu_idx: usize) -> Result<(), Error> {
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

/// Upper bounds, in microseconds, of the revocation-to-park latency histogram buckets.
pub const PARK_LATENCY_BUCKETS_US: [u64; 11] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];

/// Allocation-free histogram of revocation-to-park latencies, in microseconds. Written by
/// Plane 2, read by the Prometheus exporter.
#[derive(Debug)]
pub struct ParkLatencyHistogram {
    /// Per-bucket counts (not cumulative); the last entry counts values above every bound.
    buckets: [AtomicU64; PARK_LATENCY_BUCKETS_US.len() + 1],
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Default for ParkLatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: from_fn(|_| AtomicU64::new(0)),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl ParkLatencyHistogram {
    /// Records one latency of `usec` microseconds.
    pub fn record(&self, usec: u64) {
        let idx = PARK_LATENCY_BUCKETS_US
            .iter()
            .position(|&bound| usec <= bound)
            .unwrap_or(PARK_LATENCY_BUCKETS_US.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(usec, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Cumulative counts for each bound in `PARK_LATENCY_BUCKETS_US`, then for `+Inf`.
    pub fn cumulative_buckets(&self) -> [u64; PARK_LATENCY_BUCKETS_US.len() + 1] {
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
    use std::ffi::CString;
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
    use crate::engine::{MatrixEngine, calibrate_tsc_khz, pause_cycles, read_tsc};

    struct TestVmRig {
        req: VmRegistrationRequest,
        shm_fd: OwnedFd,
        kick_fds: Vec<OwnedFd>,
        wake_fds: Vec<OwnedFd>,
        handle: VmHandle,
    }

    fn create_memfd_shared_page(vm_id: u64, num_vcpus: u32, tsc_khz: u64) -> OwnedFd {
        let name = CString::new(format!("pylon-test-vm-{vm_id}")).unwrap();
        // SAFETY: `name` is a valid NUL-terminated C string; `MFD_CLOEXEC` is valid.
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw_fd >= 0, "memfd_create failed");
        // SAFETY: `raw_fd` is a newly created valid file descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // SAFETY: `owned` is a valid memfd file descriptor.
        let trunc_rc =
            unsafe { libc::ftruncate(owned.as_raw_fd(), PYLON_SHARED_PAGE_SIZE as libc::off_t) };
        assert_eq!(trunc_rc, 0, "ftruncate failed");

        let file = File::from(owned.try_clone().unwrap());
        // SAFETY: `file` is truncated to `PYLON_SHARED_PAGE_SIZE` (8192 bytes).
        let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };
        // SAFETY: `mmap` is page-aligned and 8192 bytes, matching `PylonSharedPage`.
        let page = unsafe { &mut *(mmap.as_mut_ptr().cast::<PylonSharedPage>()) };
        page.init(vm_id, num_vcpus, tsc_khz);
        for (vcpu_idx, slot) in page.vcpus.iter().enumerate().take(num_vcpus as usize) {
            let tid = (vm_id as u32) * 1000 + (vcpu_idx as u32) + 1;
            slot.host_tid.store(tid, Ordering::Release);
        }
        owned
    }

    fn build_test_vm(
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
        };
        let shm_fd = create_memfd_shared_page(vm_id, num_vcpus, 3_000_000);
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
            vm.shared_page().vcpus[2].host_tid.load(Ordering::Acquire),
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
        let mut engine =
            MatrixEngine::new(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[2, 3]);

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
            vm_timer.handle.shared_page().vcpus[2]
                .state
                .load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(
            vm_timer.handle.shared_page().vcpus[2]
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
            vm_rx.handle.shared_page().vcpus[1]
                .state
                .load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(pcpu_owners.read_owner(3).0, 2002);
        assert_eq!(read_eventfd(vm_rx.wake_fds[1].as_raw_fd()).unwrap(), 1);
    }

    #[test]
    fn test_plane2_phase2_and_phase3_cooperative_and_forced_preemption() {
        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        // Manage 2 physical cores: pCPU 4 and pCPU 5.
        let mut engine =
            MatrixEngine::new(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[4, 5]);

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
        donor_page.vcpus[0]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        donor_page.vcpus[0]
            .in_critical_section
            .store(1, Ordering::Release);
        donor_page.vcpus[1]
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
            donor_page.vcpus[1].revoke_requested.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            donor_page.vcpus[1]
                .revoke_deadline_tsc
                .load(Ordering::Acquire),
            26_000
        );
        assert_eq!(read_eventfd(donor.kick_fds[1].as_raw_fd()).unwrap(), 1);

        // Step 3: Cooperative handoff before deadline (now_tsc = 5,000 < 26,000).
        // Donor vCPU 1 parks cooperatively.
        donor_page.vcpus[1]
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
            donor_page.vcpus[1].assigned_pcpu.load(Ordering::Acquire),
            PCPU_UNASSIGNED
        );
        assert_eq!(pcpu_owners.read_owner(5).0, 20_001);
        assert_eq!(
            recipient_page.vcpus[0]
                .assigned_pcpu
                .load(Ordering::Acquire),
            5
        );
        assert_eq!(
            recipient_page.vcpus[0].state.load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(read_eventfd(recipient.wake_fds[0].as_raw_fd()).unwrap(), 1);

        // Step 4: Test Forced Preemption on deadline expiry!
        // Suppose Donor's min_cores is lowered or Donor drops runnable_threads to 0 so Donor vCPU 0 is over-allocated,
        // and Recipient wants a 2nd core (`runnable_threads = 2`).
        donor_page
            .telemetry
            .runnable_threads
            .store(0, Ordering::Release);
        recipient_page.vcpus[0]
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
        donor_page.vcpus[0]
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
            donor_page.vcpus[0].state.load(Ordering::Acquire),
            VCPU_STATE_RUNNING
        );
        assert_eq!(
            donor_page.vcpus[0].revoke_requested.load(Ordering::Acquire),
            1
        );
        assert_eq!(
            donor_page.vcpus[0]
                .forced_preempt_count
                .load(Ordering::Acquire),
            0
        );

        // The VMM force-parks the donor at its next exit; the core then moves.
        donor_page.vcpus[0]
            .revoke_requested
            .store(0, Ordering::Release);
        donor_page.vcpus[0]
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
            recipient_page.vcpus[1]
                .assigned_pcpu
                .load(Ordering::Acquire),
            4
        );
        assert_eq!(
            recipient_page.vcpus[1].state.load(Ordering::Acquire),
            VCPU_STATE_PARKED
        );
        assert_eq!(read_eventfd(recipient.wake_fds[1].as_raw_fd()).unwrap(), 1);
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
        let mut engine = MatrixEngine::new(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[6]);

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
        let donor_slot = &donor_handle.shared_page().vcpus[0];
        donor_slot
            .host_tid
            .store(tid.load(Ordering::SeqCst) as u32, Ordering::Release);
        donor_slot
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
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

        // The donor never parks: the core moves anyway, counted separately.
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
        exited.shared_page().vcpus[0]
            .host_tid
            .store(child.id(), Ordering::Release);
        assert!(!exited.signal_vcpu(0).unwrap());

        let unknown = build_test_vm(91, 1, 0, 1, 1).handle;
        assert_eq!(unknown.vmm_exited(), None);

        let snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
        ControlPlane::publish_vm_handle(&snapshot, exited);
        ControlPlane::publish_vm_handle(&snapshot, unknown);
        let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
        let mut engine = MatrixEngine::new(Arc::clone(&snapshot), Arc::clone(&pcpu_owners), &[7]);
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
    fn test_calibrated_tsc_rate_is_plausible() {
        let tsc_khz = calibrate_tsc_khz(Duration::from_millis(20)).expect("x86_64 has a TSC");
        // Any x86_64 TSC from the last two decades runs between 500 MHz and 10 GHz.
        assert!((500_000..10_000_000).contains(&tsc_khz), "{tsc_khz} kHz");
    }

    #[test]
    fn test_registration_rejects_non_realtime_kick_signal() {
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
    fn test_park_latency_histogram_buckets() {
        let histogram = ParkLatencyHistogram::default();
        for usec in [0, 1, 3, 8, 9, 5_000] {
            histogram.record(usec);
        }
        let cumulative = histogram.cumulative_buckets();
        // le=1: {0, 1}; le=4: +3; le=8: +8; le=16: +9; +Inf: +5,000.
        assert_eq!(cumulative[0], 2);
        assert_eq!(cumulative[2], 3);
        assert_eq!(cumulative[3], 4);
        assert_eq!(cumulative[4], 5);
        assert_eq!(cumulative[PARK_LATENCY_BUCKETS_US.len() - 1], 5);
        assert_eq!(cumulative[PARK_LATENCY_BUCKETS_US.len()], 6);
        assert_eq!(histogram.count(), 6);
        assert_eq!(histogram.sum_us(), 5_021);
    }

    #[test]
    fn test_rdtsc_and_pause_primitives() {
        let t1 = read_tsc();
        pause_cycles(t1, 100, false);
        let t2 = read_tsc();
        assert!(t2 >= t1);
    }
}
