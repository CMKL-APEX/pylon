// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::{env, fs, io, ptr, result};

#[cfg(target_arch = "x86_64")]
use hypervisor::arch::x86::{MsrEntry, msr_index};
use hypervisor::{HypervisorType, HypervisorVmError, MemoryVisibility, Vcpu, Vm};
use log::{info, warn};
use option_parser::{OptionParser, OptionParserError};
use pylon_abi::clock::read_counter;
use pylon_abi::{
    MAX_VCPUS, PCPU_UNASSIGNED, PYLON_ABI_VERSION, PYLON_DEFAULT_GPA, PYLON_MAGIC,
    PYLON_SHARED_PAGE_SIZE, PylonHeader, PylonSharedPage, PylonVcpuSlot, VCPU_STATE_PARKED,
    VCPU_STATE_PARKING, VCPU_STATE_RUNNING,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use vm_memory::guest_memory::FileOffset;
use vm_memory::mmap::MmapRegionError;
use vm_memory::{Address, GuestAddress, GuestMemoryRegion, MmapRegion};
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::signal::SIGRTMIN;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::GuestRegionMmap;

// The shared page's address is declared in the architecture's layout and in the ABI crate, which
// can't see each other, so check here that they agree.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = {
    assert!(arch::layout::PYLON_SHARED_START.0 == PYLON_DEFAULT_GPA);
    assert!(arch::layout::PYLON_SHARED_SIZE == PYLON_SHARED_PAGE_SIZE as u64);
};

/// TSC rate written into `PylonHeader`: unknown. The VMM writes the header before any vCPU
/// exists, so it can't ask KVM; `pylond` measures the host TSC rate itself instead.
pub const UNKNOWN_TSC_KHZ: u64 = 0;

/// Default VM priority weight for Pylon core arbitration.
pub const DEFAULT_PRIORITY_WEIGHT: u32 = 100;

/// Environment variable that parks vCPUs inside KVM instead of in the VMM
/// (`spec/08-park-in-kvm.md`, prototype step P0) when set to `1`.
pub const PARK_IN_KVM_ENV: &str = "PYLON_PARK_IN_KVM";

/// Halt polling limit, in nanoseconds, for a VM that parks vCPUs inside KVM. P0 turns halt
/// polling off, so a parked vCPU costs no host CPU (`spec/08-park-in-kvm.md` §3.5).
const PARK_IN_KVM_HALT_POLL_NS: u32 = 0;

static GLOBAL_PYLON_STATE: Mutex<Option<Arc<PylonVmmState>>> = Mutex::new(None);

/// Errors associated with the Pylon VMM subsystem.
#[derive(Debug, Error)]
pub enum Error {
    /// Failed to parse `--pylon` options.
    #[error("Error parsing --pylon options")]
    OptionParse(#[source] OptionParserError),

    /// Missing required `shm` parameter in `--pylon`.
    #[error("Error parsing --pylon: shm path missing")]
    MissingShmPath,

    /// `--pylon` on a hypervisor backend that can't read the guest's counter and timer.
    #[error("--pylon is not supported on MSHV on aarch64")]
    UnsupportedHypervisor,

    /// Invalid `gpa` parameter in `--pylon`.
    #[error("Error parsing --pylon: invalid GPA {0}")]
    InvalidGpa(String),

    /// Invalid vCPU count for the Pylon shared page.
    #[error("Invalid Pylon vCPU count {0} (must be between 1 and {MAX_VCPUS})")]
    InvalidVcpuCount(u32),

    /// Invalid vCPU index accessed.
    #[error("Invalid Pylon vCPU index {0}")]
    InvalidVcpuId(u32),

    /// Failed to create or open the `/dev/shm/<vm-id>-pylon` shared memory file.
    #[error("Failed to open Pylon shared memory file")]
    ShmOpen(#[source] io::Error),

    /// Failed to set the length of the shared memory file to 8,192 bytes.
    #[error("Failed to resize Pylon shared memory file")]
    ShmSetLen(#[source] io::Error),

    /// Failed to `mmap` the Pylon shared memory page into VMM address space.
    #[error("Failed to mmap Pylon shared memory page")]
    ShmMmap(#[source] io::Error),

    /// Failed to create a per-vCPU `EventFd`.
    #[error("Failed to create Pylon EventFd")]
    EventFdCreate(#[source] io::Error),

    /// Failed to write to a per-vCPU `EventFd`.
    #[error("Failed to signal Pylon EventFd")]
    EventFdWrite(#[source] io::Error),

    /// Failed to build the `MmapRegion` for GPA mapping.
    #[error("Failed to build Pylon MmapRegion")]
    MmapRegionBuild(#[source] MmapRegionError),

    /// Failed to construct the `GuestRegionMmap` at the requested GPA.
    #[error("Failed to create Pylon GuestRegionMmap at GPA {0:#x}")]
    GuestRegionNew(u64),

    /// Failed to register the Pylon shared memory region with the hypervisor.
    #[error("Failed to register Pylon user memory region with hypervisor")]
    CreateUserMemoryRegion(#[source] HypervisorVmError),

    /// Failed to register the VM with the `pylond` Unix domain socket.
    #[error("Failed to register VM with pylond daemon")]
    DaemonRegister(#[source] io::Error),
}

/// Configuration for the Pylon microsecond core arbitration subsystem (`--pylon`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PylonConfig {
    /// Path to the 8,192-byte shared memory backing file (e.g., `/dev/shm/<vm-id>-pylon`).
    pub shm: PathBuf,
    /// Optional path to the `pylond` control plane Unix socket (e.g., `/run/pylon/pylond.sock`).
    pub socket: Option<PathBuf>,
    /// Guest Physical Address where the shared page is mapped (defaults to `0xFEDF_0000`).
    pub gpa: u64,
    /// Number of vCPUs initially active at VM boot (defaults to `boot_vcpus`).
    pub initial_active_vcpus: Option<u32>,
    /// Minimum guaranteed physical cores for this VM.
    pub min_cores: u32,
    /// Maximum physical cores this VM can scale to (defaults to `max_vcpus`).
    pub max_cores: Option<u32>,
    /// Relative scheduling weight for `pylond` arbitration.
    pub priority_weight: u32,
    /// Identifier for this VM in `PylonHeader` and `pylond`.
    pub vm_id: u64,
    /// Park vCPUs inside KVM (`spec/08-park-in-kvm.md`), from `PYLON_PARK_IN_KVM`.
    pub park_in_kvm: bool,
}

impl PylonConfig {
    pub const SYNTAX: &'static str = "Pylon elastic core arbitration parameters \
        \"shm=<shm_path>[,socket=<pylond_sock_path>,gpa=<hex_or_dec_gpa>,\
        initial_active_vcpus=<n>,min_cores=<n>,max_cores=<n>,priority_weight=<n>,vm_id=<id>]\"";

    /// Parses a `--pylon` CLI configuration string.
    pub fn parse(pylon: &str) -> result::Result<Self, Error> {
        let mut parser = OptionParser::new();
        parser
            .add("shm")
            .add("socket")
            .add("gpa")
            .add("initial_active_vcpus")
            .add("min_cores")
            .add("max_cores")
            .add("priority_weight")
            .add("vm_id");
        parser.parse(pylon).map_err(Error::OptionParse)?;

        let shm_str = parser.get("shm").ok_or(Error::MissingShmPath)?;
        if shm_str.trim().is_empty() {
            return Err(Error::MissingShmPath);
        }
        let shm = PathBuf::from(shm_str);

        let socket = parser
            .get("socket")
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);

        let gpa = if let Some(gpa_str) = parser.get("gpa") {
            let trimmed = gpa_str.trim();
            let parsed = if let Some(hex) = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
            {
                u64::from_str_radix(hex, 16).map_err(|_| Error::InvalidGpa(gpa_str.clone()))?
            } else {
                trimmed
                    .parse::<u64>()
                    .map_err(|_| Error::InvalidGpa(gpa_str.clone()))?
            };
            if parsed % 4096 != 0 {
                return Err(Error::InvalidGpa(gpa_str));
            }
            parsed
        } else {
            PYLON_DEFAULT_GPA
        };

        let initial_active_vcpus = parser
            .convert::<u32>("initial_active_vcpus")
            .map_err(Error::OptionParse)?;
        let min_cores = parser
            .convert::<u32>("min_cores")
            .map_err(Error::OptionParse)?
            .unwrap_or(1);
        let max_cores = parser
            .convert::<u32>("max_cores")
            .map_err(Error::OptionParse)?;
        let priority_weight = parser
            .convert::<u32>("priority_weight")
            .map_err(Error::OptionParse)?
            .unwrap_or(DEFAULT_PRIORITY_WEIGHT);
        let vm_id = parser
            .convert::<u64>("vm_id")
            .map_err(Error::OptionParse)?
            .unwrap_or(1);

        Ok(PylonConfig {
            shm,
            socket,
            gpa,
            initial_active_vcpus,
            min_cores,
            max_cores,
            priority_weight,
            vm_id,
            park_in_kvm: env::var(PARK_IN_KVM_ENV).is_ok_and(|v| v == "1"),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct DaemonRegistrationPayload {
    vm_id: u64,
    num_vcpus: u32,
    min_cores: u32,
    max_cores: u32,
    priority_weight: u32,
    /// The signal that interrupts a vCPU's `KVM_RUN` here (`SIGRTMIN`, as installed in
    /// `vmm/src/cpu.rs`). `pylond` sends it to a donor vCPU whose revocation deadline has
    /// passed, so the post-exit hook force-parks it (`spec/06-guest-kick-path.md` §4.4).
    kick_signal: i32,
    /// Set when vCPUs park inside KVM: the wake `EventFd` can't reach a thread blocked in
    /// `KVM_RUN`, so `pylond` also sends `kick_signal` when it assigns a core
    /// (`spec/08-park-in-kvm.md` §3.3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    wake_by_signal: bool,
}

/// VMM-side Pylon runtime state managing the 8,192-byte `PylonSharedPage` and per-vCPU park/wake primitives.
pub struct PylonVmmState {
    config: PylonConfig,
    boot_vcpus: u32,
    max_vcpus: u32,
    initial_active_vcpus: u32,
    shm_file: File,
    shared_page: NonNull<PylonSharedPage>,
    /// One per vCPU. `pylond` writes one to deliver a revocation; the `pylon-kick` device
    /// installs them as its MSI-X notifiers (`vmm/src/pylon_kick.rs`).
    kick_eventfds: Vec<EventFd>,
    /// One per vCPU, written by `pylond` when it assigns a core. Non-blocking when vCPUs park
    /// inside KVM: the vCPU thread then only checks it after an exit.
    wake_eventfds: Vec<EventFd>,
    /// One per vCPU: whether it is parked inside KVM (`spec/08-park-in-kvm.md` §3.1). Only the
    /// vCPU's own thread uses it.
    parked_in_kvm: Vec<AtomicBool>,
    shutting_down: AtomicBool,
}

// SAFETY: `PylonVmmState` owns an `mmap`-backed `PylonSharedPage` whose fields after initialization
// are accessed exclusively via atomic operations (`AtomicU8`, `AtomicU32`, `AtomicU64`).
unsafe impl Send for PylonVmmState {}
// SAFETY: Concurrent access to `PylonSharedPage` across vCPU threads and VMM control threads uses
// cache-line-aligned atomic fields and thread-safe `EventFd` descriptors.
unsafe impl Sync for PylonVmmState {}

impl Drop for PylonVmmState {
    fn drop(&mut self) {
        // SAFETY: `self.shared_page` was created via `libc::mmap` with length `PYLON_SHARED_PAGE_SIZE`
        // in `PylonVmmState::new` and remains valid until `Drop`.
        unsafe {
            libc::munmap(self.shared_page.as_ptr().cast(), PYLON_SHARED_PAGE_SIZE);
        }
    }
}

impl PylonVmmState {
    /// Creates and initializes the Pylon shared memory page and per-vCPU eventfds.
    pub fn new(
        config: &PylonConfig,
        boot_vcpus: u32,
        max_vcpus: u32,
    ) -> result::Result<Arc<Self>, Error> {
        let num_vcpus = max_vcpus.max(boot_vcpus);
        if num_vcpus == 0 || (num_vcpus as usize) > MAX_VCPUS {
            return Err(Error::InvalidVcpuCount(num_vcpus));
        }

        if let Some(parent) = config.shm.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(Error::ShmOpen)?;
        }

        let shm_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&config.shm)
            .map_err(Error::ShmOpen)?;

        shm_file
            .set_len(PYLON_SHARED_PAGE_SIZE as u64)
            .map_err(Error::ShmSetLen)?;

        // SAFETY: Mapping a valid, 8192-byte file descriptor with `MAP_SHARED` into process memory.
        let mapped_ptr = unsafe {
            libc::mmap(
                null_mut(),
                PYLON_SHARED_PAGE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                shm_file.as_raw_fd(),
                0,
            )
        };
        if mapped_ptr == libc::MAP_FAILED {
            return Err(Error::ShmMmap(io::Error::last_os_error()));
        }

        let shared_page = NonNull::new(mapped_ptr.cast::<PylonSharedPage>())
            .ok_or_else(|| Error::ShmMmap(io::Error::other("mmap returned null pointer")))?;

        // Zero-initialize the entire 8,192-byte page and write the header.
        // SAFETY: `shared_page` points to a valid 8,192-byte writable `MAP_SHARED` region.
        unsafe {
            ptr::write_bytes(shared_page.as_ptr().cast::<u8>(), 0, PYLON_SHARED_PAGE_SIZE);
            ptr::write(
                &raw mut (*shared_page.as_ptr()).header,
                PylonHeader {
                    magic: PYLON_MAGIC,
                    version: PYLON_ABI_VERSION,
                    num_vcpus,
                    vm_id: config.vm_id,
                    tsc_khz: UNKNOWN_TSC_KHZ,
                    _reserved: [0u8; 32],
                },
            );
        }
        // `pylond` treats a parked vCPU as free for a core only while its `assigned_pcpu` is
        // unassigned, which zeroing doesn't produce.
        // SAFETY: `shared_page` points to the 8,192-byte mapping initialized above, and its vCPU
        // slots are only accessed through atomics.
        let page = unsafe { shared_page.as_ref() };
        for slot in &page.vcpus {
            slot.assigned_pcpu.store(PCPU_UNASSIGNED, Ordering::Release);
        }

        let wake_flags = if config.park_in_kvm {
            info!("Pylon: VM {} parks vCPUs inside KVM", config.vm_id);
            libc::EFD_NONBLOCK
        } else {
            0
        };
        let mut kick_eventfds = Vec::with_capacity(num_vcpus as usize);
        let mut wake_eventfds = Vec::with_capacity(num_vcpus as usize);
        for _ in 0..num_vcpus {
            kick_eventfds.push(EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFdCreate)?);
            wake_eventfds.push(EventFd::new(wake_flags).map_err(Error::EventFdCreate)?);
        }

        let initial_active_vcpus = config
            .initial_active_vcpus
            .unwrap_or(boot_vcpus)
            .min(num_vcpus);

        let state = Arc::new(Self {
            config: config.clone(),
            boot_vcpus,
            max_vcpus: num_vcpus,
            initial_active_vcpus,
            shm_file,
            shared_page,
            kick_eventfds,
            wake_eventfds,
            parked_in_kvm: (0..num_vcpus).map(|_| AtomicBool::new(false)).collect(),
            shutting_down: AtomicBool::new(false),
        });

        if let Some(socket_path) = &config.socket {
            if socket_path.exists() {
                if let Err(e) = state.register_with_daemon(socket_path) {
                    warn!(
                        "Pylon: failed to register VM {} with pylond at {:?}: {e}",
                        config.vm_id, socket_path
                    );
                }
            } else {
                info!(
                    "Pylon: daemon socket {socket_path:?} does not exist yet; continuing with shared memory initialized"
                );
            }
        }

        Ok(state)
    }

    /// Connects to `pylond` over Unix domain socket and transmits VM registration metadata
    /// along with `shm_fd`, per-vCPU `kick_eventfds`, and per-vCPU `wake_eventfds` via `SCM_RIGHTS`.
    pub fn register_with_daemon(&self, socket_path: &Path) -> result::Result<(), Error> {
        let stream = UnixStream::connect(socket_path).map_err(Error::DaemonRegister)?;
        let payload = DaemonRegistrationPayload {
            vm_id: self.config.vm_id,
            num_vcpus: self.max_vcpus,
            min_cores: self.config.min_cores,
            max_cores: self.config.max_cores.unwrap_or(self.max_vcpus),
            priority_weight: self.config.priority_weight,
            kick_signal: SIGRTMIN(),
            wake_by_signal: self.config.park_in_kvm,
        };
        let mut json_bytes =
            serde_json::to_vec(&payload).map_err(|e| Error::DaemonRegister(io::Error::other(e)))?;
        json_bytes.push(b'\n');

        let mut fds = Vec::with_capacity(1 + 2 * (self.max_vcpus as usize));
        fds.push(self.shm_file.as_raw_fd());
        for kick in &self.kick_eventfds {
            fds.push(kick.as_raw_fd());
        }
        for wake in &self.wake_eventfds {
            fds.push(wake.as_raw_fd());
        }

        stream
            .send_with_fds(&[json_bytes.as_slice()], &fds)
            .map_err(|e| Error::DaemonRegister(e.into()))?;
        Ok(())
    }

    /// Maps the 8,192-byte `/dev/shm/<vm-id>-pylon` backing file into guest physical memory
    /// at `self.config.gpa` (`0xFEDF_0000` by default) with `MAP_SHARED`.
    ///
    /// The page gets its own hypervisor memory slot but stays out of `GuestMemoryMmap`, like a
    /// device BAR: anything built from guest RAM (E820, PVH memmap, ACPI, hotplug layout)
    /// would otherwise report it to the guest as RAM inside the 32-bit reserved hole. The
    /// caller must keep the returned region alive as long as the VM.
    pub fn setup_gpa_mapping(
        &self,
        vm: &Arc<dyn Vm>,
        next_memory_slot: &mut u32,
    ) -> result::Result<Arc<GuestRegionMmap>, Error> {
        let shm_clone = self.shm_file.try_clone().map_err(Error::ShmOpen)?;
        let file_offset = FileOffset::new(shm_clone, 0);

        let mmap_region = MmapRegion::build(
            Some(file_offset),
            PYLON_SHARED_PAGE_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
        )
        .map_err(Error::MmapRegionBuild)?;

        let gpa = GuestAddress(self.config.gpa);
        let region = Arc::new(
            GuestRegionMmap::new(mmap_region, gpa).ok_or(Error::GuestRegionNew(self.config.gpa))?,
        );

        let slot = *next_memory_slot;
        *next_memory_slot += 1;

        // SAFETY: `region` is a valid `MAP_SHARED` mapping of `PYLON_SHARED_PAGE_SIZE` bytes,
        // and the caller keeps the returned `Arc<GuestRegionMmap>` alive with the VM.
        unsafe {
            vm.create_user_memory_region(
                slot,
                region.start_addr().raw_value(),
                region.len() as usize,
                region.as_ptr(),
                false,
                false,
                MemoryVisibility::Shared,
            )
            .map_err(Error::CreateUserMemoryRegion)?;
        }

        info!(
            "Pylon: mapped 8192B shared memory page {:?} at GPA {:#x} (slot {})",
            self.config.shm, self.config.gpa, slot
        );

        Ok(region)
    }

    /// Returns a reference to the Pylon configuration.
    pub fn config(&self) -> &PylonConfig {
        &self.config
    }

    /// Returns the boot vCPU count configured for this VM.
    pub fn boot_vcpus(&self) -> u32 {
        self.boot_vcpus
    }

    /// Returns the maximum vCPU count configured for this VM.
    pub fn max_vcpus(&self) -> u32 {
        self.max_vcpus
    }

    /// Returns the number of initially active vCPUs at boot.
    pub fn initial_active_vcpus(&self) -> u32 {
        self.initial_active_vcpus
    }

    /// Returns a shared reference to the memory-mapped `PylonSharedPage`.
    pub fn shared_page(&self) -> &PylonSharedPage {
        // SAFETY: `self.shared_page` is valid for the lifetime of `self` and all mutable fields
        // within `PylonSharedPage` use interior atomic mutability.
        unsafe { self.shared_page.as_ref() }
    }

    /// Returns a reference to the `PylonVcpuSlot` for `vcpu_id`.
    pub fn vcpu_slot(&self, vcpu_id: u32) -> Option<&PylonVcpuSlot> {
        if vcpu_id < self.max_vcpus && (vcpu_id as usize) < MAX_VCPUS {
            Some(&self.shared_page().vcpus[vcpu_id as usize])
        } else {
            None
        }
    }

    /// Returns the wake `EventFd` for `vcpu_id`.
    pub fn wake_eventfd(&self, vcpu_id: u32) -> Option<&EventFd> {
        self.wake_eventfds.get(vcpu_id as usize)
    }

    /// Returns the kick `EventFd` for `vcpu_id`, which raises its `pylon-kick` MSI-X vector.
    pub fn kick_eventfd(&self, vcpu_id: u32) -> Option<&EventFd> {
        self.kick_eventfds.get(vcpu_id as usize)
    }

    /// Stores the Linux kernel thread ID (`gettid()`) into `PylonSharedPage.vcpus[vcpu_id].host_tid`.
    pub fn register_vcpu_tid(&self, vcpu_id: u32, tid: u32) {
        if let Some(slot) = self.vcpu_slot(vcpu_id) {
            slot.host_tid.store(tid, Ordering::Release);
        }
    }

    /// Called at the start of each vCPU thread (`vmm/src/cpu.rs`): records `gettid()` in
    /// `host_tid`, records the guest TSC offset measured with `read_guest_tsc` in
    /// `guest_tsc_offset`, and tags vCPUs with `vcpu_id >= initial_active_vcpus` as
    /// `park_on_boot`.
    ///
    /// Those vCPUs are not parked yet. A guest brings its secondary CPUs online at boot, and
    /// a vCPU whose thread never runs doesn't answer (aarch64's `PSCI CPU_ON`, x86_64's
    /// INIT/SIPI): the guest waits for each CPU, gives up, and boots without it. So they run
    /// until the guest reports it is up, and `pylond` then asks the VMM to park them
    /// (`check_and_park_vcpu`).
    pub fn on_vcpu_thread_start(&self, vcpu_id: u32, read_guest_tsc: impl FnOnce() -> Option<u64>) {
        // SAFETY: `libc::gettid()` is always safe to call in a Linux thread context.
        let tid = unsafe { libc::gettid() as u32 };
        self.register_vcpu_tid(vcpu_id, tid);

        let Some(slot) = self.vcpu_slot(vcpu_id) else {
            return;
        };

        if let Some(offset) = measure_guest_tsc_offset(read_guest_tsc) {
            slot.guest_tsc_offset.store(offset, Ordering::Release);
        }

        if vcpu_id >= self.initial_active_vcpus {
            slot.park_on_boot.store(1, Ordering::Release);
        }
        slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
    }

    /// Post-exit park handler called inside the vCPU run loop (`vmm/src/cpu.rs`) after every
    /// exit to userspace, with the vCPU lock dropped.
    ///
    /// Parks the vCPU when the guest asked to (`state == VCPU_STATE_PARKING`), or when `pylond`
    /// requested a revocation whose `revoke_deadline_tsc` has passed
    /// (`spec/06-guest-kick-path.md` §4.4). A vCPU tagged `park_on_boot` parks as soon as
    /// `pylond` requests it, whatever the deadline: the guest has just brought it online and it
    /// holds no lock. That park isn't a yield: it updates neither `yield_count` nor
    /// `forced_preempt_count`, and clears the tag. A pending revocation still inside its grace period
    /// doesn't park the vCPU: the guest hasn't agreed and may hold a lock. On parking, clears
    /// `revoke_requested`, updates the yield counters, publishes `VCPU_STATE_PARKED`, blocks until
    /// woken by `pylond` (or VM shutdown), and restores `VCPU_STATE_RUNNING`. Returns `true` if
    /// the vCPU parked and woke up.
    ///
    /// Before parking, publishes the guest's armed TSC-deadline timer (`read_guest_timer`, in
    /// guest TSC) as `park_timer_deadline_tsc` in host TSC, so `pylond` can give the vCPU a
    /// core back when the timer is due: a parked vCPU's timer interrupt can't fire.
    ///
    /// With `park_in_kvm`, the vCPU parks inside KVM instead (`spec/08-park-in-kvm.md`):
    /// `vcpu_op` sets it halted and this returns at once, so the run loop's next `KVM_RUN`
    /// sleeps in KVM until an interrupt or a signal. An interrupt runs the guest again without
    /// an exit; `state` stays `PARKED`. After each later exit, the vCPU becomes runnable and
    /// `RUNNING` only once `pylond` has written its wake `EventFd`.
    pub fn check_and_park_vcpu(
        &self,
        vcpu_id: u32,
        read_guest_timer: impl FnOnce() -> Option<u64>,
        mut vcpu_op: impl FnMut(KvmParkOp) -> bool,
    ) -> bool {
        let Some(slot) = self.vcpu_slot(vcpu_id) else {
            return false;
        };
        let parked_in_kvm = &self.parked_in_kvm[vcpu_id as usize];

        if parked_in_kvm.load(Ordering::Relaxed) {
            // Parked inside KVM: an exit (an interrupt-woken guest, or `pylond`'s signal) only
            // ends the park once `pylond` assigned a core (`spec/08-park-in-kvm.md` §3.3).
            if self.wake_eventfds[vcpu_id as usize].read().is_err() {
                return false;
            }
            if !vcpu_op(KvmParkOp::Run) {
                warn!("Pylon: failed to make parked vCPU {vcpu_id} runnable");
            }
            parked_in_kvm.store(false, Ordering::Relaxed);
            slot.park_timer_deadline_tsc.store(0, Ordering::Relaxed);
            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
            return true;
        }

        let revoke_requested = slot.revoke_requested.load(Ordering::Acquire);
        let decision = if slot.park_on_boot.load(Ordering::Acquire) != 0 && revoke_requested == 1 {
            ParkDecision::Boot
        } else {
            park_decision(
                slot.state.load(Ordering::Acquire),
                revoke_requested,
                // Ordered by the `revoke_requested` Acquire load: `pylond` stores the deadline
                // before setting `revoke_requested` with Release.
                slot.revoke_deadline_tsc.load(Ordering::Relaxed),
                read_host_tsc(),
            )
        };
        if decision == ParkDecision::Continue {
            return false;
        }
        // A halted x86 vCPU with interrupts disabled wouldn't wake for an IPI, so it doesn't
        // park yet; `pylond` signals again (`spec/08-park-in-kvm.md` §3.4). Halting happens
        // before anything is published, so a failure leaves the vCPU running as if it hadn't
        // been asked.
        if self.config.park_in_kvm
            && !(vcpu_op(KvmParkOp::InterruptsEnabled) && vcpu_op(KvmParkOp::Halt))
        {
            return false;
        }

        slot.revoke_requested.store(0, Ordering::Release);
        if decision == ParkDecision::Boot {
            slot.park_on_boot.store(0, Ordering::Release);
        } else {
            slot.yield_count.fetch_add(1, Ordering::Relaxed);
            slot.last_yield_tsc
                .store(read_host_tsc(), Ordering::Relaxed);
            if decision == ParkDecision::Forced {
                slot.forced_preempt_count.fetch_add(1, Ordering::Relaxed);
            }
        }
        let timer = guest_timer_to_host(
            read_guest_timer(),
            slot.guest_tsc_offset.load(Ordering::Relaxed),
        );
        slot.park_timer_deadline_tsc.store(timer, Ordering::Relaxed);
        slot.state.store(VCPU_STATE_PARKED, Ordering::Release);

        if self.config.park_in_kvm {
            parked_in_kvm.store(true, Ordering::Relaxed);
            return true;
        }

        self.park_vcpu(vcpu_id);

        slot.park_timer_deadline_tsc.store(0, Ordering::Relaxed);
        if !self.shutting_down.load(Ordering::Acquire) {
            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
        }

        true
    }

    /// Blocks the calling vCPU thread on its per-vCPU wake `EventFd` until signaled by `pylond`
    /// or until VM shutdown is initiated.
    pub fn park_vcpu(&self, vcpu_id: u32) {
        let Some(wake_evt) = self.wake_eventfds.get(vcpu_id as usize) else {
            return;
        };

        while !self.shutting_down.load(Ordering::Acquire) {
            match wake_evt.read() {
                Ok(_) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    /// Signals `wake_eventfds[vcpu_id]` to wake a parked vCPU thread.
    pub fn wake_vcpu(&self, vcpu_id: u32) -> result::Result<(), Error> {
        let wake_evt = self
            .wake_eventfds
            .get(vcpu_id as usize)
            .ok_or(Error::InvalidVcpuId(vcpu_id))?;
        wake_evt.write(1).map_err(Error::EventFdWrite)
    }

    /// Wakes all parked vCPU threads so they can exit cleanly on VM shutdown.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        for wake_evt in &self.wake_eventfds {
            let _ = wake_evt.write(1);
        }
    }
}

/// What `check_and_park_vcpu` asks of the vCPU when it parks inside KVM
/// (`spec/08-park-in-kvm.md`). The caller runs it with the vCPU locked and returns whether it
/// succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvmParkOp {
    /// Whether the guest can take an interrupt now (`RFLAGS.IF` on x86_64; always on aarch64,
    /// where `WFI` wakes on a masked interrupt).
    InterruptsEnabled,
    /// Make the vCPU halted (x86_64) or suspended (aarch64), so `KVM_RUN` sleeps until an
    /// interrupt or a signal.
    Halt,
    /// Make the vCPU runnable again.
    Run,
}

/// Runs `op` on `vcpu` (see `KvmParkOp`). Only KVM has the states it needs.
pub fn kvm_park_op(vcpu: &dyn Vcpu, op: KvmParkOp) -> bool {
    #[cfg(feature = "kvm")]
    {
        use hypervisor::MpState;
        use hypervisor::kvm::{kvm_bindings, kvm_mp_state};

        let mp_state = |mp_state| MpState::Kvm(kvm_mp_state { mp_state });
        match op {
            #[cfg(target_arch = "x86_64")]
            KvmParkOp::InterruptsEnabled => {
                const X86_EFLAGS_IF: u64 = 1 << 9;
                vcpu.get_regs()
                    .is_ok_and(|regs| regs.get_rflags() & X86_EFLAGS_IF != 0)
            }
            #[cfg(not(target_arch = "x86_64"))]
            KvmParkOp::InterruptsEnabled => true,
            #[cfg(target_arch = "x86_64")]
            KvmParkOp::Halt => vcpu
                .set_mp_state(mp_state(kvm_bindings::KVM_MP_STATE_HALTED))
                .is_ok(),
            #[cfg(not(target_arch = "x86_64"))]
            KvmParkOp::Halt => vcpu
                .set_mp_state(mp_state(kvm_bindings::KVM_MP_STATE_SUSPENDED))
                .is_ok(),
            KvmParkOp::Run => vcpu
                .set_mp_state(mp_state(kvm_bindings::KVM_MP_STATE_RUNNABLE))
                .is_ok(),
        }
    }
    #[cfg(not(feature = "kvm"))]
    {
        let _ = (vcpu, op);
        false
    }
}

/// Outcome of the post-exit park decision (`spec/06-guest-kick-path.md` §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParkDecision {
    /// Keep running the vCPU.
    Continue,
    /// The guest asked to park (`VCPU_STATE_PARKING`), with or without a revocation.
    Cooperative,
    /// A revocation's grace period ran out without the guest's consent.
    Forced,
    /// `pylond` asked for the initial park of a vCPU tagged `park_on_boot`.
    Boot,
}

/// Decides whether a vCPU that just exited to userspace parks, from its slot's `state`,
/// `revoke_requested` and `revoke_deadline_tsc`, and the current host TSC.
fn park_decision(state: u8, revoke_requested: u8, deadline_tsc: u64, now_tsc: u64) -> ParkDecision {
    if state == VCPU_STATE_PARKING {
        ParkDecision::Cooperative
    } else if revoke_requested == 1 && now_tsc >= deadline_tsc {
        ParkDecision::Forced
    } else {
        ParkDecision::Continue
    }
}

/// Reads the host counter, the clock `pylond` writes `revoke_deadline_tsc` in: the TSC on x86_64,
/// `CNTVCT_EL0` on aarch64.
fn read_host_tsc() -> u64 {
    read_counter()
}

/// Measures guest counter minus host counter (wrapping) by reading the guest counter between two
/// host counter reads. KVM computes a host-initiated read (`IA32_TSC` on x86_64,
/// `KVM_REG_ARM_TIMER_CNT` on aarch64) from the host counter at the time of the read, so the
/// error is about half the ioctl's duration. Returns `None` if the guest TSC
/// can't be read, or if the result is 0, which `guest_tsc_offset` reserves for "unknown".
fn measure_guest_tsc_offset(read_guest_tsc: impl FnOnce() -> Option<u64>) -> Option<u64> {
    let before = read_host_tsc();
    let guest = read_guest_tsc()?;
    let after = read_host_tsc();
    if before == 0 || after < before {
        return None;
    }
    let offset = guest.wrapping_sub(before + (after - before) / 2);
    (offset != 0).then_some(offset)
}

/// Converts a guest timer deadline (in the guest counter) to the host counter for
/// `park_timer_deadline_tsc`. Returns 0 if no timer is armed (0) or the offset is unknown.
fn guest_timer_to_host(guest_deadline: Option<u64>, guest_tsc_offset: u64) -> u64 {
    match guest_deadline {
        Some(deadline) if deadline != 0 && guest_tsc_offset != 0 => {
            // A host TSC of 0 means "no timer", so a deadline that maps exactly there is
            // moved by one cycle.
            deadline.wrapping_sub(guest_tsc_offset).max(1)
        }
        _ => 0,
    }
}

/// Reads one MSR of `vcpu`. Called from the vCPU's own thread, outside `KVM_RUN`.
#[cfg(target_arch = "x86_64")]
fn read_guest_msr(vcpu: &dyn Vcpu, index: u32) -> Option<u64> {
    let mut msrs = vec![MsrEntry { index, data: 0 }];
    match vcpu.get_msrs(&mut msrs) {
        Ok(1) => Some(msrs[0].data),
        _ => None,
    }
}

/// Reads the guest's TSC (`IA32_TSC`), for `PylonVmmState::on_vcpu_thread_start`.
#[cfg(target_arch = "x86_64")]
pub fn read_guest_tsc(vcpu: &dyn Vcpu) -> Option<u64> {
    read_guest_msr(vcpu, msr_index::MSR_IA32_TSC)
}

/// Reads the guest's armed TSC-deadline timer (`IA32_TSC_DEADLINE`, 0 if none), for
/// `PylonVmmState::check_and_park_vcpu`. Needs the in-kernel local APIC; without it the
/// read fails and no timer is published.
#[cfg(target_arch = "x86_64")]
pub fn read_guest_tsc_deadline(vcpu: &dyn Vcpu) -> Option<u64> {
    read_guest_msr(vcpu, msr_index::MSR_IA32_TSC_DEADLINE)
}

/// Reads the guest's virtual counter (`CNTVCT_EL0`), for `PylonVmmState::on_vcpu_thread_start`.
#[cfg(all(target_arch = "aarch64", feature = "kvm"))]
pub fn read_guest_tsc(vcpu: &dyn Vcpu) -> Option<u64> {
    vcpu.get_cntvct().ok()
}

/// The guest's armed EL1 virtual timer as a deadline in the guest counter, or 0 if none is armed,
/// from `CNTV_CTL_EL0` and `CNTV_CVAL_EL0`. The timer fires when `ENABLE` (bit 0) is set,
/// `IMASK` (bit 1) is clear and the counter reaches the compare value.
#[cfg(any(all(target_arch = "aarch64", feature = "kvm"), test))]
fn vtimer_deadline(ctl: u64, cval: u64) -> u64 {
    const ENABLE: u64 = 1 << 0;
    const IMASK: u64 = 1 << 1;
    if ctl & ENABLE != 0 && ctl & IMASK == 0 {
        cval
    } else {
        0
    }
}

/// Reads the guest's armed virtual timer (0 if none), for `PylonVmmState::check_and_park_vcpu`.
#[cfg(all(target_arch = "aarch64", feature = "kvm"))]
pub fn read_guest_tsc_deadline(vcpu: &dyn Vcpu) -> Option<u64> {
    let (ctl, cval) = vcpu.get_vtimer().ok()?;
    Some(vtimer_deadline(ctl, cval))
}

/// No guest counter is published where the hypervisor backend can't read it.
#[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", feature = "kvm"))))]
pub fn read_guest_tsc(_vcpu: &dyn Vcpu) -> Option<u64> {
    None
}

/// No guest timer is published where the hypervisor backend can't read it.
#[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", feature = "kvm"))))]
pub fn read_guest_tsc_deadline(_vcpu: &dyn Vcpu) -> Option<u64> {
    None
}

/// Hook for `CpuManager::new` (`vmm/src/cpu.rs`): rejects `--pylon` on a hypervisor backend that
/// can't support it, so it fails at start-up instead of silently never forcing a park. MSHV on
/// aarch64 can't read the guest's counter or timer (`spec/07-aarch64-support.md` §4.2).
pub fn check_hypervisor(hypervisor_type: HypervisorType) -> result::Result<(), Error> {
    #[cfg(feature = "mshv")]
    if cfg!(target_arch = "aarch64")
        && hypervisor_type == HypervisorType::Mshv
        && global_state().is_some()
    {
        return Err(Error::UnsupportedHypervisor);
    }
    let _ = hypervisor_type;
    Ok(())
}

/// Initializes and installs the process-global `PylonVmmState`.
pub fn configure_global(
    config: &PylonConfig,
    boot_vcpus: u32,
    max_vcpus: u32,
) -> result::Result<Arc<PylonVmmState>, Error> {
    let state = PylonVmmState::new(config, boot_vcpus, max_vcpus)?;
    let mut guard = GLOBAL_PYLON_STATE.lock().unwrap();
    if let Some(prev) = guard.take() {
        prev.shutdown();
    }
    *guard = Some(Arc::clone(&state));
    Ok(state)
}

/// Parses an optional `--pylon` argument string and installs the global `PylonVmmState`.
pub fn parse_and_configure(
    pylon_str: Option<&str>,
    boot_vcpus: u32,
    max_vcpus: u32,
) -> result::Result<Option<PylonConfig>, Error> {
    if let Some(s) = pylon_str {
        let cfg = PylonConfig::parse(s)?;
        configure_global(&cfg, boot_vcpus, max_vcpus)?;
        Ok(Some(cfg))
    } else {
        Ok(None)
    }
}

/// Returns the active global `PylonVmmState`, if `--pylon` was configured.
pub fn global_state() -> Option<Arc<PylonVmmState>> {
    GLOBAL_PYLON_STATE.lock().unwrap().as_ref().map(Arc::clone)
}

/// Clears and shuts down any installed global `PylonVmmState`.
pub fn clear_global_state() {
    if let Some(prev) = GLOBAL_PYLON_STATE.lock().unwrap().take() {
        prev.shutdown();
    }
}

/// Hook for `MemoryManager::new` (`vmm/src/memory_manager.rs`): maps the Pylon shared page
/// into guest physical memory if `--pylon` is active. The returned region must live as long
/// as the VM.
pub fn setup_memory_region(
    vm: &Arc<dyn Vm>,
    next_memory_slot: &mut u32,
) -> result::Result<Option<Arc<GuestRegionMmap>>, Error> {
    if let Some(state) = global_state() {
        let region = state.setup_gpa_mapping(vm, next_memory_slot)?;
        if state.config.park_in_kvm
            && let Err(e) = vm.set_halt_poll_ns(PARK_IN_KVM_HALT_POLL_NS)
        {
            warn!("Pylon: failed to turn halt polling off: {e}");
        }
        Ok(Some(region))
    } else {
        Ok(None)
    }
}

/// Hook for `CpuManager::start_vcpu` (`vmm/src/cpu.rs`): registers `gettid()`, measures the
/// guest TSC offset, and parks vCPUs with `vcpu_id >= initial_active_vcpus`.
pub fn on_vcpu_thread_start(vcpu_id: u32, read_guest_tsc: impl FnOnce() -> Option<u64>) {
    if let Some(state) = global_state() {
        state.on_vcpu_thread_start(vcpu_id, read_guest_tsc);
    }
}

/// Hook for the `vcpu.run()` exit loop (`vmm/src/cpu.rs`): parks the vCPU thread if the
/// guest asked to or a revocation's deadline passed. `read_guest_timer` is only called then,
/// and `vcpu_op` only when parking inside KVM.
pub fn check_and_park_vcpu(
    vcpu_id: u32,
    read_guest_timer: impl FnOnce() -> Option<u64>,
    vcpu_op: impl FnMut(KvmParkOp) -> bool,
) -> bool {
    if let Some(state) = global_state() {
        state.check_and_park_vcpu(vcpu_id, read_guest_timer, vcpu_op)
    } else {
        false
    }
}

/// Hook for `CpuManager::shutdown` (`vmm/src/cpu.rs`): wakes any Pylon-parked vCPU threads.
pub fn shutdown_all() {
    if let Some(state) = global_state() {
        state.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    use pylon_abi::VCPU_STATE_WAKING;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_pylon_config_parse_valid_and_defaults() {
        let cfg = PylonConfig::parse("shm=/dev/shm/vm1-pylon").unwrap();
        assert_eq!(cfg.shm, PathBuf::from("/dev/shm/vm1-pylon"));
        assert_eq!(cfg.socket, None);
        assert_eq!(cfg.gpa, PYLON_DEFAULT_GPA);
        assert_eq!(cfg.initial_active_vcpus, None);
        assert_eq!(cfg.min_cores, 1);
        assert_eq!(cfg.max_cores, None);
        assert_eq!(cfg.priority_weight, DEFAULT_PRIORITY_WEIGHT);
        assert_eq!(cfg.vm_id, 1);

        let full = PylonConfig::parse(
            "shm=/dev/shm/vm42-pylon,socket=/run/pylon/pylond.sock,gpa=0xfedf0000,\
             initial_active_vcpus=2,min_cores=1,max_cores=8,priority_weight=250,vm_id=42",
        )
        .unwrap();
        assert_eq!(full.shm, PathBuf::from("/dev/shm/vm42-pylon"));
        assert_eq!(full.socket, Some(PathBuf::from("/run/pylon/pylond.sock")));
        assert_eq!(full.gpa, 0xFEDF_0000);
        assert_eq!(full.initial_active_vcpus, Some(2));
        assert_eq!(full.min_cores, 1);
        assert_eq!(full.max_cores, Some(8));
        assert_eq!(full.priority_weight, 250);
        assert_eq!(full.vm_id, 42);
    }

    #[test]
    fn test_pylon_config_parse_errors() {
        // Missing shm
        assert!(matches!(
            PylonConfig::parse("socket=/run/pylon/pylond.sock"),
            Err(Error::MissingShmPath)
        ));
        // Empty shm
        assert!(matches!(
            PylonConfig::parse("shm="),
            Err(Error::MissingShmPath)
        ));
        // Invalid hex GPA
        assert!(matches!(
            PylonConfig::parse("shm=/dev/shm/test-pylon,gpa=0xnothex"),
            Err(Error::InvalidGpa(_))
        ));
        // Unaligned GPA
        assert!(matches!(
            PylonConfig::parse("shm=/dev/shm/test-pylon,gpa=0xfedf0123"),
            Err(Error::InvalidGpa(_))
        ));
        // Unknown option
        assert!(matches!(
            PylonConfig::parse("shm=/dev/shm/test-pylon,unknown_key=1"),
            Err(Error::OptionParse(_))
        ));
    }

    #[test]
    fn test_pylon_vmm_state_shared_page_init() {
        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-init-pylon");
        let cfg = PylonConfig::parse(&format!(
            "shm={},vm_id=77,initial_active_vcpus=2",
            shm_path.display()
        ))
        .unwrap();

        let state = PylonVmmState::new(&cfg, 2, 4).unwrap();
        let file_meta = fs::metadata(&shm_path).unwrap();
        assert_eq!(file_meta.len(), PYLON_SHARED_PAGE_SIZE as u64);

        let page = state.shared_page();
        assert_eq!(page.header.magic, PYLON_MAGIC);
        assert_eq!(page.header.version, PYLON_ABI_VERSION);
        assert_eq!(page.header.num_vcpus, 4);
        assert_eq!(page.header.vm_id, 77);
        assert_eq!(page.header.tsc_khz, UNKNOWN_TSC_KHZ);
        assert_eq!(page.telemetry.runnable_threads.load(Ordering::Relaxed), 0);

        for i in 0..4 {
            let slot = state.vcpu_slot(i).unwrap();
            assert_eq!(slot.state.load(Ordering::Relaxed), VCPU_STATE_PARKED);
            assert_eq!(slot.revoke_requested.load(Ordering::Relaxed), 0);
            assert_eq!(slot.host_tid.load(Ordering::Relaxed), 0);
            assert_eq!(slot.assigned_pcpu.load(Ordering::Relaxed), PCPU_UNASSIGNED);
        }
        assert!(state.vcpu_slot(4).is_none());
    }

    #[test]
    fn test_vcpu_thread_start_tid_and_boot_tag() {
        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-tid-pylon");
        let cfg = PylonConfig::parse(&format!(
            "shm={},initial_active_vcpus=1",
            shm_path.display()
        ))
        .unwrap();

        let state = PylonVmmState::new(&cfg, 1, 2).unwrap();

        // vCPU 0 is < initial_active_vcpus (1), so it should register TID and enter RUNNING immediately.
        let state_vcpu0 = Arc::clone(&state);
        let handle0 = thread::spawn(move || {
            state_vcpu0.on_vcpu_thread_start(0, || None);
        });
        handle0.join().unwrap();

        let slot0 = state.vcpu_slot(0).unwrap();
        assert_ne!(slot0.host_tid.load(Ordering::Acquire), 0);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);

        // vCPU 1 is >= initial_active_vcpus (1): it registers its thread and is tagged
        // `park_on_boot`, but keeps running, so a guest can bring it online.
        let state_vcpu1 = Arc::clone(&state);
        thread::spawn(move || state_vcpu1.on_vcpu_thread_start(1, || None))
            .join()
            .unwrap();
        let slot1 = state.vcpu_slot(1).unwrap();
        assert_ne!(slot1.host_tid.load(Ordering::Acquire), 0);
        assert_eq!(slot1.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        assert_eq!(slot1.park_on_boot.load(Ordering::Acquire), 1);
        assert_eq!(slot0.park_on_boot.load(Ordering::Acquire), 0);
    }

    /// The initial park of a vCPU tagged `park_on_boot` happens when `pylond` asks for it, at
    /// once and whatever the revocation deadline says, and counts as neither a yield nor a
    /// forced park. Until then the vCPU is left alone.
    #[test]
    fn test_boot_park_waits_for_the_request_and_isn_t_a_yield() {
        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-boot-park-pylon");
        let cfg = PylonConfig::parse(&format!(
            "shm={},initial_active_vcpus=1",
            shm_path.display()
        ))
        .unwrap();
        let state = PylonVmmState::new(&cfg, 2, 2).unwrap();
        state.on_vcpu_thread_start(1, || None);
        let slot1 = state.vcpu_slot(1).unwrap();

        // Nothing asked: an exit to userspace leaves it running.
        assert!(!state.check_and_park_vcpu(1, || None, |_| false));
        assert_eq!(slot1.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        assert_eq!(slot1.park_on_boot.load(Ordering::Acquire), 1);

        // Asked, with a deadline far in the future (a normal revocation would wait).
        slot1.revoke_deadline_tsc.store(u64::MAX, Ordering::Relaxed);
        slot1.revoke_requested.store(1, Ordering::Release);
        assert_eq!(
            park_with_timer_on_worker_and_wake(&state, 1, None),
            (true, 0)
        );
        assert_eq!(slot1.park_on_boot.load(Ordering::Acquire), 0);
        assert_eq!(slot1.yield_count.load(Ordering::Acquire), 0);
        assert_eq!(slot1.forced_preempt_count.load(Ordering::Acquire), 0);

        // The tag is gone: a later request is an ordinary revocation again, which waits for its
        // deadline.
        slot1.revoke_deadline_tsc.store(u64::MAX, Ordering::Relaxed);
        slot1.revoke_requested.store(1, Ordering::Release);
        assert!(!state.check_and_park_vcpu(1, || None, |_| false));
        assert_eq!(slot1.revoke_requested.load(Ordering::Acquire), 1);
    }

    #[test]
    fn test_park_decision_table() {
        const NOW: u64 = 1_000;
        // A guest that asked to park always parks cooperatively, revocation or not.
        for (revoke, deadline) in [(0, 0), (1, NOW + 1), (1, NOW)] {
            assert_eq!(
                park_decision(VCPU_STATE_PARKING, revoke, deadline, NOW),
                ParkDecision::Cooperative
            );
        }
        // A revocation forces a park only once its deadline has passed.
        assert_eq!(
            park_decision(VCPU_STATE_RUNNING, 1, NOW, NOW),
            ParkDecision::Forced
        );
        assert_eq!(
            park_decision(VCPU_STATE_RUNNING, 1, NOW - 1, NOW),
            ParkDecision::Forced
        );
        assert_eq!(
            park_decision(VCPU_STATE_RUNNING, 1, NOW + 1, NOW),
            ParkDecision::Continue
        );
        // No revocation and no request from the guest: keep running.
        assert_eq!(
            park_decision(VCPU_STATE_RUNNING, 0, 0, NOW),
            ParkDecision::Continue
        );
        assert_eq!(
            park_decision(VCPU_STATE_WAKING, 0, 0, NOW),
            ParkDecision::Continue
        );
    }

    /// Runs `check_and_park_vcpu(vcpu_id)` on a worker thread, waits until the vCPU is parked,
    /// wakes it, and returns whether it reported parking.
    fn park_on_worker_and_wake(state: &Arc<PylonVmmState>, vcpu_id: u32) -> bool {
        park_with_timer_on_worker_and_wake(state, vcpu_id, None).0
    }

    /// Like `park_on_worker_and_wake`, with `guest_timer` as the guest's TSC-deadline timer.
    /// Also returns the `park_timer_deadline_tsc` published while the vCPU was parked.
    fn park_with_timer_on_worker_and_wake(
        state: &Arc<PylonVmmState>,
        vcpu_id: u32,
        guest_timer: Option<u64>,
    ) -> (bool, u64) {
        let slot = state.vcpu_slot(vcpu_id).unwrap();
        let state_worker = Arc::clone(state);
        let worker = thread::spawn(move || {
            state_worker.check_and_park_vcpu(vcpu_id, || guest_timer, |_| false)
        });

        for _ in 0..200 {
            if slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(slot.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot.revoke_requested.load(Ordering::Acquire), 0);
        let timer = slot.park_timer_deadline_tsc.load(Ordering::Acquire);

        state.wake_vcpu(vcpu_id).unwrap();
        let parked = worker.join().unwrap();
        assert_eq!(slot.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        assert_eq!(slot.park_timer_deadline_tsc.load(Ordering::Acquire), 0);
        (parked, timer)
    }

    fn running_vcpu0_state(dir: &TempDir) -> Arc<PylonVmmState> {
        let shm_path = dir.path().join("vm-park-pylon");
        let cfg = PylonConfig::parse(&format!("shm={}", shm_path.display())).unwrap();
        let state = PylonVmmState::new(&cfg, 2, 2).unwrap();
        state.on_vcpu_thread_start(0, || None);
        assert_eq!(
            state.vcpu_slot(0).unwrap().state.load(Ordering::Acquire),
            VCPU_STATE_RUNNING
        );
        state
    }

    /// Like `running_vcpu0_state`, with vCPUs parking inside KVM.
    fn running_vcpu0_state_parking_in_kvm(dir: &TempDir) -> Arc<PylonVmmState> {
        let shm_path = dir.path().join("vm-park-kvm-pylon");
        let mut cfg = PylonConfig::parse(&format!("shm={}", shm_path.display())).unwrap();
        cfg.park_in_kvm = true;
        let state = PylonVmmState::new(&cfg, 2, 2).unwrap();
        state.on_vcpu_thread_start(0, || None);
        state
    }

    #[test]
    fn test_park_in_kvm_halts_and_runs_again_only_when_woken() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state_parking_in_kvm(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();
        let mut ops = Vec::new();

        // A forced park halts the vCPU and returns at once, without blocking.
        slot0.revoke_deadline_tsc.store(0, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);
        assert!(state.check_and_park_vcpu(
            0,
            || None,
            |op| {
                ops.push(op);
                true
            }
        ));
        assert_eq!(ops, [KvmParkOp::InterruptsEnabled, KvmParkOp::Halt]);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 0);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Relaxed), 1);

        // An exit while parked, from an interrupt-woken guest or a stray signal, changes nothing.
        ops.clear();
        assert!(!state.check_and_park_vcpu(
            0,
            || None,
            |op| {
                ops.push(op);
                true
            }
        ));
        assert!(ops.is_empty());
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);

        // Once pylond has assigned a core, the next exit makes the vCPU runnable.
        state.wake_vcpu(0).unwrap();
        assert!(state.check_and_park_vcpu(
            0,
            || None,
            |op| {
                ops.push(op);
                true
            }
        ));
        assert_eq!(ops, [KvmParkOp::Run]);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_park_in_kvm_waits_while_interrupts_are_off_or_halting_fails() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state_parking_in_kvm(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();
        slot0.revoke_deadline_tsc.store(0, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);

        for refused in [KvmParkOp::InterruptsEnabled, KvmParkOp::Halt] {
            assert!(!state.check_and_park_vcpu(0, || None, |op| op != refused));
            assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
            assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 1);
            assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn test_revoke_within_grace_period_does_not_park() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();

        // Without a revocation or a request from the guest, nothing happens.
        assert!(!state.check_and_park_vcpu(0, || None, |_| false));

        // A revocation still inside its grace period leaves the vCPU running and the
        // request pending.
        slot0.revoke_deadline_tsc.store(u64::MAX, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);
        assert!(!state.check_and_park_vcpu(0, || None, |_| false));
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 1);
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_cooperative_park_and_wake() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();

        // The guest agrees to a pending revocation by writing VCPU_STATE_PARKING.
        slot0.revoke_deadline_tsc.store(u64::MAX, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);
        slot0.state.store(VCPU_STATE_PARKING, Ordering::Release);

        assert!(park_on_worker_and_wake(&state, 0));
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 1);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Relaxed), 0);

        // A guest may also give a core back with no revocation pending.
        slot0.state.store(VCPU_STATE_PARKING, Ordering::Release);
        assert!(park_on_worker_and_wake(&state, 0));
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 2);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_forced_park_after_deadline() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();

        // A deadline of 0 has always passed.
        slot0.revoke_deadline_tsc.store(0, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);

        assert!(park_on_worker_and_wake(&state, 0));
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 1);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_park_publishes_guest_timer_in_host_tsc() {
        let dir = TempDir::new().unwrap();
        let state = running_vcpu0_state(&dir);
        let slot0 = state.vcpu_slot(0).unwrap();

        // Without a measured offset, a guest timer can't be placed on the host's clock.
        slot0.state.store(VCPU_STATE_PARKING, Ordering::Release);
        assert_eq!(
            park_with_timer_on_worker_and_wake(&state, 0, Some(5_000)),
            (true, 0)
        );

        // The guest TSC started when the host TSC was at 1,000,000.
        slot0
            .guest_tsc_offset
            .store(0u64.wrapping_sub(1_000_000), Ordering::Release);
        slot0.state.store(VCPU_STATE_PARKING, Ordering::Release);
        assert_eq!(
            park_with_timer_on_worker_and_wake(&state, 0, Some(5_000)),
            (true, 1_005_000)
        );

        // No armed timer (the MSR reads 0), or the MSR couldn't be read.
        for guest_timer in [Some(0), None] {
            slot0.state.store(VCPU_STATE_PARKING, Ordering::Release);
            assert_eq!(
                park_with_timer_on_worker_and_wake(&state, 0, guest_timer),
                (true, 0)
            );
        }
    }

    #[test]
    fn test_measure_guest_tsc_offset() {
        assert_eq!(measure_guest_tsc_offset(|| None), None);

        // A guest TSC 12,345 cycles ahead of the host's.
        let offset = measure_guest_tsc_offset(|| Some(read_host_tsc() + 12_345)).unwrap();
        let error = offset.wrapping_sub(12_345) as i64;
        assert!(error.abs() < 10_000_000, "offset error {error} cycles");

        // A guest TSC that started at 0 when the host TSC was well past it.
        let offset = measure_guest_tsc_offset(|| Some(0)).unwrap();
        assert!((offset as i64) < 0);
    }

    #[test]
    fn test_vtimer_deadline_needs_enable_and_no_mask() {
        // ENABLE set, IMASK clear: the timer will fire at CVAL. ISTATUS (bit 2) doesn't matter.
        assert_eq!(vtimer_deadline(0b001, 777), 777);
        assert_eq!(vtimer_deadline(0b101, 777), 777);
        // Disabled, or masked: no interrupt will come, so nothing to publish.
        assert_eq!(vtimer_deadline(0b000, 777), 0);
        assert_eq!(vtimer_deadline(0b011, 777), 0);
        assert_eq!(vtimer_deadline(0b010, 777), 0);
        // The guest's compare value is 0 when the timer is armed for the epoch.
        assert_eq!(vtimer_deadline(0b001, 0), 0);
    }

    #[test]
    fn test_daemon_socket_registration_scm_rights() {
        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-sock-pylon");
        let sock_path = dir.path().join("pylond.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let server_thread = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let mut iov = [libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            }];
            let mut fds = [0i32; 8];
            // SAFETY: `iov` points to a valid mutable stack buffer `buf` and `fds` has space for 8 FDs.
            let (bytes_read, fds_read) =
                unsafe { stream.recv_with_fds(&mut iov, &mut fds) }.unwrap();
            for &fd in &fds[..fds_read] {
                // SAFETY: Closing file descriptors received via SCM_RIGHTS in this test.
                unsafe {
                    libc::close(fd);
                }
            }
            let msg: DaemonRegistrationPayload =
                serde_json::from_slice(&buf[..bytes_read]).unwrap();
            (msg, fds_read)
        });

        let cfg = PylonConfig::parse(&format!(
            "shm={},socket={},vm_id=99,min_cores=1,max_cores=2,priority_weight=150",
            shm_path.display(),
            sock_path.display()
        ))
        .unwrap();

        let _state = PylonVmmState::new(&cfg, 2, 2).unwrap();
        let (msg, fds_read) = server_thread.join().unwrap();
        assert_eq!(msg.vm_id, 99);
        assert_eq!(msg.num_vcpus, 2);
        assert_eq!(msg.min_cores, 1);
        assert_eq!(msg.max_cores, 2);
        assert_eq!(msg.priority_weight, 150);
        assert_eq!(msg.kick_signal, SIGRTMIN());
        // 1 shm_fd + 2 kick_eventfds + 2 wake_eventfds = 5 FDs
        assert_eq!(fds_read, 5);
    }

    /// The guest counter read from a real vCPU converts to the host counter within a small
    /// error, and an unarmed virtual timer publishes no deadline.
    #[cfg(all(target_arch = "aarch64", feature = "kvm"))]
    #[test]
    fn test_real_kvm_vcpu_counter_offset_and_idle_timer() {
        use hypervisor::HypervisorVmConfig;

        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-kvm-counter-pylon");
        let cfg = PylonConfig::parse(&format!(
            "shm={},gpa=0x8000,vm_id=44,initial_active_vcpus=1",
            shm_path.display()
        ))
        .unwrap();
        let state = PylonVmmState::new(&cfg, 1, 1).unwrap();

        let Ok(hv) = hypervisor::new() else {
            eprintln!("Skipping real KVM VM test because /dev/kvm is not accessible");
            return;
        };
        let vm = hv
            .create_vm(HypervisorVmConfig::default())
            .expect("failed to create KVM VM");
        let vcpu = vm
            .create_vcpu(0, None)
            .expect("failed to create KVM vCPU 0");
        let mut kvi = vcpu.create_vcpu_init();
        vm.get_preferred_target(&mut kvi).expect("preferred target");
        vcpu.vcpu_init(&kvi).expect("vcpu init");

        state.on_vcpu_thread_start(0, || read_guest_tsc(vcpu.as_ref()));

        let guest_now = read_guest_tsc(vcpu.as_ref()).expect("read guest counter");
        let host_now = read_host_tsc();
        let converted = state.shared_page().guest_to_host_tsc(guest_now);
        let error = host_now.wrapping_sub(converted) as i64;
        // Counter ticks (1 GHz on this class of hardware): a few ioctls' worth of error is fine,
        // a missing offset would be off by the VM's counter offset.
        assert!(
            error.abs() < 100_000_000,
            "converted guest counter off by {error} ticks"
        );
        assert_eq!(read_guest_tsc_deadline(vcpu.as_ref()), Some(0));
    }

    #[cfg(all(target_arch = "x86_64", feature = "kvm"))]
    #[test]
    fn test_e2e_real_kvm_vm_forced_park_on_kick_signal() {
        use hypervisor::{HypervisorVmConfig, VmExit};
        use vm_memory::{Bytes, GuestMemoryBackend};
        use vmm_sys_util::signal::register_signal_handler;

        use crate::GuestMemoryMmap;

        extern "C" fn handle_signal(_: i32, _: *mut libc::siginfo_t, _: *mut libc::c_void) {}

        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-kvm-forced-pylon");
        let cfg = PylonConfig::parse(&format!(
            "shm={},gpa=0x8000,vm_id=43,initial_active_vcpus=1",
            shm_path.display()
        ))
        .unwrap();
        let state = PylonVmmState::new(&cfg, 1, 1).unwrap();

        let Ok(hv) = hypervisor::new() else {
            eprintln!("Skipping real KVM VM test because /dev/kvm is not accessible");
            return;
        };
        let vm = hv
            .create_vm(HypervisorVmConfig::default())
            .expect("failed to create KVM VM");

        let code_gpa = GuestAddress(0x1000);
        let guest_mem = GuestMemoryMmap::from_ranges(&[(code_gpa, 0x1000)]).unwrap();
        for (index, region) in guest_mem.iter().enumerate() {
            // SAFETY: `region` is a valid GuestRegionMmap backed by anonymous memory.
            unsafe {
                vm.create_user_memory_region(
                    index as u32,
                    region.start_addr().raw_value(),
                    region.len().try_into().unwrap(),
                    region.as_ptr(),
                    false,
                    false,
                    MemoryVisibility::Shared,
                )
                .expect("failed to register guest code memory region");
            }
        }
        let mut next_slot = 1u32;
        let _pylon_region = state
            .setup_gpa_mapping(&vm, &mut next_slot)
            .expect("failed to map Pylon shared page");

        // A guest that never agrees to park: `jmp $` at GPA 0x1000.
        guest_mem
            .write_slice(&[0xEB, 0xFE], code_gpa)
            .expect("failed to write guest code");

        let state_vcpu = Arc::clone(&state);
        let vcpu_thread = thread::spawn(move || {
            // As in `vmm/src/cpu.rs`: a no-op handler, so the signal only interrupts KVM_RUN.
            register_signal_handler(SIGRTMIN(), handle_signal).unwrap();
            let mut vcpu = vm
                .create_vcpu(0, None, Default::default())
                .expect("failed to create KVM vCPU 0");
            let mut sregs = vcpu.get_sregs().expect("get_sregs");
            sregs.cs.base = 0;
            sregs.cs.selector = 0;
            vcpu.set_sregs(&sregs).expect("set_sregs");
            let mut regs = vcpu.get_regs().expect("get_regs");
            regs.set_rip(0x1000);
            regs.set_rflags(0x2);
            vcpu.set_regs(&regs).expect("set_regs");

            state_vcpu.on_vcpu_thread_start(0, || read_guest_tsc(vcpu.as_ref()));

            // The measured offset puts the guest's TSC on the host's clock. Unconverted, it
            // would be off by the host's uptime: KVM starts a new vCPU's TSC at 0.
            let guest_now = read_guest_tsc(vcpu.as_ref()).expect("read guest TSC");
            let host_now = read_host_tsc();
            let converted = state_vcpu.shared_page().guest_to_host_tsc(guest_now);
            let error = host_now.wrapping_sub(converted) as i64;
            assert!(
                error.abs() < 100_000_000,
                "converted guest TSC off by {error} cycles"
            );

            loop {
                // The kick signal makes KVM_RUN fail with EINTR, which the KVM backend
                // reports as `VmExit::Ignore`.
                match vcpu.run().expect("vcpu.run failed") {
                    VmExit::Ignore => {}
                    other => panic!("unexpected VM exit: {other:?}"),
                }
                if state_vcpu.check_and_park_vcpu(
                    0,
                    || read_guest_tsc_deadline(vcpu.as_ref()),
                    |_| false,
                ) {
                    break;
                }
            }
        });

        let slot0 = state.vcpu_slot(0).unwrap();
        for _ in 0..500 {
            if slot0.host_tid.load(Ordering::Acquire) != 0
                && slot0.state.load(Ordering::Acquire) == VCPU_STATE_RUNNING
            {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let tid = slot0.host_tid.load(Ordering::Acquire);
        assert_ne!(tid, 0);

        // Do what pylond does once a revocation's deadline has passed: the request is pending,
        // its deadline (0) is over, and the vCPU thread gets the kick signal. Resend it, as
        // pylond does, in case one lands while the thread is outside KVM_RUN.
        slot0.revoke_deadline_tsc.store(0, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);
        for _ in 0..500 {
            if slot0.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                break;
            }
            // SAFETY: `tgkill` signals thread `tid` of this process; the vCPU thread has a
            // handler installed for `SIGRTMIN`.
            unsafe {
                libc::syscall(
                    libc::SYS_tgkill,
                    libc::getpid(),
                    tid as libc::pid_t,
                    SIGRTMIN(),
                );
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 0);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Acquire), 1);
        assert_eq!(slot0.yield_count.load(Ordering::Acquire), 1);

        state.wake_vcpu(0).unwrap();
        vcpu_thread.join().expect("KVM vCPU thread panicked");
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
    }

    /// A real KVM VM with one vCPU whose guest spins (`b .`) and never agrees to park, so the
    /// vCPU thread parks only when `check_and_park_vcpu` says so. The thread runs until `stop`.
    #[cfg(all(target_arch = "aarch64", feature = "kvm"))]
    struct SpinGuest {
        state: Arc<PylonVmmState>,
        stop: Arc<AtomicBool>,
        thread: thread::JoinHandle<()>,
        tid: libc::pid_t,
        _pylon_region: Arc<GuestRegionMmap>,
        _dir: TempDir,
    }

    #[cfg(all(target_arch = "aarch64", feature = "kvm"))]
    impl SpinGuest {
        /// `None` if `/dev/kvm` isn't usable.
        fn start(vm_id: u64) -> Option<Self> {
            use hypervisor::{HypervisorVmConfig, VmExit};
            use vm_memory::{Bytes, GuestMemoryBackend};
            use vmm_sys_util::signal::register_signal_handler;

            use crate::GuestMemoryMmap;

            extern "C" fn handle_signal(_: i32, _: *mut libc::siginfo_t, _: *mut libc::c_void) {}

            let hv = hypervisor::new().ok()?;
            let dir = TempDir::new().unwrap();
            let shm_path = dir.path().join(format!("vm-kvm-spin-{vm_id}-pylon"));
            let cfg = PylonConfig::parse(&format!(
                "shm={},gpa=0x8000,vm_id={vm_id},initial_active_vcpus=1",
                shm_path.display()
            ))
            .unwrap();
            let state = PylonVmmState::new(&cfg, 1, 1).unwrap();
            let vm = hv
                .create_vm(HypervisorVmConfig::default())
                .expect("failed to create KVM VM");

            // `b .`, the branch to itself, at the start of one page of guest memory.
            let code_gpa = GuestAddress(0x4000_0000);
            let guest_mem = GuestMemoryMmap::from_ranges(&[(code_gpa, 0x1000)]).unwrap();
            for (index, region) in guest_mem.iter().enumerate() {
                // SAFETY: `region` is a valid GuestRegionMmap backed by anonymous memory, which
                // the vCPU thread keeps alive by owning `guest_mem`.
                unsafe {
                    vm.create_user_memory_region(
                        index as u32,
                        region.start_addr().raw_value(),
                        region.len().try_into().unwrap(),
                        region.as_ptr(),
                        false,
                        false,
                        MemoryVisibility::Shared,
                    )
                    .expect("failed to register guest code memory region");
                }
            }
            let mut next_slot = 1u32;
            let pylon_region = state
                .setup_gpa_mapping(&vm, &mut next_slot)
                .expect("failed to map Pylon shared page");
            guest_mem
                .write_slice(&0x1400_0000u32.to_le_bytes(), code_gpa)
                .expect("failed to write guest code");

            let stop = Arc::new(AtomicBool::new(false));
            let (state_vcpu, stop_vcpu) = (Arc::clone(&state), Arc::clone(&stop));
            let thread = thread::spawn(move || {
                // As in `vmm/src/cpu.rs`: a no-op handler, so the signal only interrupts KVM_RUN.
                register_signal_handler(SIGRTMIN(), handle_signal).unwrap();
                let _guest_mem = guest_mem;
                let mut vcpu = vm
                    .create_vcpu(0, None)
                    .expect("failed to create KVM vCPU 0");
                let mut kvi = vcpu.create_vcpu_init();
                vm.get_preferred_target(&mut kvi).expect("preferred target");
                vcpu.vcpu_init(&kvi).expect("vcpu init");
                vcpu.setup_regs(0, code_gpa.raw_value(), 0, false)
                    .expect("setup_regs");

                state_vcpu.on_vcpu_thread_start(0, || read_guest_tsc(vcpu.as_ref()));
                while !stop_vcpu.load(Ordering::Acquire) {
                    // The kick signal makes KVM_RUN fail with EINTR, which the KVM backend
                    // reports as `VmExit::Ignore`.
                    match vcpu.run().expect("vcpu.run failed") {
                        VmExit::Ignore => {}
                        other => panic!("unexpected VM exit: {other:?}"),
                    }
                    state_vcpu.check_and_park_vcpu(
                        0,
                        || read_guest_tsc_deadline(vcpu.as_ref()),
                        |_| false,
                    );
                }
            });

            let slot0 = state.vcpu_slot(0).unwrap();
            for _ in 0..500 {
                if slot0.host_tid.load(Ordering::Acquire) != 0
                    && slot0.state.load(Ordering::Acquire) == VCPU_STATE_RUNNING
                {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            let tid = slot0.host_tid.load(Ordering::Acquire) as libc::pid_t;
            assert_ne!(tid, 0);
            Some(Self {
                state,
                stop,
                thread,
                tid,
                _pylon_region: pylon_region,
                _dir: dir,
            })
        }

        /// Sends the kick signal, as `pylond` does for a revocation.
        fn kick(&self) {
            // SAFETY: `tgkill` signals thread `tid` of this process; the vCPU thread has a
            // handler installed for `SIGRTMIN`.
            unsafe {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), self.tid, SIGRTMIN());
            }
        }

        /// Requests a revocation whose grace period is over, as `pylond` does.
        fn request_forced_revocation(&self) {
            let slot0 = self.state.vcpu_slot(0).unwrap();
            slot0.revoke_deadline_tsc.store(0, Ordering::Relaxed);
            slot0.revoke_requested.store(1, Ordering::Release);
        }

        /// Stops the vCPU thread and waits for it.
        fn stop(self) {
            self.stop.store(true, Ordering::Release);
            // Kick it out of KVM_RUN, or wake it if it is parked.
            for _ in 0..500 {
                if self.thread.is_finished() {
                    break;
                }
                self.kick();
                let _ = self.state.wake_vcpu(0);
                thread::sleep(Duration::from_millis(2));
            }
            self.thread.join().expect("KVM vCPU thread panicked");
        }
    }

    /// An aarch64 guest that never agrees to park is parked by the kick signal once the grace
    /// period is over: the same path as the x86_64 test below, on a real aarch64 KVM vCPU.
    #[cfg(all(target_arch = "aarch64", feature = "kvm"))]
    #[test]
    fn test_e2e_real_kvm_vcpu_aarch64_forced_park_on_kick_signal() {
        let Some(guest) = SpinGuest::start(45) else {
            eprintln!("Skipping real KVM VM test because /dev/kvm is not accessible");
            return;
        };
        let slot0 = guest.state.vcpu_slot(0).unwrap();

        // Resend the signal, as pylond does, in case one lands while the thread is outside
        // KVM_RUN.
        guest.request_forced_revocation();
        for _ in 0..500 {
            if slot0.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                break;
            }
            guest.kick();
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 0);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Acquire), 1);
        assert_eq!(slot0.yield_count.load(Ordering::Acquire), 1);

        guest.state.wake_vcpu(0).unwrap();
        for _ in 0..500 {
            if slot0.state.load(Ordering::Acquire) == VCPU_STATE_RUNNING {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
        guest.stop();
    }

    /// Latency of the forced-park path on a real aarch64 KVM vCPU: from the revocation request
    /// (and first kick signal) until `PARKED`, and from the wake to `RUNNING` again. Run with
    /// `cargo test -p vmm --features kvm --lib bench_forced_park -- --ignored --nocapture`.
    #[cfg(all(target_arch = "aarch64", feature = "kvm"))]
    #[test]
    #[ignore = "a benchmark: prints latencies"]
    fn bench_forced_park_latency_real_kvm_aarch64() {
        use core::hint::spin_loop;

        use pylon_abi::clock::architectural_counter_khz;

        const ITERATIONS: usize = 3000;
        // As pylond: re-send the kick this often (`REVOKE_RESIGNAL_US`).
        const RESIGNAL_US: u64 = 10;

        let Some(guest) = SpinGuest::start(46) else {
            eprintln!("Skipping real KVM VM test because /dev/kvm is not accessible");
            return;
        };
        let khz = architectural_counter_khz().expect("CNTFRQ_EL0 is set");
        let to_ns = |ticks: u64| ticks.saturating_mul(1_000_000) / khz;
        let resignal = RESIGNAL_US * khz / 1000;
        let slot0 = guest.state.vcpu_slot(0).unwrap();

        let (mut park_ns, mut wake_ns, mut resignals) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..ITERATIONS {
            let start = read_host_tsc();
            guest.request_forced_revocation();
            guest.kick();
            let (mut last_signal, mut signals) = (start, 1u32);
            while slot0.state.load(Ordering::Acquire) != VCPU_STATE_PARKED {
                let now = read_host_tsc();
                assert!(now - start < khz * 1000, "no park within 1 s");
                if now - last_signal >= resignal {
                    guest.kick();
                    last_signal = now;
                    signals += 1;
                }
                spin_loop();
            }
            park_ns.push(to_ns(read_host_tsc() - start));
            resignals.push(signals);

            // Let it settle in the park before waking, as a core handoff would.
            thread::sleep(Duration::from_micros(200));
            let wake_start = read_host_tsc();
            guest.state.wake_vcpu(0).unwrap();
            while slot0.state.load(Ordering::Acquire) != VCPU_STATE_RUNNING {
                assert!(
                    read_host_tsc() - wake_start < khz * 1000,
                    "no wake within 1 s"
                );
                spin_loop();
            }
            wake_ns.push(to_ns(read_host_tsc() - wake_start));
            // Let the vCPU get back into KVM_RUN.
            thread::sleep(Duration::from_micros(200));
        }
        guest.stop();

        let report = |name: &str, mut v: Vec<u64>| {
            v.sort_unstable();
            let mean = v.iter().sum::<u64>() as f64 / v.len() as f64 / 1000.0;
            let at = |q: usize| v[(v.len() * q / 100).min(v.len() - 1)] as f64 / 1000.0;
            eprintln!(
                "{name}: n={} mean={mean:.1} us p50={:.1} p90={:.1} p99={:.1} max={:.1}",
                v.len(),
                at(50),
                at(90),
                at(99),
                *v.last().unwrap() as f64 / 1000.0
            );
        };
        report("forced park (request -> PARKED)", park_ns);
        report("wake (wake_vcpu -> RUNNING)     ", wake_ns);
        let quick = resignals.iter().filter(|&&n| n == 1).count();
        eprintln!(
            "parked within {RESIGNAL_US} us, before any re-signal: {quick}/{ITERATIONS}; \
             most signals sent for one park: {}",
            resignals.iter().max().unwrap()
        );
    }

    #[cfg(all(target_arch = "x86_64", feature = "kvm"))]
    #[test]
    fn test_e2e_real_kvm_vm_pylon_shared_gpa_and_cooperative_park() {
        use hypervisor::{HypervisorVmConfig, VmExit};
        use vm_memory::{Bytes, GuestMemoryBackend};

        use crate::GuestMemoryMmap;

        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-kvm-e2e-pylon");
        // Map Pylon shared page at GPA 0x8000 so 16-bit real-mode guest code can address it directly.
        let cfg = PylonConfig::parse(&format!(
            "shm={},gpa=0x8000,vm_id=42,initial_active_vcpus=1",
            shm_path.display()
        ))
        .unwrap();
        let state = PylonVmmState::new(&cfg, 1, 1).unwrap();

        let Ok(hv) = hypervisor::new() else {
            eprintln!("Skipping real KVM VM test because /dev/kvm is not accessible");
            return;
        };
        let vm = hv
            .create_vm(HypervisorVmConfig::default())
            .expect("failed to create KVM VM");

        let code_gpa = GuestAddress(0x1000);
        let guest_mem = GuestMemoryMmap::from_ranges(&[(code_gpa, 0x2000)]).unwrap();
        for (index, region) in guest_mem.iter().enumerate() {
            // SAFETY: `region` is a valid GuestRegionMmap backed by anonymous memory.
            unsafe {
                vm.create_user_memory_region(
                    index as u32,
                    region.start_addr().raw_value(),
                    region.len().try_into().unwrap(),
                    region.as_ptr(),
                    false,
                    false,
                    MemoryVisibility::Shared,
                )
                .expect("failed to register guest code memory region");
            }
        }

        let mut next_slot = 1u32;
        let _pylon_region = state
            .setup_gpa_mapping(&vm, &mut next_slot)
            .expect("failed to map Pylon shared page into KVM guest GPA 0x8000");
        assert_eq!(next_slot, 2);
        // The page has a memory slot of its own and stays out of guest RAM (K4).
        assert!(guest_mem.find_region(GuestAddress(0x8000)).is_none());

        // 16-bit x86 guest code at GPA 0x1000 interacting with PylonSharedPage at GPA 0x8000:
        // - 0x8000: PylonHeader.magic (low 16 bits == 0x4D31, "1M")
        // - 0x8040: PylonTelemetry.runnable_threads (u32)
        // - 0x8060: PylonTelemetry.guest_heartbeat (u64)
        // - 0x8080: PylonVcpuSlot[0].state (u8, offset 128 = 0x80)
        // - 0x80A0: PylonVcpuSlot[0].revoke_requested (u8, offset 128 + 32 = 160 = 0xA0)
        // Without an in-kernel irqchip, HLT exits to userspace; here it stands in for the
        // park doorbell of spec/06-guest-kick-path.md §4.3.
        let guest_code: [u8; 37] = [
            0xA1, 0x00, 0x80, // mov ax, [0x8000]
            0xC7, 0x06, 0x40, 0x80, 0x03, 0x00, // mov word ptr [0x8040], 3
            // loop (0x1009):
            0xFF, 0x06, 0x60, 0x80, // inc word ptr [0x8060]
            0x80, 0x3E, 0xA0, 0x80, 0x01, // cmp byte ptr [0x80A0], 1
            0x75, 0xF5, // jne 0x1009
            0xC6, 0x06, 0x80, 0x80, 0x02, // mov byte ptr [0x8080], 2 (VCPU_STATE_PARKING)
            0xF4, // hlt (1st exit -> VMM parks cooperatively)
            // after wakeup (0x101A):
            0xFF, 0x06, 0x60, 0x80, // inc word ptr [0x8060]
            0xC7, 0x06, 0x40, 0x80, 0x07, 0x00, // mov word ptr [0x8040], 7
            0xF4, // hlt (2nd exit -> test complete)
        ];
        guest_mem
            .write_slice(&guest_code, code_gpa)
            .expect("failed to write guest code");

        let state_vcpu = Arc::clone(&state);
        let vcpu_thread = thread::spawn(move || {
            let mut vcpu = vm
                .create_vcpu(0, None, Default::default())
                .expect("failed to create KVM vCPU 0");

            let mut sregs = vcpu.get_sregs().expect("get_sregs");
            sregs.cs.base = 0;
            sregs.cs.selector = 0;
            sregs.ds.base = 0;
            sregs.ds.selector = 0;
            vcpu.set_sregs(&sregs).expect("set_sregs");

            let mut regs = vcpu.get_regs().expect("get_regs");
            regs.set_rip(0x1000);
            regs.set_rflags(0x2);
            vcpu.set_regs(&regs).expect("set_regs");

            state_vcpu.on_vcpu_thread_start(0, || read_guest_tsc(vcpu.as_ref()));

            // First KVM_RUN: guest updates telemetry and spins until host sets revoke_requested = 1,
            // then agrees with VCPU_STATE_PARKING and executes HLT.
            match vcpu.run().expect("1st vcpu.run failed") {
                VmExit::Reset | VmExit::Ignore => {}
                other => panic!("unexpected 1st VM exit: {other:?}"),
            }

            // Post-exit hook: sees VCPU_STATE_PARKING, parks in VCPU_STATE_PARKED until woken via wake_eventfd.
            assert!(state_vcpu.check_and_park_vcpu(
                0,
                || read_guest_tsc_deadline(vcpu.as_ref()),
                |_| false
            ));

            // Second KVM_RUN after host wakeup: guest writes runnable_threads = 7 and executes final HLT.
            match vcpu.run().expect("2nd vcpu.run failed") {
                VmExit::Reset | VmExit::Ignore => {}
                other => panic!("unexpected 2nd VM exit: {other:?}"),
            }

            vcpu.get_regs().expect("get_regs after 2nd run").get_rax()
        });

        let page = state.shared_page();
        let slot0 = state.vcpu_slot(0).unwrap();

        // 1. Wait for the KVM guest to write runnable_threads = 3 and increment guest_heartbeat via GPA 0x8000.
        for _ in 0..200 {
            if page.telemetry.runnable_threads.load(Ordering::Acquire) == 3
                && page.telemetry.guest_heartbeat.load(Ordering::Acquire) > 0
            {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(page.telemetry.runnable_threads.load(Ordering::Acquire), 3);
        assert!(page.telemetry.guest_heartbeat.load(Ordering::Acquire) > 0);
        assert_ne!(slot0.host_tid.load(Ordering::Acquire), 0);

        // 2. Request a revocation with a deadline that never passes, so only the guest's consent can
        //    park the vCPU: the guest sees [0x80A0] == 1, writes PARKING, executes HLT, and the VMM parks vCPU 0.
        slot0.revoke_deadline_tsc.store(u64::MAX, Ordering::Relaxed);
        slot0.revoke_requested.store(1, Ordering::Release);

        for _ in 0..200 {
            if slot0.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 0);
        assert_eq!(slot0.yield_count.load(Ordering::Acquire), 1);
        assert_eq!(slot0.forced_preempt_count.load(Ordering::Acquire), 0);
        assert_ne!(slot0.guest_tsc_offset.load(Ordering::Acquire), 0);
        // No in-kernel local APIC here, so there's no TSC-deadline timer to publish.
        assert_eq!(slot0.park_timer_deadline_tsc.load(Ordering::Acquire), 0);

        // 3. Wake the parked KVM vCPU thread via wake_eventfd and verify it resumes KVM execution.
        state.wake_vcpu(0).unwrap();
        let guest_rax = vcpu_thread.join().expect("KVM vCPU thread panicked");

        // Verify the guest read PylonHeader.magic[0..2] (0x4D31) from GPA 0x8000 and updated runnable_threads = 7.
        assert_eq!(guest_rax & 0xFFFF, 0x4D31);
        assert_eq!(page.telemetry.runnable_threads.load(Ordering::Acquire), 7);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
    }
}
