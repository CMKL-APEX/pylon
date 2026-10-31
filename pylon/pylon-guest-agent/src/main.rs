// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::error::Error;
use std::io::{self, ErrorKind};
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::Parser;
use log::{info, warn};
use pylon_guest_agent::{
    GuestAgentStatus, LinuxProcfsReader, PV_PYLON_DEVICE, PylonGuestAgent, PylonMemoryBackend,
};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;

#[derive(Parser, Debug)]
#[command(
    name = "pylon-guest-agent",
    about = "Pylon guest telemetry daemon",
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

    /// Guest CPU the agent runs on. It must be a vCPU that is never parked: if the agent sits
    /// on a parked vCPU it can't report demand, and nothing wakes that vCPU. Any of the VM's
    /// `initial_active_vcpus` will do; vCPU 0 always runs.
    #[arg(long, default_value_t = 0)]
    cpu: usize,

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

/// Restricts the calling thread to `cpu`.
fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(io::Error::from(ErrorKind::InvalidInput));
    }
    // SAFETY: `cpu_set_t` is plain old data that zero-initializes, `CPU_SET` stays within it
    // (`cpu` is below `CPU_SETSIZE`), and `sched_setaffinity(0, ..)` acts on the calling thread
    // with a valid mask pointer and size.
    let ret = unsafe {
        let mut set: libc::cpu_set_t = zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
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
    } else if Path::new(PV_PYLON_DEVICE).exists() {
        // `pv-pylon` maps the page itself, and x86 then refuses a `/dev/mem` mapping of it.
        info!("Connecting to Pylon shared memory via {PV_PYLON_DEVICE}");
        PylonMemoryBackend::from_file(PV_PYLON_DEVICE)?
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

    if let Err(err) = pin_to_cpu(args.cpu) {
        warn!(
            "Couldn't pin the agent to CPU {}: {err}; it may stall if its CPU is parked",
            args.cpu
        );
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The CPUs the calling thread may run on.
    fn allowed_cpus() -> Vec<usize> {
        // SAFETY: `cpu_set_t` is plain old data that zero-initializes, and `sched_getaffinity(0,
        // ..)` fills it for the calling thread given a valid pointer and size.
        let set = unsafe {
            let mut set: libc::cpu_set_t = zeroed();
            assert_eq!(
                libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set),
                0
            );
            set
        };
        // SAFETY: `CPU_ISSET` only reads within the set, for indexes below `CPU_SETSIZE`.
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
            .collect()
    }

    #[test]
    fn pin_to_cpu_restricts_the_calling_thread_to_that_cpu() {
        // On its own thread, so the test runner's threads keep their CPUs.
        thread::spawn(|| {
            let cpu = allowed_cpus()[0];
            pin_to_cpu(cpu).unwrap();
            assert_eq!(allowed_cpus(), [cpu]);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn pin_to_cpu_rejects_a_cpu_beyond_the_set() {
        assert_eq!(
            pin_to_cpu(libc::CPU_SETSIZE as usize).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}
