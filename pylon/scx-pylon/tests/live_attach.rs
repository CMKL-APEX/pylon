// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};
use std::{fs, hint, io, mem, process, thread};

use scx_pylon::ScxPylonUserspace;
use scx_pylon::coresched::{CookieDenial, GuardMode, KernelCoreSched};

#[test]
#[ignore = "requires CAP_BPF / root"]
fn test_live_attach_and_detach() {
    let guard = KernelCoreSched::detect()
        .needs_guard(GuardMode::Auto)
        .then_some(CookieDenial::Eperm);
    let scx =
        ScxPylonUserspace::load_and_attach(guard).expect("Failed to load and attach scx_pylon");
    let state = fs::read_to_string("/sys/kernel/sched_ext/state").expect("read sched_ext state");
    assert_eq!(state.trim(), "enabled");
    println!("sched_ext state: {}", state.trim());

    // Test writing to owner map
    scx.owner_map()
        .set_owner(2, 12345, scx_pylon::PcpuOwnerFlags::ACTIVE)
        .expect("set owner");
    let entry = scx.owner_map().get_owner(2).expect("get owner");
    assert_eq!(entry.target_tid, 12345);
    assert!(
        scx_pylon::PcpuOwnerFlags::from_bits_retain(entry.flags)
            .contains(scx_pylon::PcpuOwnerFlags::ACTIVE)
    );

    // Drop scx to detach
    drop(scx);

    let state_after =
        fs::read_to_string("/sys/kernel/sched_ext/state").expect("read sched_ext state");
    assert_eq!(state_after.trim(), "disabled");
    println!("sched_ext state after detach: {}", state_after.trim());
}

/// Pins the calling thread to `cpu`.
fn pin_to(cpu: usize) {
    // SAFETY: `cpu_set_t` is plain data; all zeroes is the empty set.
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    // SAFETY: `set` is a valid `cpu_set_t`, and `cpu` is below `CPU_SETSIZE`.
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: `set` lives across the call, and the size passed is its size. Pid 0 is the
    // calling thread.
    let ret = unsafe { libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set) };
    assert_eq!(ret, 0, "sched_setaffinity: {}", io::Error::last_os_error());
}

/// The time a thread of this process has run so far, from its schedstat.
fn run_time(tid: i32) -> Duration {
    let stat = fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).expect("schedstat");
    let ns = stat.split_whitespace().next().expect("run time field");
    Duration::from_nanos(ns.parse().expect("run time in ns"))
}

/// Starts a thread that spins until `stop` is set. It sends its thread ID, then waits on `go`
/// before it pins itself to `cpu`.
fn spinner(cpu: usize, stop: &Arc<AtomicBool>, go: &Arc<Barrier>) -> (thread::JoinHandle<()>, i32) {
    let (tx, rx) = mpsc::channel();
    let (stop, go) = (Arc::clone(stop), Arc::clone(go));
    let handle = thread::spawn(move || {
        // SAFETY: gettid has no preconditions.
        tx.send(unsafe { libc::gettid() }).expect("send tid");
        go.wait();
        pin_to(cpu);
        while !stop.load(Ordering::Relaxed) {
            hint::spin_loop();
        }
    });
    (handle, rx.recv().expect("receive tid"))
}

/// A busy host task pinned to an owned CPU gets a fair share of it next to a busy owner. The
/// owner's enqueue at the end of its slice used to kick the CPU after the switch, which preempted
/// the pinned task each time it got the CPU, so it made almost no progress.
#[test]
#[ignore = "requires CAP_BPF / root"]
fn test_pinned_task_shares_cpu_with_busy_owner() {
    let cpu = thread::available_parallelism().map_or(1, |n| n.get() - 1);
    let guard = KernelCoreSched::detect()
        .needs_guard(GuardMode::Auto)
        .then_some(CookieDenial::Eperm);
    let scx =
        ScxPylonUserspace::load_and_attach(guard).expect("Failed to load and attach scx_pylon");

    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(Barrier::new(3));
    let (owner, owner_tid) = spinner(cpu, &stop, &go);
    let (pinned, pinned_tid) = spinner(cpu, &stop, &go);
    scx.owner_map()
        .set_owner(
            cpu,
            u32::try_from(owner_tid).expect("tid"),
            scx_pylon::PcpuOwnerFlags::ACTIVE,
        )
        .expect("set owner");
    go.wait();

    thread::sleep(Duration::from_millis(500));
    let start = (Instant::now(), run_time(owner_tid), run_time(pinned_tid));
    thread::sleep(Duration::from_secs(2));
    let elapsed = start.0.elapsed();
    let owner_ran = run_time(owner_tid) - start.1;
    let pinned_ran = run_time(pinned_tid) - start.2;

    stop.store(true, Ordering::Relaxed);
    owner.join().expect("owner thread");
    pinned.join().expect("pinned thread");
    scx.owner_map().clear_owner(cpu).expect("clear owner");
    drop(scx);

    println!("CPU {cpu} over {elapsed:?}: owner ran {owner_ran:?}, pinned task ran {pinned_ran:?}");
    assert!(
        pinned_ran > elapsed / 4,
        "the pinned task ran {pinned_ran:?} in {elapsed:?}"
    );
    assert!(
        owner_ran > elapsed / 4,
        "the owner ran {owner_ran:?} in {elapsed:?}"
    );
}

/// A vCPU thread that pylond makes the owner of a CPU while it waits on the vCPU queue runs on
/// that CPU, next to a woken parked vCPU of its VM. Enqueue doesn't run again for a queued
/// thread, so the owner wasn't on the CPU's owner queue: the woken vCPU kept the CPU, and other
/// CPUs left the owner alone, until the watchdog fired (spec 08 §7.3).
#[test]
#[ignore = "requires CAP_BPF / root"]
fn test_owner_given_its_cpu_while_queued_runs() {
    const SLOT: u16 = 5;
    const CYCLES: usize = 10;
    let cpu = thread::available_parallelism().map_or(1, |n| n.get() - 1);
    let guard = KernelCoreSched::detect()
        .needs_guard(GuardMode::Auto)
        .then_some(CookieDenial::Eperm);
    let scx =
        ScxPylonUserspace::load_and_attach(guard).expect("Failed to load and attach scx_pylon");
    let vcpus = scx.vcpu_domain_map().expect("vCPU domain map");
    let mut vm_vcpus = scx.vm_vcpus_mmap().expect("VM vCPU masks");

    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(Barrier::new(3));
    let (owner, owner_tid) = spinner(cpu, &stop, &go);
    let (woken, woken_tid) = spinner(cpu, &stop, &go);
    let tgid = process::id();
    let owner_tid_u32 = u32::try_from(owner_tid).expect("tid");
    vcpus
        .insert(owner_tid_u32, tgid, SLOT, 0)
        .expect("list owner");
    vcpus
        .insert(u32::try_from(woken_tid).expect("tid"), tgid, SLOT, 1)
        .expect("list woken vCPU");
    // Both vCPUs are parked with no pCPU, as pylond reports them.
    let parked = usize::from(SLOT) * 2 * mem::size_of::<u64>();
    vm_vcpus[parked..parked + 8].copy_from_slice(&0b11u64.to_ne_bytes());
    let flags = scx_pylon::PcpuOwnerFlags::from_bits_retain(
        scx_pylon::PcpuOwnerFlags::ACTIVE.bits()
            | ((u32::from(SLOT) + 1) << scx_pylon::PCPU_OWNER_SLOT_SHIFT),
    );
    go.wait();

    // The VM owns no CPU, so both threads wait on the vCPU queue in turn. Then pylond gives
    // the CPU to the first, which is as often queued as running at that moment.
    let mut starved = Vec::new();
    for cycle in 0..CYCLES {
        scx.owner_map().clear_owner(cpu).expect("clear owner");
        thread::sleep(Duration::from_millis(100));
        scx.owner_map()
            .set_owner(cpu, owner_tid_u32, flags)
            .expect("set owner");
        let start = (Instant::now(), run_time(owner_tid));
        thread::sleep(Duration::from_millis(300));
        let (elapsed, ran) = (start.0.elapsed(), run_time(owner_tid) - start.1);
        if ran < elapsed / 4 {
            starved.push((cycle, ran, elapsed));
        }
    }

    stop.store(true, Ordering::Relaxed);
    scx.owner_map().clear_owner(cpu).expect("clear owner");
    vm_vcpus[parked..parked + 8].copy_from_slice(&0u64.to_ne_bytes());
    owner.join().expect("owner thread");
    woken.join().expect("woken thread");
    vcpus.remove(owner_tid_u32).expect("unlist owner");
    vcpus
        .remove(u32::try_from(woken_tid).expect("tid"))
        .expect("unlist woken vCPU");
    drop(scx);

    assert!(
        starved.is_empty(),
        "the owner of CPU {cpu} ran less than a quarter of the time in cycles {starved:?}"
    );
}
