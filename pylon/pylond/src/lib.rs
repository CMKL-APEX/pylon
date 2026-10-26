// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

pub mod control;
pub mod engine;

use core::array::from_fn;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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

    #[error("Invalid file descriptor count: got {actual}, expected {expected}")]
    InvalidFdCount { actual: usize, expected: usize },

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
}

/// JSON response returned by `pylond` after processing a `VmRegistrationRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRegistrationResponse {
    pub accepted: bool,
    pub message: String,
}

#[derive(Debug)]
struct VmHandleInner {
    vm_id: u64,
    num_vcpus: u32,
    min_cores: u32,
    max_cores: u32,
    priority_weight: u32,
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
                min_cores: req.min_cores,
                max_cores: req.max_cores,
                priority_weight: req.priority_weight.max(1),
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
        self.inner.min_cores
    }

    #[inline]
    pub fn max_cores(&self) -> u32 {
        self.inner.max_cores
    }

    #[inline]
    pub fn priority_weight(&self) -> u32 {
        self.inner.priority_weight
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
    use std::path::PathBuf;
    use std::process::id as process_id;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use pylon_abi::{PCPU_UNASSIGNED, VCPU_STATE_PARKED, VCPU_STATE_RUNNING, VCPU_STATE_WAKING};
    use tokio::sync::watch;
    use tokio::task::spawn_blocking;
    use tokio::time::sleep;

    use super::*;
    use crate::control::{ControlPlane, send_registration};
    use crate::engine::{MatrixEngine, pause_cycles, read_tsc};

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
        for vcpu_idx in 0..(num_vcpus as usize) {
            let tid = (vm_id as u32) * 1000 + (vcpu_idx as u32) + 1;
            page.vcpus[vcpu_idx].host_tid.store(tid, Ordering::Release);
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
        assert_eq!(
            vm_timer.handle.shared_page().vcpus[2]
                .state
                .load(Ordering::Acquire),
            VCPU_STATE_WAKING
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
            VCPU_STATE_WAKING
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
        assert_eq!(
            donor_page.vcpus[1]
                .forced_preempt_count
                .load(Ordering::Acquire),
            0
        );
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
            VCPU_STATE_WAKING
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

        // Poll at deadline (now_tsc = 30,000): Donor vCPU 0 is forcibly preempted!
        engine.poll_once(30_000);
        assert_eq!(
            engine.core_state(4),
            CoreState::Assigned { vm_id: 20, vcpu: 1 }
        );
        assert_eq!(
            donor_page.vcpus[0]
                .forced_preempt_count
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(pcpu_owners.read_owner(4).0, 20_002);
        assert_eq!(
            recipient_page.vcpus[1]
                .assigned_pcpu
                .load(Ordering::Acquire),
            4
        );
        assert_eq!(
            recipient_page.vcpus[1].state.load(Ordering::Acquire),
            VCPU_STATE_WAKING
        );
        assert_eq!(read_eventfd(recipient.wake_fds[1].as_raw_fd()).unwrap(), 1);
    }

    #[test]
    fn test_rdtsc_and_pause_primitives() {
        let t1 = read_tsc();
        pause_cycles(t1, 100, false);
        let t2 = read_tsc();
        assert!(t2 >= t1);
    }
}
