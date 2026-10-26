// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! End-to-End Integration Tests for the Pylon Subsystem (`pylon-abi`, `pylond`, `scx-pylon`, and VMM vCPU fast-park lifecycle).

use std::env::temp_dir;
use std::fs::{self, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{io, process};

use arc_swap::ArcSwap;
use memmap2::MmapMut;
use pylon_abi::{
    PCPU_UNASSIGNED, PYLON_SHARED_PAGE_SIZE, PylonSharedPage, VCPU_STATE_PARKED, VCPU_STATE_RUNNING,
};
use pylond::control::{ControlPlane, send_registration};
use pylond::engine::{DEFAULT_TSC_KHZ, MatrixEngine, read_tsc};
use pylond::{
    CoreState, PcpuOwnerTable, VmRegistrationRequest, VmSnapshot, create_eventfd, read_eventfd,
    write_eventfd,
};
use scx_pylon::{PCPU_OWNER_MAP_BYTES, PcpuOwnerMap, ScxPylonUserspace};
use tokio::runtime::Builder;
use tokio::sync::watch;

fn dup_fd(fd: RawFd) -> OwnedFd {
    // SAFETY: `libc::dup` duplicates a valid open file descriptor.
    let duped = unsafe { libc::dup(fd) };
    assert!(duped >= 0, "dup failed: {}", io::Error::last_os_error());
    // SAFETY: `duped` is a freshly duplicated valid file descriptor.
    unsafe { OwnedFd::from_raw_fd(duped) }
}

fn create_blocking_eventfd() -> OwnedFd {
    // SAFETY: `libc::eventfd(0, EFD_CLOEXEC)` creates a blocking eventfd.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    assert!(fd >= 0, "eventfd failed: {}", io::Error::last_os_error());
    // SAFETY: `fd` is a newly created valid file descriptor.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn poll_fd_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is a valid single-element `pollfd` struct on the stack.
    let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Represents a simulated Cloud Hypervisor VM with real OS vCPU threads,
/// a shared `/dev/shm` `PylonSharedPage`, and `kick`/`wake` `EventFd` pairs.
struct SimulatedVmm {
    shm_path: PathBuf,
    shm_mmap: MmapMut,
    stop: Arc<AtomicBool>,
    work_counters: Vec<Arc<AtomicU64>>,
    wake_fds: Vec<OwnedFd>,
    kick_fds: Vec<OwnedFd>,
    vcpu_threads: Vec<JoinHandle<()>>,
}

impl SimulatedVmm {
    #[expect(clippy::too_many_arguments)]
    fn spawn_and_register(
        dir: &Path,
        socket_path: &Path,
        vm_id: u64,
        num_vcpus: u32,
        min_cores: u32,
        max_cores: u32,
        priority_weight: u32,
        cooperative: bool,
    ) -> Self {
        let shm_path = dir.join(format!("vm-{vm_id}-pylon.shm"));
        let shm_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&shm_path)
            .expect("failed to create VM shared memory file");
        shm_file
            .set_len(PYLON_SHARED_PAGE_SIZE as u64)
            .expect("failed to resize VM shared memory file");

        // SAFETY: `shm_file` is 8,192 bytes and accessed atomically through `PylonSharedPage`.
        let mut shm_mmap = unsafe { MmapMut::map_mut(&shm_file).expect("mmap failed") };
        // SAFETY: `shm_mmap` is page-aligned and 8,192 bytes long.
        let page = unsafe { &mut *(shm_mmap.as_mut_ptr().cast::<PylonSharedPage>()) };
        page.init(vm_id, num_vcpus, DEFAULT_TSC_KHZ);

        let stop = Arc::new(AtomicBool::new(false));
        let cooperative_flag = Arc::new(AtomicBool::new(cooperative));
        let mut kick_fds = Vec::with_capacity(num_vcpus as usize);
        let mut wake_fds = Vec::with_capacity(num_vcpus as usize);
        let mut work_counters = Vec::with_capacity(num_vcpus as usize);
        let mut vcpu_threads = Vec::with_capacity(num_vcpus as usize);

        for vcpu_idx in 0..(num_vcpus as usize) {
            let kick_fd = create_eventfd().expect("failed to create kick eventfd");
            let wake_fd = create_blocking_eventfd();

            let thread_kick_fd = dup_fd(kick_fd.as_raw_fd());
            let thread_wake_fd = dup_fd(wake_fd.as_raw_fd());
            kick_fds.push(kick_fd);
            wake_fds.push(wake_fd);

            let work_counter = Arc::new(AtomicU64::new(0));
            work_counters.push(Arc::clone(&work_counter));

            let thread_file = shm_file.try_clone().expect("failed to clone shm_file");
            // SAFETY: Mapping the same 8,192-byte shared memory file for the vCPU thread.
            let thread_mmap = unsafe { MmapMut::map_mut(&thread_file).expect("thread mmap") };
            let thread_stop = Arc::clone(&stop);
            let thread_coop = Arc::clone(&cooperative_flag);

            let handle = thread::Builder::new()
                .name(format!("vm{vm_id}-vcpu{vcpu_idx}"))
                .spawn(move || {
                    // SAFETY: `thread_mmap` is 8,192 bytes and page-aligned.
                    let shared = unsafe { &*(thread_mmap.as_ptr().cast::<PylonSharedPage>()) };
                    let slot = &shared.vcpus[vcpu_idx];

                    // Export real Linux kernel TID (`gettid()`).
                    // SAFETY: `libc::gettid()` is always safe to call.
                    let tid = unsafe { libc::gettid() as u32 };
                    slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
                    slot.host_tid.store(tid, Ordering::Release);

                    let mut parked = true;
                    while !thread_stop.load(Ordering::Acquire) {
                        if parked {
                            if poll_fd_readable(thread_wake_fd.as_raw_fd(), 5) {
                                let _ = read_eventfd(thread_wake_fd.as_raw_fd());
                                if thread_stop.load(Ordering::Acquire) {
                                    break;
                                }
                                slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
                                parked = false;
                            }
                            continue;
                        }

                        // If forced preemption occurred externally (Phase 3), transition to parked.
                        if slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED {
                            parked = true;
                            continue;
                        }

                        // vCPU is RUNNING or WAKING: transition WAKING -> RUNNING and execute work.
                        if slot.state.load(Ordering::Acquire) != VCPU_STATE_RUNNING {
                            slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
                        }
                        work_counter.fetch_add(1, Ordering::Relaxed);

                        // Drain or wait briefly on kick_irqfd so running vCPUs do not starve 4-vCPU
                        // CI runners and wake immediately when MatrixEngine signals kick_irqfd.
                        let kick_timeout_ms = if slot.revoke_requested.load(Ordering::Acquire) == 0
                        {
                            1
                        } else {
                            0
                        };
                        if poll_fd_readable(thread_kick_fd.as_raw_fd(), kick_timeout_ms) {
                            let _ = read_eventfd(thread_kick_fd.as_raw_fd());
                        }

                        // Check cooperative revocation contract (`revoke_requested == 1`).
                        if thread_coop.load(Ordering::Acquire)
                            && slot.revoke_requested.load(Ordering::Acquire) == 1
                            && slot.in_critical_section.load(Ordering::Acquire) == 0
                        {
                            slot.revoke_requested.store(0, Ordering::Release);
                            slot.yield_count.fetch_add(1, Ordering::AcqRel);
                            slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
                            parked = true;
                        }
                    }
                })
                .expect("failed to spawn vCPU thread");

            vcpu_threads.push(handle);
        }

        // Wait until all vCPU threads have registered their non-zero `host_tid`.
        let deadline = Instant::now() + Duration::from_secs(3);
        // SAFETY: `shm_mmap` is valid and page-aligned.
        let shared = unsafe { &*(shm_mmap.as_ptr().cast::<PylonSharedPage>()) };
        for (vcpu_idx, slot) in shared.vcpus.iter().enumerate().take(num_vcpus as usize) {
            while slot.host_tid.load(Ordering::Acquire) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for vm{vm_id} vCPU {vcpu_idx} host_tid registration"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }

        // Register VM with `pylond` Plane 1 over the Unix domain socket using `SCM_RIGHTS`.
        let req = VmRegistrationRequest {
            vm_id,
            num_vcpus,
            min_cores,
            max_cores,
            priority_weight,
        };
        let kick_raws: Vec<RawFd> = kick_fds.iter().map(AsRawFd::as_raw_fd).collect();
        let wake_raws: Vec<RawFd> = wake_fds.iter().map(AsRawFd::as_raw_fd).collect();
        let resp = send_registration(
            socket_path,
            &req,
            shm_file.as_raw_fd(),
            &kick_raws,
            &wake_raws,
        )
        .expect("SCM_RIGHTS registration with pylond failed");
        assert!(
            resp.accepted,
            "pylond rejected VM {vm_id}: {}",
            resp.message
        );

        Self {
            shm_path,
            shm_mmap,
            stop,
            work_counters,
            wake_fds,
            kick_fds,
            vcpu_threads,
        }
    }

    fn shared_page(&self) -> &PylonSharedPage {
        // SAFETY: `shm_mmap` is 8,192 bytes and page-aligned.
        unsafe { &*(self.shm_mmap.as_ptr().cast::<PylonSharedPage>()) }
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        for fd in &self.wake_fds {
            let _ = write_eventfd(fd.as_raw_fd());
        }
        for fd in &self.kick_fds {
            let _ = write_eventfd(fd.as_raw_fd());
        }
        for handle in self.vcpu_threads.drain(..) {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.shm_path);
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool, msg: &str) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "Timed out: {msg}");
        thread::sleep(Duration::from_micros(250));
    }
}

#[test]
fn test_e2e_full_stack_multi_vm_scale_out_and_cooperative_revocation() {
    // 1. Verify the embedded scx_pylon eBPF object passes structural verification.
    ScxPylonUserspace::verify_embedded_object()
        .expect("Embedded scx_pylon.bpf.o failed verification");

    let temp_dir = temp_dir().join(format!("pylon-e2e-full-{}", process::id()));
    fs::create_dir_all(&temp_dir).expect("failed to create temp_dir");
    let socket_path = temp_dir.join("pylond.sock");
    let pcpu_map_path = temp_dir.join("pylon_pcpu_owners.shm");

    // 2. Create the shared `pylon_pcpu_owners` backing file and map it into both `pylond` and `scx-pylon`.
    let map_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&pcpu_map_path)
        .expect("failed to create pcpu_map_path");
    map_file
        .set_len(PCPU_OWNER_MAP_BYTES as u64)
        .expect("failed to size pcpu_map_path");
    // SAFETY: `map_file` is 2,048 bytes (`256 * 8`) and accessed via atomic operations.
    let map_mmap = unsafe { MmapMut::map_mut(&map_file).expect("failed to mmap pcpu_map_path") };
    let pcpu_owner_table =
        Arc::new(PcpuOwnerTable::from_mmap(map_mmap).expect("PcpuOwnerTable::from_mmap"));
    let scx_owner_map =
        PcpuOwnerMap::from_shm(&pcpu_map_path).expect("PcpuOwnerMap::from_shm failed");

    // 3. Start Plane 1 (ControlPlane) on a Tokio runtime and Plane 2 (MatrixEngine) on a dedicated thread.
    let vm_snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let control_plane = ControlPlane::new(&socket_path, Arc::clone(&vm_snapshot));

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let control_thread = thread::spawn(move || {
        rt.block_on(control_plane.run(shutdown_rx))
            .expect("ControlPlane failed");
    });

    wait_until(
        Duration::from_secs(2),
        || socket_path.exists(),
        "ControlPlane socket creation",
    );

    let tenant_pcpus = [2usize, 3, 4, 5];
    let mut engine = MatrixEngine::new(
        Arc::clone(&vm_snapshot),
        Arc::clone(&pcpu_owner_table),
        &tenant_pcpus,
    );
    let engine_stop = Arc::new(AtomicBool::new(false));
    let engine_stop_clone = Arc::clone(&engine_stop);
    let engine_thread = thread::spawn(move || {
        engine.run(&engine_stop_clone).expect("MatrixEngine failed");
    });

    // 4. Spawn and register two 4-vCPU VMs over the Unix socket with SCM_RIGHTS.
    let vm1 = SimulatedVmm::spawn_and_register(
        &temp_dir,
        &socket_path,
        101,
        4,
        2,  // min_cores
        4,  // max_cores
        10, // priority_weight
        true,
    );
    let vm2 = SimulatedVmm::spawn_and_register(
        &temp_dir,
        &socket_path,
        102,
        4,
        0,   // min_cores (0-core capable)
        4,   // max_cores
        100, // higher priority_weight
        true,
    );

    // 5. Phase A: VM 101 scales out from 0 -> 4 cores across tenant pCPUs 2..=5.
    let page1 = vm1.shared_page();
    let now_tsc = read_tsc();
    page1
        .telemetry
        .stranded_work_bitmap
        .store(0b1111, Ordering::Release);
    page1
        .telemetry
        .head_wait_tsc
        .store(now_tsc.saturating_sub(50_000), Ordering::Release);
    page1.telemetry.runnable_threads.store(4, Ordering::Release);

    wait_until(
        Duration::from_secs(3),
        || {
            page1.active_vcpu_count() == 4
                && (0..4)
                    .all(|i| page1.vcpus[i].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING)
        },
        "VM 101 scale-out to 4 running vCPUs",
    );

    // Verify via `scx_pylon::PcpuOwnerMap` that every vCPU's real Linux TID owns its assigned pCPU!
    for vcpu_idx in 0..4 {
        let tid = page1.vcpus[vcpu_idx].host_tid.load(Ordering::Acquire);
        let pcpu = page1.vcpus[vcpu_idx].assigned_pcpu.load(Ordering::Acquire) as usize;
        assert!(
            tenant_pcpus.contains(&pcpu),
            "unexpected assigned_pcpu {pcpu} for vCPU {vcpu_idx}"
        );
        assert_eq!(
            scx_owner_map.find_pcpu_for_tid(tid),
            Some(pcpu),
            "scx_pylon BPF map did not contain VM 101 vCPU {vcpu_idx} TID {tid}"
        );
        assert!(
            vm1.work_counters[vcpu_idx].load(Ordering::Relaxed) > 0,
            "VM 101 vCPU {vcpu_idx} did not execute work after waking"
        );
    }

    // 6. Phase B: Mark VM 101 vCPUs 0 and 1 as `in_critical_section = 1` and clear its queue wait.
    // Then trigger a high-urgency burst on VM 102 requesting 2 cores (`stranded_work_bitmap = 0b0101` -> vCPUs 0 and 2).
    page1.vcpus[0]
        .in_critical_section
        .store(1, Ordering::Release);
    page1.vcpus[1]
        .in_critical_section
        .store(1, Ordering::Release);
    page1.telemetry.head_wait_tsc.store(0, Ordering::Release);

    let page2 = vm2.shared_page();
    page2
        .telemetry
        .stranded_work_bitmap
        .store(0b0101, Ordering::Release);
    page2
        .telemetry
        .head_wait_tsc
        .store(read_tsc().saturating_sub(200_000), Ordering::Release);
    page2.telemetry.runnable_threads.store(2, Ordering::Release);

    wait_until(
        Duration::from_secs(3),
        || {
            page2.vcpus[0].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING
                && page2.vcpus[2].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING
                && page1.vcpus[2].state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                && page1.vcpus[3].state.load(Ordering::Acquire) == VCPU_STATE_PARKED
        },
        "Cooperative core handoff from VM 101 (vCPUs 2,3) to VM 102 (vCPUs 0,2)",
    );

    // Verify VM 101 vCPUs 0 and 1 (in critical section) remained RUNNING, while vCPUs 2 and 3 yielded and PARKED!
    assert_eq!(
        page1.vcpus[0].state.load(Ordering::Acquire),
        VCPU_STATE_RUNNING
    );
    assert_eq!(
        page1.vcpus[1].state.load(Ordering::Acquire),
        VCPU_STATE_RUNNING
    );
    assert_eq!(
        page1.vcpus[2].state.load(Ordering::Acquire),
        VCPU_STATE_PARKED
    );
    assert_eq!(
        page1.vcpus[3].state.load(Ordering::Acquire),
        VCPU_STATE_PARKED
    );
    assert_eq!(
        page1.vcpus[0].yield_count.load(Ordering::Acquire)
            + page1.vcpus[0].forced_preempt_count.load(Ordering::Acquire),
        0
    );
    assert_eq!(
        page1.vcpus[1].yield_count.load(Ordering::Acquire)
            + page1.vcpus[1].forced_preempt_count.load(Ordering::Acquire),
        0
    );
    assert!(
        page1.vcpus[2].yield_count.load(Ordering::Acquire)
            + page1.vcpus[2].forced_preempt_count.load(Ordering::Acquire)
            >= 1
    );
    assert!(
        page1.vcpus[3].yield_count.load(Ordering::Acquire)
            + page1.vcpus[3].forced_preempt_count.load(Ordering::Acquire)
            >= 1
    );

    // Verify `scx_pylon::PcpuOwnerMap` now maps the 2 handed-off pCPUs to VM 102's vCPU 0 and vCPU 2 TIDs.
    for &vcpu_idx in &[0usize, 2usize] {
        let tid2 = page2.vcpus[vcpu_idx].host_tid.load(Ordering::Acquire);
        let pcpu2 = page2.vcpus[vcpu_idx].assigned_pcpu.load(Ordering::Acquire) as usize;
        assert_eq!(scx_owner_map.find_pcpu_for_tid(tid2), Some(pcpu2));
    }

    // Clean shutdown.
    engine_stop.store(true, Ordering::Release);
    let _ = engine_thread.join();
    let _ = shutdown_tx.send(true);
    let _ = control_thread.join();
    vm1.shutdown();
    vm2.shutdown();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_e2e_zero_core_hibernation_timer_and_rx_doorbell_wakeup() {
    let temp_dir = temp_dir().join(format!("pylon-e2e-zerocore-{}", process::id()));
    fs::create_dir_all(&temp_dir).expect("failed to create temp_dir");
    let socket_path = temp_dir.join("pylond.sock");

    let vm_snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let control_plane = ControlPlane::new(&socket_path, Arc::clone(&vm_snapshot));

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let control_thread = thread::spawn(move || {
        rt.block_on(control_plane.run(shutdown_rx)).unwrap();
    });

    wait_until(
        Duration::from_secs(2),
        || socket_path.exists(),
        "ControlPlane socket ready",
    );

    let vm = SimulatedVmm::spawn_and_register(&temp_dir, &socket_path, 201, 2, 0, 2, 100, true);
    let page = vm.shared_page();

    let mut engine = MatrixEngine::new(Arc::clone(&vm_snapshot), Arc::clone(&pcpu_owners), &[2, 3]);

    // 1. VM 201 starts in 0-core hibernation (`runnable_threads == 0`).
    // Set `next_timer_tsc` in the past to trigger an automatic 0-core timer wakeup.
    let now_tsc = read_tsc();
    page.telemetry
        .next_timer_tsc
        .store(now_tsc.saturating_sub(50_000), Ordering::Release);
    page.telemetry
        .stranded_work_bitmap
        .store(0b10, Ordering::Release); // Select vCPU 1 via tzcnt

    engine.poll_once(now_tsc);

    assert_eq!(page.vcpus[1].assigned_pcpu.load(Ordering::Acquire), 2);
    wait_until(
        Duration::from_secs(2),
        || page.vcpus[1].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING,
        "0-core timer wakeup of vCPU 1",
    );

    // 2. Return VM 201 to 0-core hibernation and test network `host_rx_doorbell` wakeup.
    page.vcpus[1]
        .state
        .store(VCPU_STATE_PARKED, Ordering::Release);
    page.vcpus[1]
        .assigned_pcpu
        .store(PCPU_UNASSIGNED, Ordering::Release);
    page.telemetry.runnable_threads.store(0, Ordering::Release);
    page.telemetry.next_timer_tsc.store(0, Ordering::Release);
    engine.set_core_state(2, CoreState::Idle);

    page.telemetry.rx_ring_cons_idx.store(10, Ordering::Release);
    let snap = vm_snapshot.load();
    snap[0].set_host_rx_doorbell(11);
    page.telemetry
        .stranded_work_bitmap
        .store(0b01, Ordering::Release); // Select vCPU 0

    engine.poll_once(read_tsc());

    assert_eq!(page.vcpus[0].assigned_pcpu.load(Ordering::Acquire), 2);
    wait_until(
        Duration::from_secs(2),
        || page.vcpus[0].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING,
        "0-core RX doorbell wakeup of vCPU 0",
    );

    let _ = shutdown_tx.send(true);
    let _ = control_thread.join();
    vm.shutdown();
    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_e2e_hard_deadline_forced_preemption_when_donor_ignores_kick() {
    let temp_dir = temp_dir().join(format!("pylon-e2e-preempt-{}", process::id()));
    fs::create_dir_all(&temp_dir).expect("failed to create temp_dir");
    let socket_path = temp_dir.join("pylond.sock");

    let vm_snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let control_plane = ControlPlane::new(&socket_path, Arc::clone(&vm_snapshot));

    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let control_thread = thread::spawn(move || {
        rt.block_on(control_plane.run(shutdown_rx)).unwrap();
    });

    wait_until(
        Duration::from_secs(2),
        || socket_path.exists(),
        "ControlPlane socket ready",
    );

    // Donor VM 301 is non-cooperative (`cooperative = false`) so it ignores `revoke_requested == 1`.
    let donor = SimulatedVmm::spawn_and_register(&temp_dir, &socket_path, 301, 1, 0, 1, 10, false);
    let recipient =
        SimulatedVmm::spawn_and_register(&temp_dir, &socket_path, 302, 1, 0, 1, 300, true);

    let mut engine = MatrixEngine::new(Arc::clone(&vm_snapshot), Arc::clone(&pcpu_owners), &[4]);

    // 1. Donor acquires pCPU 4.
    let donor_page = donor.shared_page();
    let base_tsc = 1_000_000_000u64;
    donor_page
        .telemetry
        .runnable_threads
        .store(1, Ordering::Release);
    donor_page
        .telemetry
        .head_wait_tsc
        .store(base_tsc - 1_000, Ordering::Release);

    engine.poll_once(base_tsc);
    wait_until(
        Duration::from_secs(2),
        || donor_page.vcpus[0].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING,
        "Donor vCPU 0 running",
    );
    donor_page
        .telemetry
        .head_wait_tsc
        .store(0, Ordering::Release);

    // 2. Recipient requests 1 core with much higher urgency; Plane 2 initiates revocation on pCPU 4.
    let recip_page = recipient.shared_page();
    recip_page
        .telemetry
        .runnable_threads
        .store(1, Ordering::Release);
    recip_page
        .telemetry
        .head_wait_tsc
        .store(base_tsc - 50_000, Ordering::Release);

    engine.poll_once(base_tsc + 1_000);
    assert_eq!(
        donor_page.vcpus[0].revoke_requested.load(Ordering::Acquire),
        1
    );
    let deadline = donor_page.vcpus[0]
        .revoke_deadline_tsc
        .load(Ordering::Acquire);
    assert!(deadline > base_tsc);

    // 3. Advance TSC past the 8 us deadline while donor remains in `VCPU_STATE_RUNNING`.
    engine.poll_once(deadline + 1);

    assert_eq!(
        donor_page.vcpus[0]
            .forced_preempt_count
            .load(Ordering::Acquire),
        1,
        "Donor vCPU 0 forced_preempt_count must increment when 8 us deadline expires"
    );
    let recip_tid = recip_page.vcpus[0].host_tid.load(Ordering::Acquire);
    assert_eq!(pcpu_owners.read_owner(4).0, recip_tid);

    wait_until(
        Duration::from_secs(2),
        || recip_page.vcpus[0].state.load(Ordering::Acquire) == VCPU_STATE_RUNNING,
        "Recipient vCPU 0 running after forced preemption",
    );

    let _ = shutdown_tx.send(true);
    let _ = control_thread.join();
    donor.shutdown();
    recipient.shutdown();
    let _ = fs::remove_dir_all(&temp_dir);
}
