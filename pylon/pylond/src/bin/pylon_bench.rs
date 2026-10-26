// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! Benchmark comparing Pylon microsecond core elasticity against Cloud Hypervisor ACPI CPU hotplug
//! across a multi-wave bursty parallel compute workload.

use core::hint::spin_loop;
use std::env::temp_dir;
use std::fs::{self, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};
use std::{io, process, thread};

use arc_swap::ArcSwap;
use clap::Parser;
use memmap2::MmapMut;
use pylon_abi::{
    PCPU_UNASSIGNED, PYLON_SHARED_PAGE_SIZE, PylonSharedPage, VCPU_STATE_PARKED, VCPU_STATE_RUNNING,
};
use pylond::engine::{DEFAULT_TSC_KHZ, MatrixEngine, read_tsc};
use pylond::{
    CoreState, PcpuOwnerTable, VmHandle, VmRegistrationRequest, VmSnapshot, create_eventfd,
    read_eventfd, write_eventfd,
};

/// Command-line options for the Pylon vs. ACPI CPU Hotplug benchmark runner.
#[derive(Debug, Parser)]
#[command(
    name = "pylon-bench",
    version,
    about = "Benchmark comparing Pylon microsecond core elasticity vs. ACPI CPU hotplug"
)]
struct BenchArgs {
    /// Number of burst waves (scale-out 1 -> max_vcpus -> scale-in 1).
    #[arg(long, default_value_t = 20)]
    waves: usize,

    /// Maximum vCPUs scaled out during each burst wave.
    #[arg(long, default_value_t = 8)]
    max_vcpus: u32,

    /// Parallel compute tasks executed per burst wave.
    #[arg(long, default_value_t = 64)]
    tasks_per_wave: usize,

    /// Compute iterations per task (controls CPU work per wave).
    #[arg(long, default_value_t = 25_000)]
    iters_per_task: u64,

    /// Guest ACPI AML (`CSCN`/`CTFY`/`_MAT`) + Linux `CPUHP_AP_ONLINE_DYN` bringup latency per hotplugged wave in microseconds.
    #[arg(long, default_value_t = 2_500)]
    acpi_bringup_us: u64,

    /// Guest ACPI `_EJ0` (`CEJ0`) + Linux `stop_machine` CPU offline latency per removed wave in microseconds.
    #[arg(long, default_value_t = 1_500)]
    acpi_eject_us: u64,

    /// Run a fast preset suitable for automated CI tests.
    #[arg(long)]
    quick: bool,
}

/// Summary statistics for a latency distribution in microseconds.
#[derive(Debug, Clone, Copy)]
struct LatencyStats {
    mean_us: f64,
    p50_us: f64,
    p99_us: f64,
    max_us: f64,
}

impl LatencyStats {
    fn from_samples(samples_ns: &[u64]) -> Self {
        assert!(!samples_ns.is_empty(), "samples_ns cannot be empty");
        let mut sorted = samples_ns.to_vec();
        sorted.sort_unstable();
        let sum_ns: u128 = sorted.iter().map(|&x| u128::from(x)).sum();
        let mean_us = (sum_ns as f64) / (sorted.len() as f64) / 1_000.0;
        let p50_idx = sorted.len() / 2;
        let p99_idx = ((sorted.len() * 99) / 100).min(sorted.len() - 1);
        let p50_us = (sorted[p50_idx] as f64) / 1_000.0;
        let p99_us = (sorted[p99_idx] as f64) / 1_000.0;
        let max_us = (*sorted.last().unwrap() as f64) / 1_000.0;
        Self {
            mean_us,
            p50_us,
            p99_us,
            max_us,
        }
    }
}

/// Complete benchmark result for one elasticity mechanism (Pylon or ACPI Hotplug).
#[derive(Debug, Clone)]
struct BenchmarkResult {
    scale_out: LatencyStats,
    scale_in: LatencyStats,
    total_wall_ms: f64,
    throughput_tasks_per_sec: f64,
    checksum: u64,
}

/// CPU-bound mixing kernel (SplitMix64 rounds) executed by active vCPU worker threads.
#[inline]
fn execute_compute_task(seed: u64, iterations: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    for _ in 0..iterations {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z = z ^ (z >> 31);
    }
    z
}

fn dup_fd(fd: RawFd) -> OwnedFd {
    // SAFETY: `libc::dup` duplicates a valid open file descriptor.
    let duped = unsafe { libc::dup(fd) };
    assert!(duped >= 0, "dup failed: {}", io::Error::last_os_error());
    // SAFETY: `duped` is a freshly duplicated valid file descriptor.
    unsafe { OwnedFd::from_raw_fd(duped) }
}

fn create_blocking_eventfd() -> OwnedFd {
    // SAFETY: `libc::eventfd(0, EFD_CLOEXEC)` creates a valid blocking eventfd.
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

/// Shared work queue used by active vCPU threads during each burst wave.
struct WaveWorkQueue {
    wave_id: AtomicU64,
    next_task: AtomicU32,
    total_tasks: AtomicU32,
    completed_tasks: AtomicU32,
    iters_per_task: u64,
    accum_checksum: AtomicU64,
}

impl WaveWorkQueue {
    fn new(iters_per_task: u64) -> Self {
        Self {
            wave_id: AtomicU64::new(0),
            next_task: AtomicU32::new(0),
            total_tasks: AtomicU32::new(0),
            completed_tasks: AtomicU32::new(0),
            iters_per_task,
            accum_checksum: AtomicU64::new(0),
        }
    }

    fn start_wave(&self, wave_id: u64, tasks: u32) {
        self.total_tasks.store(0, Ordering::SeqCst);
        self.wave_id.store(wave_id, Ordering::SeqCst);
        self.completed_tasks.store(0, Ordering::SeqCst);
        self.next_task.store(0, Ordering::SeqCst);
        self.total_tasks.store(tasks, Ordering::SeqCst);
    }

    fn finish_wave(&self) {
        self.total_tasks.store(0, Ordering::SeqCst);
    }

    #[inline]
    fn try_execute_one(&self) -> bool {
        let total = self.total_tasks.load(Ordering::Acquire);
        if total == 0 || self.next_task.load(Ordering::Relaxed) >= total {
            return false;
        }
        let idx = self.next_task.fetch_add(1, Ordering::AcqRel);
        if idx < total {
            let wave_id = self.wave_id.load(Ordering::Acquire);
            let digest =
                execute_compute_task((wave_id << 32) | u64::from(idx), self.iters_per_task);
            self.accum_checksum.fetch_xor(digest, Ordering::Relaxed);
            self.completed_tasks.fetch_add(1, Ordering::Release);
            true
        } else {
            false
        }
    }
}

/// Runs the workload using Pylon microsecond core elasticity (`PylonSharedPage` + `MatrixEngine` + `EventFd` fast-park/wake).
fn run_pylon_elastic_benchmark(
    waves: usize,
    max_vcpus: u32,
    tasks_per_wave: usize,
    iters_per_task: u64,
) -> BenchmarkResult {
    let shm_path: PathBuf = temp_dir().join(format!("pylon-bench-{}.shm", process::id()));
    let shm_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&shm_path)
        .expect("failed to create benchmark shm file");
    shm_file
        .set_len(PYLON_SHARED_PAGE_SIZE as u64)
        .expect("failed to size benchmark shm file");

    // SAFETY: `shm_file` is 8,192 bytes and accessed atomically via `PylonSharedPage`.
    let mut shm_mmap = unsafe { MmapMut::map_mut(&shm_file).expect("mmap failed") };
    // SAFETY: `shm_mmap` is 8,192 bytes and page-aligned.
    let page = unsafe { &mut *(shm_mmap.as_mut_ptr().cast::<PylonSharedPage>()) };
    page.init(1, max_vcpus, DEFAULT_TSC_KHZ);

    let stop = Arc::new(AtomicBool::new(false));
    let work_queue = Arc::new(WaveWorkQueue::new(iters_per_task));

    let mut handle_kick_fds = Vec::with_capacity(max_vcpus as usize);
    let mut handle_wake_fds = Vec::with_capacity(max_vcpus as usize);
    let mut local_wake_fds = Vec::with_capacity(max_vcpus as usize);
    let mut vcpu_threads = Vec::with_capacity(max_vcpus as usize);

    for vcpu_idx in 0..(max_vcpus as usize) {
        let kick_fd = create_eventfd().expect("kick_fd");
        let wake_fd = create_blocking_eventfd();
        handle_kick_fds.push(dup_fd(kick_fd.as_raw_fd()));
        handle_wake_fds.push(dup_fd(wake_fd.as_raw_fd()));
        let thread_wake_fd = dup_fd(wake_fd.as_raw_fd());
        local_wake_fds.push(wake_fd);

        let thread_file = shm_file.try_clone().expect("clone shm_file");
        // SAFETY: Mapping the 8,192-byte shared memory page into the vCPU worker thread.
        let thread_mmap = unsafe { MmapMut::map_mut(&thread_file).expect("thread mmap") };
        let thread_stop = Arc::clone(&stop);
        let thread_queue = Arc::clone(&work_queue);

        let th = thread::Builder::new()
            .name(format!("pylon-bench-vcpu{vcpu_idx}"))
            .spawn(move || {
                // SAFETY: `thread_mmap` is 8,192 bytes and page-aligned.
                let shared = unsafe { &*(thread_mmap.as_ptr().cast::<PylonSharedPage>()) };
                let slot = &shared.vcpus[vcpu_idx];
                // SAFETY: `libc::gettid()` is always safe to call.
                let tid = unsafe { libc::gettid() as u32 };
                let mut parked = vcpu_idx != 0;
                let initial_state = if parked {
                    VCPU_STATE_PARKED
                } else {
                    VCPU_STATE_RUNNING
                };
                slot.state.store(initial_state, Ordering::Release);
                slot.host_tid.store(tid, Ordering::Release);

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

                    if slot.state.load(Ordering::Acquire) != VCPU_STATE_RUNNING {
                        slot.state.store(VCPU_STATE_RUNNING, Ordering::Release);
                    }

                    // Check cooperative revocation request (`revoke_requested == 1`).
                    if slot.revoke_requested.load(Ordering::Acquire) == 1 {
                        slot.revoke_requested.store(0, Ordering::Release);
                        slot.assigned_pcpu.store(PCPU_UNASSIGNED, Ordering::Release);
                        slot.yield_count.fetch_add(1, Ordering::AcqRel);
                        slot.state.store(VCPU_STATE_PARKED, Ordering::Release);
                        parked = true;
                        continue;
                    }

                    if !thread_queue.try_execute_one() {
                        thread::yield_now();
                    }
                }
            })
            .expect("spawn pylon vcpu thread");
        vcpu_threads.push(th);
    }

    // Wait for all vCPU threads to register their `host_tid`.
    // SAFETY: `shm_mmap` is 8,192 bytes and page-aligned.
    let shared = unsafe { &*(shm_mmap.as_ptr().cast::<PylonSharedPage>()) };
    for slot in shared.vcpus.iter().take(max_vcpus as usize) {
        while slot.host_tid.load(Ordering::Acquire) == 0 {
            thread::yield_now();
        }
    }

    let req = VmRegistrationRequest {
        vm_id: 1,
        num_vcpus: max_vcpus,
        min_cores: 1,
        max_cores: max_vcpus,
        priority_weight: 100,
    };
    let vm_handle = VmHandle::new(
        &req,
        dup_fd(shm_file.as_raw_fd()),
        handle_kick_fds,
        handle_wake_fds,
    )
    .expect("VmHandle::new");

    let vm_snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(vec![vm_handle]));
    let pcpu_owners = Arc::new(PcpuOwnerTable::new_in_memory());
    let tenant_pcpus: Vec<usize> = (0..(max_vcpus as usize)).collect();
    let mut engine = MatrixEngine::new(
        Arc::clone(&vm_snapshot),
        Arc::clone(&pcpu_owners),
        &tenant_pcpus,
    );

    // Initialize baseline: vCPU 0 owns pCPU 0 (`runnable_threads = 1`).
    shared.vcpus[0].assigned_pcpu.store(0, Ordering::Release);
    engine.set_core_state(0, CoreState::Assigned { vm_id: 1, vcpu: 0 });

    let mut scale_out_samples_ns = Vec::with_capacity(waves);
    let mut scale_in_samples_ns = Vec::with_capacity(waves);
    let all_parked_mask = ((1u64 << max_vcpus) - 1) & !1u64;

    let wall_start = Instant::now();

    for wave in 0..waves {
        // 1. Scale-Out: 1 -> `max_vcpus` via shared-memory telemetry + `MatrixEngine` arbitration sweep.
        let t_out = Instant::now();
        shared
            .telemetry
            .stranded_work_bitmap
            .store(all_parked_mask, Ordering::Release);
        shared
            .telemetry
            .head_wait_tsc
            .store(read_tsc().saturating_sub(10_000), Ordering::Release);
        shared
            .telemetry
            .runnable_threads
            .store(max_vcpus, Ordering::Release);

        engine.poll_once(read_tsc());

        while (0..(max_vcpus as usize))
            .any(|i| shared.vcpus[i].state.load(Ordering::Acquire) != VCPU_STATE_RUNNING)
        {
            thread::yield_now();
        }
        scale_out_samples_ns.push(t_out.elapsed().as_nanos() as u64);

        // 2. Execute the wave's parallel compute tasks across all running vCPUs.
        work_queue.start_wave(wave as u64, tasks_per_wave as u32);
        while work_queue.completed_tasks.load(Ordering::Acquire) < (tasks_per_wave as u32) {
            spin_loop();
        }
        work_queue.finish_wave();

        // 3. Scale-In: `max_vcpus` -> 1 via cooperative `revoke_requested = 1` fast-park.
        let t_in = Instant::now();
        shared
            .telemetry
            .runnable_threads
            .store(1, Ordering::Release);
        for slot in shared.vcpus.iter().take(max_vcpus as usize).skip(1) {
            slot.revoke_requested.store(1, Ordering::Release);
        }
        while (1..(max_vcpus as usize))
            .any(|i| shared.vcpus[i].state.load(Ordering::Acquire) != VCPU_STATE_PARKED)
        {
            thread::yield_now();
        }
        for pcpu in 1..(max_vcpus as usize) {
            engine.set_core_state(pcpu, CoreState::Idle);
        }
        scale_in_samples_ns.push(t_in.elapsed().as_nanos() as u64);
    }

    let total_wall_ms = wall_start.elapsed().as_secs_f64() * 1_000.0;
    let total_tasks = (waves * tasks_per_wave) as f64;
    let throughput_tasks_per_sec = total_tasks / (total_wall_ms / 1_000.0);

    // Clean shutdown.
    stop.store(true, Ordering::Release);
    for fd in &local_wake_fds {
        let _ = write_eventfd(fd.as_raw_fd());
    }
    for th in vcpu_threads {
        let _ = th.join();
    }
    let _ = fs::remove_file(&shm_path);

    BenchmarkResult {
        scale_out: LatencyStats::from_samples(&scale_out_samples_ns),
        scale_in: LatencyStats::from_samples(&scale_in_samples_ns),
        total_wall_ms,
        throughput_tasks_per_sec,
        checksum: work_queue.accum_checksum.load(Ordering::Acquire),
    }
}

/// Models Cloud Hypervisor's `AcpiCpuHotplugController` (`_SB_.PRES` MMIO registers + `vcpu_states` mutex).
struct AcpiHotplugState {
    selected_cpu: u32,
    inserting: Vec<bool>,
    removing: Vec<bool>,
}

/// Runs the identical workload using the Cloud Hypervisor ACPI CPU Hotplug lifecycle
/// (`CpuManager::resize` -> thread spawn + barrier + `_SB_.PRES` `CSCN`/`CTFY` MMIO scan + guest `cpuhp` bringup -> `_EJ0` eject & thread join).
fn run_acpi_hotplug_benchmark(
    waves: usize,
    max_vcpus: u32,
    tasks_per_wave: usize,
    iters_per_task: u64,
    acpi_bringup_us: u64,
    acpi_eject_us: u64,
) -> BenchmarkResult {
    let stop_boot_vcpu = Arc::new(AtomicBool::new(false));
    let work_queue = Arc::new(WaveWorkQueue::new(iters_per_task));
    let acpi_state = Arc::new(Mutex::new(AcpiHotplugState {
        selected_cpu: 0,
        inserting: vec![false; max_vcpus as usize],
        removing: vec![false; max_vcpus as usize],
    }));

    // Boot vCPU 0 stays online throughout the benchmark.
    let boot_stop = Arc::clone(&stop_boot_vcpu);
    let boot_queue = Arc::clone(&work_queue);
    let boot_thread = thread::Builder::new()
        .name("acpi-vcpu0".to_string())
        .spawn(move || {
            while !boot_stop.load(Ordering::Acquire) {
                if !boot_queue.try_execute_one() {
                    spin_loop();
                }
            }
        })
        .expect("spawn boot vcpu");

    let mut scale_out_samples_ns = Vec::with_capacity(waves);
    let mut scale_in_samples_ns = Vec::with_capacity(waves);

    let wall_start = Instant::now();

    for wave in 0..waves {
        // 1. Scale-Out via ACPI CPU Hotplug (`CpuManager::activate_vcpus` + AML `CSCN` scan + Linux `cpuhp` online).
        let t_out = Instant::now();
        let hotplug_count = (max_vcpus - 1) as usize;
        let barrier = Arc::new(Barrier::new(hotplug_count + 1));
        let mut hotplug_handles = Vec::with_capacity(hotplug_count);
        let mut kill_flags = Vec::with_capacity(hotplug_count);

        for vcpu_id in 1..(max_vcpus as usize) {
            {
                let mut st = acpi_state.lock().unwrap();
                st.inserting[vcpu_id] = true;
            }
            let kill = Arc::new(AtomicBool::new(false));
            kill_flags.push(Arc::clone(&kill));
            let b = Arc::clone(&barrier);
            let wq = Arc::clone(&work_queue);

            let handle = thread::Builder::new()
                .name(format!("acpi-hotplug-vcpu{vcpu_id}"))
                .spawn(move || {
                    b.wait();
                    while !kill.load(Ordering::Acquire) {
                        if !wq.try_execute_one() {
                            spin_loop();
                        }
                    }
                })
                .expect("spawn hotplug vcpu");
            hotplug_handles.push(handle);
        }

        // Unblock barrier (`CpuManager::activate_vcpus`).
        barrier.wait();

        // Execute the `_SB_.PRES` AML `CSCN` MMIO scan across `0..max_vcpus` and guest `cpuhp` bringup.
        for vcpu_id in 1..(max_vcpus as usize) {
            let mut st = acpi_state.lock().unwrap();
            st.selected_cpu = vcpu_id as u32;
            if st.inserting[vcpu_id] {
                st.inserting[vcpu_id] = false;
            }
        }
        if acpi_bringup_us > 0 {
            thread::sleep(Duration::from_micros(acpi_bringup_us));
        }
        scale_out_samples_ns.push(t_out.elapsed().as_nanos() as u64);

        // 2. Execute the wave's parallel compute tasks across all online vCPUs.
        work_queue.start_wave(wave as u64, tasks_per_wave as u32);
        while work_queue.completed_tasks.load(Ordering::Acquire) < (tasks_per_wave as u32) {
            spin_loop();
        }
        work_queue.finish_wave();

        // 3. Scale-In via ACPI CPU Hot-Unplug (`mark_vcpus_for_removal` + `CRMV`/`_EJ0` MMIO + thread join).
        let t_in = Instant::now();
        for vcpu_id in 1..(max_vcpus as usize) {
            let mut st = acpi_state.lock().unwrap();
            st.removing[vcpu_id] = true;
        }
        if acpi_eject_us > 0 {
            thread::sleep(Duration::from_micros(acpi_eject_us));
        }
        for (idx, kill) in kill_flags.iter().enumerate() {
            let vcpu_id = idx + 1;
            {
                let mut st = acpi_state.lock().unwrap();
                st.selected_cpu = vcpu_id as u32;
                st.removing[vcpu_id] = false;
            }
            kill.store(true, Ordering::Release);
        }
        for handle in hotplug_handles {
            let _ = handle.join();
        }
        scale_in_samples_ns.push(t_in.elapsed().as_nanos() as u64);
    }

    let total_wall_ms = wall_start.elapsed().as_secs_f64() * 1_000.0;
    let total_tasks = (waves * tasks_per_wave) as f64;
    let throughput_tasks_per_sec = total_tasks / (total_wall_ms / 1_000.0);

    stop_boot_vcpu.store(true, Ordering::Release);
    let _ = boot_thread.join();

    BenchmarkResult {
        scale_out: LatencyStats::from_samples(&scale_out_samples_ns),
        scale_in: LatencyStats::from_samples(&scale_in_samples_ns),
        total_wall_ms,
        throughput_tasks_per_sec,
        checksum: work_queue.accum_checksum.load(Ordering::Acquire),
    }
}

fn print_comparison_report(args: &BenchArgs, pylon: &BenchmarkResult, acpi: &BenchmarkResult) {
    let scale_out_speedup = acpi.scale_out.mean_us / pylon.scale_out.mean_us.max(0.001);
    let scale_in_speedup = acpi.scale_in.mean_us / pylon.scale_in.mean_us.max(0.001);
    let throughput_speedup =
        pylon.throughput_tasks_per_sec / acpi.throughput_tasks_per_sec.max(0.001);

    println!("================================================================================");
    println!(" Pylon Microsecond Core Elasticity vs. ACPI CPU Hotplug Benchmark Report");
    println!("================================================================================");
    println!(
        " Workload Config : {} burst waves | 1 <-> {} vCPUs | {} tasks/wave ({} iters/task)",
        args.waves, args.max_vcpus, args.tasks_per_wave, args.iters_per_task
    );
    println!(
        " Checksum Match  : Pylon={:#018x} | ACPI={:#018x}",
        pylon.checksum, acpi.checksum
    );
    println!("--------------------------------------------------------------------------------");
    println!(
        "| {:<30} | {:>16} | {:>16} | {:>10} |",
        "Metric", "Pylon Elastic", "ACPI Hotplug", "Improvement"
    );
    println!(
        "|--------------------------------|------------------|------------------|------------|"
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-Out Mean Latency",
        pylon.scale_out.mean_us,
        acpi.scale_out.mean_us,
        scale_out_speedup
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-Out p50 Latency",
        pylon.scale_out.p50_us,
        acpi.scale_out.p50_us,
        acpi.scale_out.p50_us / pylon.scale_out.p50_us.max(0.001)
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-Out p99 Latency",
        pylon.scale_out.p99_us,
        acpi.scale_out.p99_us,
        acpi.scale_out.p99_us / pylon.scale_out.p99_us.max(0.001)
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-Out Max Latency",
        pylon.scale_out.max_us,
        acpi.scale_out.max_us,
        acpi.scale_out.max_us / pylon.scale_out.max_us.max(0.001)
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-In (Revoke) Mean Latency",
        pylon.scale_in.mean_us,
        acpi.scale_in.mean_us,
        scale_in_speedup
    );
    println!(
        "| {:<30} | {:>13.2} us | {:>13.2} us | {:>8.1}x |",
        "Scale-In (Revoke) p99 Latency",
        pylon.scale_in.p99_us,
        acpi.scale_in.p99_us,
        acpi.scale_in.p99_us / pylon.scale_in.p99_us.max(0.001)
    );
    println!(
        "| {:<30} | {:>13.2} ms | {:>13.2} ms | {:>8.2}x |",
        "Total Workload Wall Time",
        pylon.total_wall_ms,
        acpi.total_wall_ms,
        acpi.total_wall_ms / pylon.total_wall_ms.max(0.001)
    );
    println!(
        "| {:<30} | {:>10.1} ops/s | {:>10.1} ops/s | {:>8.2}x |",
        "Effective Workload Throughput",
        pylon.throughput_tasks_per_sec,
        acpi.throughput_tasks_per_sec,
        throughput_speedup
    );
    println!("================================================================================");
}

fn main() {
    let mut args = BenchArgs::parse();
    if args.quick {
        args.waves = 6;
        args.max_vcpus = 4;
        args.tasks_per_wave = 32;
        args.iters_per_task = 10_000;
        args.acpi_bringup_us = 1_500;
        args.acpi_eject_us = 1_000;
    }

    let pylon_res = run_pylon_elastic_benchmark(
        args.waves,
        args.max_vcpus,
        args.tasks_per_wave,
        args.iters_per_task,
    );
    let acpi_res = run_acpi_hotplug_benchmark(
        args.waves,
        args.max_vcpus,
        args.tasks_per_wave,
        args.iters_per_task,
        args.acpi_bringup_us,
        args.acpi_eject_us,
    );

    assert_eq!(
        pylon_res.checksum, acpi_res.checksum,
        "Workload checksums must match between Pylon and ACPI Hotplug"
    );
    print_comparison_report(&args, &pylon_res, &acpi_res);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_benchmark_pylon_vs_acpi_hotplug() {
        let pylon_res = run_pylon_elastic_benchmark(4, 4, 16, 5_000);
        let acpi_res = run_acpi_hotplug_benchmark(4, 4, 16, 5_000, 1_000, 1_000);

        assert_eq!(pylon_res.checksum, acpi_res.checksum);
        assert!(
            pylon_res.scale_out.mean_us < acpi_res.scale_out.mean_us,
            "Expected Pylon scale-out ({:.2} us) to be faster than ACPI hotplug ({:.2} us)",
            pylon_res.scale_out.mean_us,
            acpi_res.scale_out.mean_us
        );
        assert!(
            pylon_res.throughput_tasks_per_sec > acpi_res.throughput_tasks_per_sec,
            "Expected Pylon throughput ({:.1} ops/s) to exceed ACPI hotplug ({:.1} ops/s)",
            pylon_res.throughput_tasks_per_sec,
            acpi_res.throughput_tasks_per_sec
        );
    }
}
