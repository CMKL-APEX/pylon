// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::Parser;
use log::info;
use pylon_guest_agent::{GuestAgentStatus, LinuxProcfsReader, PylonGuestAgent, PylonMemoryBackend};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

#[derive(Parser, Debug)]
#[command(
    name = "pylon-guest-agent",
    about = "Pylon Guest Cooperation & Telemetry Daemon",
    version
)]
struct Args {
    /// Guest Physical Address (GPA) where Pylon shared memory is mapped (hex or dec)
    #[arg(long, default_value = "0xFEDF0000", value_parser = parse_hex_or_dec)]
    gpa: u64,

    /// Direct shared memory file path (used on host or mock test environments)
    #[arg(long)]
    shm: Option<PathBuf>,

    /// Polling interval in microseconds
    #[arg(long, default_value_t = 50_000)]
    interval_us: u64,

    /// Show current Pylon status and exit
    #[arg(long)]
    status: bool,

    /// Perform a single telemetry sampling pass and exit
    #[arg(long)]
    once: bool,
}

fn parse_hex_or_dec(s: &str) -> Result<u64, String> {
    if let Some(stripped) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(stripped, 16).map_err(|e| format!("invalid hex GPA: {e}"))
    } else {
        s.parse::<u64>().map_err(|e| format!("invalid GPA: {e}"))
    }
}

fn print_status(status: &GuestAgentStatus) {
    println!("========================================");
    println!("Pylon Guest Agent Status");
    println!("========================================");
    println!("VM ID:            {}", status.vm_id);
    println!("vCPUs:            {}", status.num_vcpus);
    println!("TSC Frequency:    {} kHz", status.tsc_khz);
    println!("Runnable Threads: {}", status.runnable_threads);
    println!("Heartbeat:        {}", status.guest_heartbeat);
    println!("----------------------------------------");
    for vcpu in &status.vcpus {
        let pcpu_str = vcpu
            .assigned_pcpu
            .map_or_else(|| "unassigned".to_string(), |p| format!("pCPU {p}"));
        println!(
            "  vCPU {:02}: state={:<7} ({}) | yields={} | forced_preempts={}",
            vcpu.vcpu_id, vcpu.state_name, pcpu_str, vcpu.yield_count, vcpu.forced_preempt_count
        );
    }
    println!("========================================");
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    let args = Args::parse();

    let backend = if let Some(ref shm_path) = args.shm {
        info!("Connecting to Pylon shared memory file: {shm_path:?}");
        PylonMemoryBackend::from_file(shm_path)?
    } else {
        info!(
            "Connecting to Pylon shared memory via /dev/mem at GPA 0x{:X}",
            args.gpa
        );
        PylonMemoryBackend::from_dev_mem(args.gpa)?
    };

    let agent = PylonGuestAgent::new(backend, LinuxProcfsReader::new())?;

    if args.status {
        print_status(&agent.status());
        return Ok(());
    }

    if args.once {
        let status = agent.step()?;
        print_status(&status);
        return Ok(());
    }

    info!(
        "Pylon guest agent started for VM {} ({} vCPUs), interval = {} us",
        agent.header().vm_id,
        agent.header().num_vcpus,
        args.interval_us
    );

    // Register clean shutdown on SIGINT or SIGTERM
    let term = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&term))?;
    flag::register(SIGTERM, Arc::clone(&term))?;

    let interval = Duration::from_micros(args.interval_us);

    while !term.load(Ordering::Relaxed) {
        agent.step()?;
        thread::sleep(interval);
    }

    info!("Pylon guest agent received exit signal, shutting down cleanly.");
    Ok(())
}
