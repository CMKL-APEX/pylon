// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{fs, thread};

use clap::Parser;
use log::info;
use scx_pylon::coresched::{GuardMode, KernelCoreSched};
use scx_pylon::{MAX_PCPUS, PcpuOwnerFlags, PcpuOwnerMap, ScxPylonUserspace};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

#[derive(Parser, Debug)]
#[command(
    name = "scx_pylon",
    about = "Pylon zero-syscall sched_ext BPF dispatcher",
    version
)]
struct Args {
    /// Verify embedded BPF object structure and exit.
    #[arg(long)]
    verify_only: bool,

    /// Attach to live kernel sched_ext interface and run as standalone dispatcher daemon.
    #[arg(long)]
    attach: bool,

    /// Show current sched_ext status and pCPU ownership slots.
    #[arg(long)]
    status: bool,

    /// Direct path to a file-backed `pylon_pcpu_owners` shared memory file.
    #[arg(long)]
    shm: Option<PathBuf>,

    /// Refuse to attach while any task has a core scheduling cookie. `auto` does this
    /// only on kernels without the sched_ext core-scheduling fixes.
    #[arg(long, value_enum, default_value_t)]
    core_sched_guard: GuardMode,
}

fn print_sched_ext_sysfs() {
    println!("========================================");
    println!("Linux sched_ext Kernel Status");
    println!("========================================");
    let state = fs::read_to_string("/sys/kernel/sched_ext/state")
        .unwrap_or_else(|_| "unavailable (CONFIG_SCHED_EXT not enabled or no sysfs)".into());
    println!("State:         {}", state.trim());

    if let Ok(seq) = fs::read_to_string("/sys/kernel/sched_ext/enable_seq") {
        println!("Enable Seq:    {}", seq.trim());
    }
    if let Ok(rej) = fs::read_to_string("/sys/kernel/sched_ext/nr_rejected") {
        println!("Rejected Progs:{}", rej.trim());
    }
    println!("========================================");
}

fn print_owner_map(map: &PcpuOwnerMap) {
    println!("========================================");
    println!("Pylon pCPU Ownership Slots (256)");
    println!("========================================");
    let mut active_count = 0;
    for pcpu in 0..MAX_PCPUS {
        if let Ok(entry) = map.get_owner(pcpu) {
            let has_owner = entry.target_tid != 0 || entry.flags != 0;
            if has_owner {
                let flags = PcpuOwnerFlags::from_bits_retain(entry.flags);
                let active = flags.contains(PcpuOwnerFlags::ACTIVE);
                let preempt = flags.contains(PcpuOwnerFlags::PREEMPT);
                println!(
                    "  pCPU {:03}: target_tid = {:<7} | active = {:<5} | preempt = {}",
                    pcpu, entry.target_tid, active, preempt
                );
                active_count += 1;
            }
        }
    }
    if active_count == 0 {
        println!("  (all 256 physical CPU slots idle / unassigned)");
    }
    println!("========================================");
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    let args = Args::parse();

    if args.verify_only {
        let meta = ScxPylonUserspace::verify_embedded_object()?;
        println!("BPF Object Verification: OK");
        println!("  Map Name:    {}", meta.name);
        println!("  Max Entries: {}", meta.max_entries);
        println!("  Key Size:    {} bytes", meta.key_size);
        println!("  Value Size:  {} bytes", meta.value_size);
        println!("  Flags:       0x{:x}", meta.map_flags);
        return Ok(());
    }

    if args.status {
        print_sched_ext_sysfs();
        if let Some(ref shm_path) = args.shm {
            let map = PcpuOwnerMap::from_shm(shm_path)?;
            print_owner_map(&map);
        }
        return Ok(());
    }

    // Standalone attach mode
    let kernel = KernelCoreSched::detect();
    let guard = kernel.needs_guard(args.core_sched_guard);
    info!(
        "Core scheduling: {kernel}; guard {}",
        if guard { "on" } else { "off" }
    );
    info!("Loading and attaching scx_pylon to kernel sched_ext...");
    let scx = ScxPylonUserspace::load_and_attach(guard)?;
    info!("scx_pylon attached successfully; kernel sched_ext is enabled.");

    print_sched_ext_sysfs();
    print_owner_map(scx.owner_map());

    let term = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&term))?;
    flag::register(SIGTERM, Arc::clone(&term))?;

    info!("Dispatcher running; press Ctrl-C to detach.");
    while !term.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(200));
    }

    info!("Exit signal received; detaching scx_pylon from kernel.");
    drop(scx);
    info!("scx_pylon detached; returned to default Linux scheduler.");
    Ok(())
}
