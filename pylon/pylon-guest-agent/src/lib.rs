// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use core::sync::atomic::Ordering;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use std::{io, thread};

use pylon_abi::clock::read_counter;
use pylon_abi::{
    MAX_VCPUS, PCPU_UNASSIGNED, PYLON_ABI_VERSION, PYLON_DEFAULT_GPA, PYLON_MAGIC,
    PYLON_SHARED_PAGE_SIZE, PylonHeader, PylonSharedPage, VCPU_STATE_PARKED, VCPU_STATE_RUNNING,
    VCPU_STATE_WAKING,
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PylonGuestError {
    #[error("Failed to open memory backing file/device: {0}")]
    OpenFailed(#[from] io::Error),

    #[error("Failed to mmap Pylon shared memory: {0}")]
    MmapFailed(String),

    #[error("Invalid Pylon magic: expected 0x{expected:X}, found 0x{actual:X}")]
    InvalidMagic { expected: u64, actual: u64 },

    #[error("Unsupported Pylon ABI version: expected {expected}, found {actual}")]
    InvalidVersion { expected: u32, actual: u32 },

    #[error("Invalid vCPU count in header: {0} (must be 1..={MAX_VCPUS})")]
    InvalidNumVcpus(u32),

    #[error("Failed to parse /proc metrics: {0}")]
    ProcParseError(String),
}

/// Abstract reader for system workload and runnable thread metrics.
pub trait TelemetryReader: Send + Sync {
    /// Returns the current number of runnable threads in the guest kernel.
    fn read_runnable_threads(&self) -> Result<u32, PylonGuestError>;
}

/// Standard Linux procfs reader (`/proc/loadavg` and `/proc/stat`).
#[derive(Debug, Clone, Default)]
pub struct LinuxProcfsReader;

impl LinuxProcfsReader {
    pub fn new() -> Self {
        Self
    }

    /// Parses the runnable task count from a `/proc/loadavg` string.
    /// Format: `0.00 0.00 0.00 2/145 12345` -> returns `2`.
    pub fn parse_loadavg_content(content: &str) -> Result<u32, PylonGuestError> {
        // Find field 4 (e.g., "2/145")
        let field4 = content.split_whitespace().nth(3).ok_or_else(|| {
            PylonGuestError::ProcParseError("invalid /proc/loadavg format".into())
        })?;

        let (runnable_str, _) = field4.split_once('/').ok_or_else(|| {
            PylonGuestError::ProcParseError("missing '/' in /proc/loadavg".into())
        })?;

        runnable_str.parse::<u32>().map_err(|e| {
            PylonGuestError::ProcParseError(format!("failed to parse runnable count: {e}"))
        })
    }

    /// Parses `procs_running` from `/proc/stat` content.
    pub fn parse_stat_content(content: &str) -> Option<u32> {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("procs_running ") {
                return rest.trim().parse::<u32>().ok();
            }
        }
        None
    }
}

impl TelemetryReader for LinuxProcfsReader {
    fn read_runnable_threads(&self) -> Result<u32, PylonGuestError> {
        // Try /proc/loadavg first (very fast, <100 bytes)
        if let Ok(content) = fs::read_to_string("/proc/loadavg") {
            let res = Self::parse_loadavg_content(&content);
            if let Ok(runnable) = res {
                return Ok(runnable);
            }
        }

        // Fallback to /proc/stat
        if let Ok(content) = fs::read_to_string("/proc/stat") {
            let res = Self::parse_stat_content(&content);
            if let Some(runnable) = res {
                return Ok(runnable);
            }
        }

        Err(PylonGuestError::ProcParseError(
            "unable to read runnable threads from /proc".into(),
        ))
    }
}

/// Backing memory handle for the Pylon shared page.
pub enum PylonMemoryBackend {
    Mmap(memmap2::MmapMut),
    RawPtr(*mut PylonSharedPage),
}

// SAFETY: `PylonSharedPage` fields are atomic or read-only headers and safe across threads.
unsafe impl Send for PylonMemoryBackend {}
// SAFETY: Concurrent atomic access to `PylonSharedPage` is guaranteed by atomic memory fences.
unsafe impl Sync for PylonMemoryBackend {}

impl PylonMemoryBackend {
    /// Map GPA at `0xFEDF_0000` via `/dev/mem`.
    pub fn from_dev_mem(gpa: u64) -> Result<Self, PylonGuestError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open("/dev/mem")
            .map_err(PylonGuestError::OpenFailed)?;

        // SAFETY: Mapping the Pylon shared MMIO page via /dev/mem with O_SYNC is safe
        // because the physical region 0xFEDF_0000 is reserved MMIO provided by the hypervisor.
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .offset(gpa)
                .len(PYLON_SHARED_PAGE_SIZE)
                .map_mut(&file)
                .map_err(|e| PylonGuestError::MmapFailed(e.to_string()))?
        };

        Ok(Self::Mmap(mmap))
    }

    /// Map from a shared memory file (e.g. `/dev/shm/...` or test mock).
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, PylonGuestError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(PylonGuestError::OpenFailed)?;

        // SAFETY: Mapping the shared memory file is safe as the size matches PYLON_SHARED_PAGE_SIZE.
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .len(PYLON_SHARED_PAGE_SIZE)
                .map_mut(&file)
                .map_err(|e| PylonGuestError::MmapFailed(e.to_string()))?
        };

        Ok(Self::Mmap(mmap))
    }

    /// Wrap an existing in-memory pointer for unit testing.
    pub fn from_raw(ptr: *mut PylonSharedPage) -> Self {
        Self::RawPtr(ptr)
    }

    #[inline]
    pub fn as_shared_page(&self) -> &PylonSharedPage {
        match self {
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "mmap(2) returns page-aligned memory"
            )]
            // SAFETY: The mmap covers at least PYLON_SHARED_PAGE_SIZE bytes aligned to page boundary.
            Self::Mmap(m) => unsafe { &*(m.as_ptr() as *const PylonSharedPage) },
            // SAFETY: Caller guarantees the raw pointer points to a valid PylonSharedPage.
            Self::RawPtr(p) => unsafe { &**p },
        }
    }
}

/// Status snapshot of the Pylon VM from the guest perspective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpuStatus {
    pub vcpu_id: u32,
    pub state: u8,
    pub state_name: &'static str,
    pub assigned_pcpu: Option<u8>,
    pub revoke_requested: bool,
    pub yield_count: u64,
    pub forced_preempt_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestAgentStatus {
    pub vm_id: u64,
    pub num_vcpus: u32,
    pub tsc_khz: u64,
    pub runnable_threads: u32,
    pub guest_heartbeat: u64,
    pub vcpus: Vec<VcpuStatus>,
}

/// Guest-side agent that interfaces with Pylon shared memory.
pub struct PylonGuestAgent<R = LinuxProcfsReader> {
    backend: PylonMemoryBackend,
    reader: R,
}

impl<R: TelemetryReader> PylonGuestAgent<R> {
    /// Creates a new guest agent with the given memory backend and telemetry reader.
    pub fn new(backend: PylonMemoryBackend, reader: R) -> Result<Self, PylonGuestError> {
        let page = backend.as_shared_page();
        let header = &page.header;

        if header.magic != PYLON_MAGIC {
            return Err(PylonGuestError::InvalidMagic {
                expected: PYLON_MAGIC,
                actual: header.magic,
            });
        }
        if header.version != PYLON_ABI_VERSION {
            return Err(PylonGuestError::InvalidVersion {
                expected: PYLON_ABI_VERSION,
                actual: header.version,
            });
        }
        if header.num_vcpus == 0 || (header.num_vcpus as usize) > MAX_VCPUS {
            return Err(PylonGuestError::InvalidNumVcpus(header.num_vcpus));
        }

        Ok(Self { backend, reader })
    }

    /// Access the underlying `PylonSharedPage`.
    #[inline]
    pub fn shared_page(&self) -> &PylonSharedPage {
        self.backend.as_shared_page()
    }

    /// Access the header metadata.
    #[inline]
    pub fn header(&self) -> &PylonHeader {
        &self.shared_page().header
    }

    /// Read guest runqueue demand and update shared memory telemetry.
    pub fn update_telemetry(&self) -> Result<u32, PylonGuestError> {
        let runnable = self.reader.read_runnable_threads()?;
        let page = self.shared_page();

        // 1. Update runnable thread count
        page.telemetry
            .runnable_threads
            .store(runnable, Ordering::Release);

        // 2. Increment heartbeat. The agent is its only writer, so a load and a store do the
        //    job of `fetch_add`. The page is mapped through `/dev/mem`, which on arm64 is
        //    device memory, where atomic read-modify-writes aren't supported.
        let heartbeat = &page.telemetry.guest_heartbeat;
        heartbeat.store(
            heartbeat.load(Ordering::Relaxed).wrapping_add(1),
            Ordering::Relaxed,
        );

        // 3. Record (in the guest counter) when threads started waiting for a vCPU, and keep that time
        //    while they still wait, so `pylond` sees how long the backlog has lasted.
        let active = page.active_vcpu_count();
        let head_wait = &page.telemetry.head_wait_tsc;
        if runnable > active {
            if head_wait.load(Ordering::Relaxed) == 0 {
                // 0 means "none wait", so a counter that reads 0 is stored as 1.
                head_wait.store(read_counter().max(1), Ordering::Release);
            }
        } else {
            head_wait.store(0, Ordering::Release);
        }

        Ok(runnable)
    }

    /// Take a full status snapshot.
    pub fn status(&self) -> GuestAgentStatus {
        let page = self.shared_page();
        let num_vcpus = (page.header.num_vcpus as usize).min(MAX_VCPUS);
        let mut vcpus = Vec::with_capacity(num_vcpus);

        for (i, slot) in page.vcpus.iter().enumerate().take(num_vcpus) {
            let state = slot.state.load(Ordering::Acquire);
            let state_name = match state {
                VCPU_STATE_PARKED => "PARKED",
                VCPU_STATE_RUNNING => "RUNNING",
                pylon_abi::VCPU_STATE_PARKING => "PARKING",
                VCPU_STATE_WAKING => "WAKING",
                _ => "UNKNOWN",
            };
            let pcpu = slot.assigned_pcpu.load(Ordering::Acquire);
            let assigned_pcpu = if pcpu == PCPU_UNASSIGNED {
                None
            } else {
                Some(pcpu)
            };

            vcpus.push(VcpuStatus {
                vcpu_id: i as u32,
                state,
                state_name,
                assigned_pcpu,
                revoke_requested: slot.revoke_requested.load(Ordering::Acquire) == 1,
                yield_count: slot.yield_count.load(Ordering::Relaxed),
                forced_preempt_count: slot.forced_preempt_count.load(Ordering::Relaxed),
            });
        }

        GuestAgentStatus {
            vm_id: page.header.vm_id,
            num_vcpus: page.header.num_vcpus,
            tsc_khz: page.header.tsc_khz,
            runnable_threads: page.telemetry.runnable_threads.load(Ordering::Acquire),
            guest_heartbeat: page.telemetry.guest_heartbeat.load(Ordering::Relaxed),
            vcpus,
        }
    }

    /// Main loop step: update telemetry and return status.
    ///
    /// The agent only writes telemetry. It never writes a vCPU's `state` or clears
    /// `revoke_requested`: a userspace process can't stop a vCPU, so parking belongs to the
    /// guest kernel and the VMM (`spec/06-guest-kick-path.md` §3).
    pub fn step(&self) -> Result<GuestAgentStatus, PylonGuestError> {
        self.update_telemetry()?;
        Ok(self.status())
    }

    /// Run the agent loop until `running` becomes false.
    pub fn run_loop(
        &self,
        poll_interval: Duration,
        running: &AtomicBool,
    ) -> Result<(), PylonGuestError> {
        while running.load(Ordering::Relaxed) {
            self.step()?;
            thread::sleep(poll_interval);
        }
        Ok(())
    }
}

impl PylonGuestAgent<LinuxProcfsReader> {
    /// Connect to the Pylon shared page at GPA (defaults to `0xFEDF_0000`).
    pub fn connect_gpa(gpa: Option<u64>) -> Result<Self, PylonGuestError> {
        let gpa_val = gpa.unwrap_or(PYLON_DEFAULT_GPA);
        let backend = PylonMemoryBackend::from_dev_mem(gpa_val)?;
        Self::new(backend, LinuxProcfsReader::new())
    }

    /// Connect to a shared memory file directly (e.g., `/dev/shm/...`).
    pub fn connect_file<P: AsRef<Path>>(path: P) -> Result<Self, PylonGuestError> {
        let backend = PylonMemoryBackend::from_file(path)?;
        Self::new(backend, LinuxProcfsReader::new())
    }
}

#[cfg(test)]
mod tests {
    use core::mem;
    use std::sync::atomic::AtomicU32;

    use super::*;

    struct MockReader {
        runnable: AtomicU32,
    }

    impl MockReader {
        fn new(val: u32) -> Self {
            Self {
                runnable: AtomicU32::new(val),
            }
        }
        fn set(&self, val: u32) {
            self.runnable.store(val, Ordering::Release);
        }
    }

    impl TelemetryReader for MockReader {
        fn read_runnable_threads(&self) -> Result<u32, PylonGuestError> {
            Ok(self.runnable.load(Ordering::Acquire))
        }
    }

    #[test]
    fn test_parse_loadavg_formats() {
        let normal = "0.15 0.08 0.02 3/145 12345\n";
        assert_eq!(LinuxProcfsReader::parse_loadavg_content(normal).unwrap(), 3);

        let idle = "0.00 0.00 0.00 0/98 543\n";
        assert_eq!(LinuxProcfsReader::parse_loadavg_content(idle).unwrap(), 0);

        let high = "12.45 8.12 4.01 16/512 9999\n";
        assert_eq!(LinuxProcfsReader::parse_loadavg_content(high).unwrap(), 16);

        let bad = "invalid string without slashes";
        assert!(matches!(
            LinuxProcfsReader::parse_loadavg_content(bad),
            Err(PylonGuestError::ProcParseError(_))
        ));
    }

    #[test]
    fn test_parse_stat_formats() {
        let content = "cpu  123 456 789\nprocs_running 4\nprocs_blocked 0\n";
        assert_eq!(LinuxProcfsReader::parse_stat_content(content), Some(4));

        let empty = "cpu  123 456 789\n";
        assert_eq!(LinuxProcfsReader::parse_stat_content(empty), None);
    }

    #[test]
    fn test_guest_agent_lifecycle() {
        // SAFETY: Zeroing PylonSharedPage produces a valid initial state with zeroes in atomics.
        let mut page = Box::new(unsafe { mem::zeroed::<PylonSharedPage>() });
        page.init(101, 4, 2_400_000);

        let page_ptr = Box::into_raw(page);
        let backend = PylonMemoryBackend::from_raw(page_ptr);
        let reader = MockReader::new(1);
        let agent = PylonGuestAgent::new(backend, reader).unwrap();

        // 1. Initial status
        let st0 = agent.status();
        assert_eq!(st0.vm_id, 101);
        assert_eq!(st0.num_vcpus, 4);
        assert_eq!(st0.runnable_threads, 0);

        // 2. Update telemetry with demand = 3
        agent.reader.set(3);
        let runnable = agent.update_telemetry().unwrap();
        assert_eq!(runnable, 3);
        let st1 = agent.status();
        assert_eq!(st1.runnable_threads, 3);
        assert_eq!(st1.guest_heartbeat, 1);

        // 3. A pending revocation is left to the VMM: a step only writes telemetry.
        let shared = agent.shared_page();
        shared.vcpus[1]
            .state
            .store(VCPU_STATE_RUNNING, Ordering::Release);
        shared.vcpus[1].assigned_pcpu.store(4, Ordering::Release);
        shared.vcpus[1].revoke_requested.store(1, Ordering::Release);

        let st2 = agent.step().unwrap();
        assert!(st2.vcpus[1].revoke_requested);
        assert_eq!(
            shared.vcpus[1].state.load(Ordering::Acquire),
            VCPU_STATE_RUNNING
        );
        assert_eq!(shared.vcpus[1].revoke_requested.load(Ordering::Acquire), 1);
        assert_eq!(shared.vcpus[1].yield_count.load(Ordering::Relaxed), 0);
        assert_eq!(st2.guest_heartbeat, 2);

        // 4. The backlog's start time holds while threads still wait for a vCPU...
        let head_wait = shared.telemetry.head_wait_tsc.load(Ordering::Acquire);
        assert_ne!(head_wait, 0);
        agent.update_telemetry().unwrap();
        assert_eq!(
            shared.telemetry.head_wait_tsc.load(Ordering::Acquire),
            head_wait
        );

        // ...and clears once there are as many running vCPUs as runnable threads.
        for slot in &shared.vcpus[..3] {
            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
        }
        agent.update_telemetry().unwrap();
        assert_eq!(shared.telemetry.head_wait_tsc.load(Ordering::Acquire), 0);

        // Clean up raw pointer
        // SAFETY: Reconstituting Box from raw pointer for safe deallocation.
        unsafe { drop(Box::from_raw(page_ptr)) };
    }

    /// `head_wait_tsc` is the guest counter at the moment a backlog started, not a placeholder:
    /// `pylond` turns it into a wait time.
    #[test]
    fn test_head_wait_is_a_counter_reading() {
        // SAFETY: Zeroing PylonSharedPage produces a valid initial state with zeroes in atomics.
        let mut page = Box::new(unsafe { mem::zeroed::<PylonSharedPage>() });
        page.init(102, 4, 0);
        let page_ptr = Box::into_raw(page);
        let agent =
            PylonGuestAgent::new(PylonMemoryBackend::from_raw(page_ptr), MockReader::new(3))
                .unwrap();

        let before = read_counter();
        agent.update_telemetry().unwrap();
        let after = read_counter();
        let head_wait = agent
            .shared_page()
            .telemetry
            .head_wait_tsc
            .load(Ordering::Acquire);
        assert!(
            (before.max(1)..=after.max(1)).contains(&head_wait),
            "{head_wait} not in {before}..={after}"
        );

        // SAFETY: Reconstituting Box from raw pointer for safe deallocation.
        unsafe { drop(Box::from_raw(page_ptr)) };
    }

    /// The agent maps the page through `/dev/mem`, which on arm64 is device memory, where atomic
    /// read-modify-writes (`ldadd`, `swp`, exclusives) aren't supported (`spec/07-aarch64-support.md`
    /// §4.4). It may only load and store. Read-modify-write methods compile to those instructions,
    /// so keep them out of the agent's own code.
    #[test]
    fn test_agent_does_no_read_modify_write_on_the_page() {
        let source = include_str!("lib.rs");
        let agent_code = source.split("#[cfg(test)]").next().unwrap();
        for method in [
            ".fetch_add(",
            ".fetch_sub(",
            ".fetch_and(",
            ".fetch_or(",
            ".fetch_xor(",
            ".fetch_max(",
            ".fetch_min(",
            ".fetch_update(",
            ".swap(",
            ".compare_exchange(",
            ".compare_exchange_weak(",
        ] {
            assert!(
                !agent_code.contains(method),
                "the agent must not call `{method}` on the shared page"
            );
        }
    }
}
