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
use std::{fs, io, ptr, result};

use hypervisor::{HypervisorVmError, MemoryVisibility, Vm};
use log::{info, warn};
use option_parser::{OptionParser, OptionParserError};
use pylon_abi::{
    MAX_VCPUS, PYLON_ABI_VERSION, PYLON_DEFAULT_GPA, PYLON_MAGIC, PYLON_SHARED_PAGE_SIZE,
    PylonHeader, PylonSharedPage, PylonVcpuSlot, VCPU_STATE_PARKED, VCPU_STATE_RUNNING,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use vm_memory::guest_memory::FileOffset;
use vm_memory::mmap::MmapRegionError;
use vm_memory::{
    Address, GuestAddress, GuestMemoryRegion, GuestRegionCollectionError, MmapRegion,
};
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::{GuestMemoryMmap, GuestRegionMmap};

/// Default guest TSC frequency in kHz written into `PylonHeader` when not queried from host.
pub const DEFAULT_TSC_KHZ: u64 = 3_000_000;

/// Default VM priority weight for Pylon core arbitration.
pub const DEFAULT_PRIORITY_WEIGHT: u32 = 100;

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

    /// Failed to insert the Pylon `GuestRegionMmap` into `GuestMemoryMmap`.
    #[error("Failed to insert Pylon region into guest memory")]
    GuestMemoryInsert(#[source] GuestRegionCollectionError),

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
}

/// VMM-side Pylon runtime state managing the 8,192-byte `PylonSharedPage` and per-vCPU park/wake primitives.
pub struct PylonVmmState {
    config: PylonConfig,
    boot_vcpus: u32,
    max_vcpus: u32,
    initial_active_vcpus: u32,
    shm_file: File,
    shared_page: NonNull<PylonSharedPage>,
    kick_eventfds: Vec<EventFd>,
    wake_eventfds: Vec<EventFd>,
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
                    tsc_khz: DEFAULT_TSC_KHZ,
                    _reserved: [0u8; 32],
                },
            );
        }

        let mut kick_eventfds = Vec::with_capacity(num_vcpus as usize);
        let mut wake_eventfds = Vec::with_capacity(num_vcpus as usize);
        for _ in 0..num_vcpus {
            kick_eventfds.push(EventFd::new(libc::EFD_NONBLOCK).map_err(Error::EventFdCreate)?);
            wake_eventfds.push(EventFd::new(0).map_err(Error::EventFdCreate)?);
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
        };
        let mut json_bytes = serde_json::to_vec(&payload)
            .map_err(|e| Error::DaemonRegister(io::Error::other(e)))?;
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
    pub fn setup_gpa_mapping(
        &self,
        vm: &Arc<dyn Vm>,
        guest_memory: &mut GuestMemoryMmap,
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
            GuestRegionMmap::new(mmap_region, gpa)
                .ok_or(Error::GuestRegionNew(self.config.gpa))?,
        );

        *guest_memory = guest_memory
            .insert_region(Arc::clone(&region))
            .map_err(Error::GuestMemoryInsert)?;

        let slot = *next_memory_slot;
        *next_memory_slot += 1;

        // SAFETY: `region` is a valid `MAP_SHARED` mapping of `PYLON_SHARED_PAGE_SIZE` bytes
        // whose backing `Arc<GuestRegionMmap>` is retained inside `guest_memory`.
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

    /// Returns the kick `EventFd` for `vcpu_id`.
    pub fn kick_eventfd(&self, vcpu_id: u32) -> Option<&EventFd> {
        self.kick_eventfds.get(vcpu_id as usize)
    }

    /// Stores the Linux kernel thread ID (`gettid()`) into `PylonSharedPage.vcpus[vcpu_id].host_tid`.
    pub fn register_vcpu_tid(&self, vcpu_id: u32, tid: u32) {
        if let Some(slot) = self.vcpu_slot(vcpu_id) {
            slot.host_tid.store(tid, Ordering::Release);
        }
    }

    /// Called at the start of each vCPU thread (`vmm/src/cpu.rs`):
    /// records `gettid()` in `host_tid` and parks vCPUs with `vcpu_id >= initial_active_vcpus`.
    pub fn on_vcpu_thread_start(&self, vcpu_id: u32) {
        // SAFETY: `libc::gettid()` is always safe to call in a Linux thread context.
        let tid = unsafe { libc::gettid() as u32 };
        self.register_vcpu_tid(vcpu_id, tid);

        let Some(slot) = self.vcpu_slot(vcpu_id) else {
            return;
        };

        if vcpu_id >= self.initial_active_vcpus {
            slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
            self.park_vcpu(vcpu_id);
            if !self.shutting_down.load(Ordering::Acquire) {
                slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
            }
        } else {
            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
        }
    }

    /// Fast-park exit handler called inside the vCPU run loop (`vmm/src/cpu.rs`).
    ///
    /// If `slot.revoke_requested == 1`, clears `revoke_requested`, sets `state = VCPU_STATE_PARKED`,
    /// increments `yield_count`, blocks until woken by `pylond` (or VM shutdown), and restores
    /// `state = VCPU_STATE_RUNNING`. Returns `true` if the vCPU parked and woke up.
    pub fn check_and_park_vcpu(&self, vcpu_id: u32) -> bool {
        let Some(slot) = self.vcpu_slot(vcpu_id) else {
            return false;
        };

        if slot.revoke_requested.load(Ordering::Acquire) != 1 {
            return false;
        }

        slot.revoke_requested.store(0, Ordering::Release);
        slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
        slot.yield_count.fetch_add(1, Ordering::Relaxed);

        self.park_vcpu(vcpu_id);

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
/// into guest physical memory if `--pylon` is active.
pub fn setup_memory_region(
    vm: &Arc<dyn Vm>,
    guest_memory: &mut GuestMemoryMmap,
    next_memory_slot: &mut u32,
) -> result::Result<Option<Arc<GuestRegionMmap>>, Error> {
    if let Some(state) = global_state() {
        let region = state.setup_gpa_mapping(vm, guest_memory, next_memory_slot)?;
        Ok(Some(region))
    } else {
        Ok(None)
    }
}

/// Hook for `CpuManager::start_vcpu` (`vmm/src/cpu.rs`): registers `gettid()` and parks
/// vCPUs with `vcpu_id >= initial_active_vcpus`.
pub fn on_vcpu_thread_start(vcpu_id: u32) {
    if let Some(state) = global_state() {
        state.on_vcpu_thread_start(vcpu_id);
    }
}

/// Hook for the `vcpu.run()` exit loop (`vmm/src/cpu.rs`): parks the vCPU thread if
/// `revoke_requested == 1`.
pub fn check_and_park_vcpu(vcpu_id: u32) -> bool {
    if let Some(state) = global_state() {
        state.check_and_park_vcpu(vcpu_id)
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
        assert_eq!(page.header.tsc_khz, DEFAULT_TSC_KHZ);
        assert_eq!(page.telemetry.runnable_threads.load(Ordering::Relaxed), 0);

        for i in 0..4 {
            let slot = state.vcpu_slot(i).unwrap();
            assert_eq!(slot.state.load(Ordering::Relaxed), VCPU_STATE_PARKED);
            assert_eq!(slot.revoke_requested.load(Ordering::Relaxed), 0);
            assert_eq!(slot.host_tid.load(Ordering::Relaxed), 0);
        }
        assert!(state.vcpu_slot(4).is_none());
    }

    #[test]
    fn test_vcpu_thread_start_tid_and_initial_parking() {
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
            state_vcpu0.on_vcpu_thread_start(0);
        });
        handle0.join().unwrap();

        let slot0 = state.vcpu_slot(0).unwrap();
        assert_ne!(slot0.host_tid.load(Ordering::Acquire), 0);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);

        // vCPU 1 is >= initial_active_vcpus (1), so it should park until woken.
        let state_vcpu1 = Arc::clone(&state);
        let handle1 = thread::spawn(move || {
            state_vcpu1.on_vcpu_thread_start(1);
        });

        let slot1 = state.vcpu_slot(1).unwrap();
        for _ in 0..100 {
            if slot1.host_tid.load(Ordering::Acquire) != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(slot1.host_tid.load(Ordering::Acquire), 0);
        assert_eq!(slot1.state.load(Ordering::Acquire), VCPU_STATE_PARKED);

        // Wake vCPU 1 and verify it transitions to VCPU_STATE_RUNNING.
        state.wake_vcpu(1).unwrap();
        handle1.join().unwrap();
        assert_eq!(slot1.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
    }

    #[test]
    fn test_fast_park_revoke_and_wake_lifecycle() {
        let dir = TempDir::new().unwrap();
        let shm_path = dir.path().join("vm-fastpark-pylon");
        let cfg = PylonConfig::parse(&format!("shm={}", shm_path.display())).unwrap();
        let state = PylonVmmState::new(&cfg, 2, 2).unwrap();

        state.on_vcpu_thread_start(0);
        let slot0 = state.vcpu_slot(0).unwrap();
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);

        // Without revoke_requested set, check_and_park_vcpu is a no-op returning false.
        assert!(!state.check_and_park_vcpu(0));
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);

        // Request revocation on vCPU 0 and run check_and_park_vcpu on a worker thread.
        slot0.revoke_requested.store(1, Ordering::Release);

        let state_worker = Arc::clone(&state);
        let worker = thread::spawn(move || state_worker.check_and_park_vcpu(0));

        // Wait until the worker clears revoke_requested and transitions to VCPU_STATE_PARKED.
        for _ in 0..100 {
            if slot0.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(slot0.revoke_requested.load(Ordering::Acquire), 0);
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_PARKED);
        assert_eq!(slot0.yield_count.load(Ordering::Relaxed), 1);

        // Wake vCPU 0 via its wake EventFd and verify it resumes VCPU_STATE_RUNNING.
        state.wake_vcpu(0).unwrap();
        assert!(worker.join().unwrap());
        assert_eq!(slot0.state.load(Ordering::Acquire), VCPU_STATE_RUNNING);
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
        // 1 shm_fd + 2 kick_eventfds + 2 wake_eventfds = 5 FDs
        assert_eq!(fds_read, 5);
    }
}
