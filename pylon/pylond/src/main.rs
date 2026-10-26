// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use log::info;
use memmap2::MmapMut;
use pylond::control::ControlPlane;
use pylond::engine::MatrixEngine;
use pylond::{DEFAULT_SOCKET_PATH, MAX_PCPUS, PcpuOwnerTable, VmSnapshot};
use tokio::runtime::Builder;
use tokio::sync::watch;

/// Pylon Node Core Arbiter Daemon (`pylond`).
#[derive(Debug, Parser)]
#[command(name = "pylond", version, about = "Pylon microsecond node core arbiter daemon")]
struct CliArgs {
    /// Unix domain socket path for Plane 1 control plane.
    #[arg(long, default_value = DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// Comma-separated list or ranges of tenant physical CPU cores managed by Plane 2 (e.g. "2-7" or "2,3,4,5").
    #[arg(long, default_value = "2-7")]
    cores: String,

    /// Optional path to a memory-mapped `pylon_pcpu_owners` backing file.
    #[arg(long)]
    pcpu_map_file: Option<PathBuf>,
}

fn parse_core_list(spec: &str) -> Result<Vec<usize>> {
    let mut cores = Vec::new();
    for part in spec.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((start_str, end_str)) = trimmed.split_once('-') {
            let start: usize = start_str.trim().parse().context("invalid start core")?;
            let end: usize = end_str.trim().parse().context("invalid end core")?;
            for c in start..=end {
                if c < MAX_PCPUS && !cores.contains(&c) {
                    cores.push(c);
                }
            }
        } else {
            let c: usize = trimmed.parse().context("invalid core id")?;
            if c < MAX_PCPUS && !cores.contains(&c) {
                cores.push(c);
            }
        }
    }
    Ok(cores)
}

fn main() -> Result<()> {
    env_logger::init();
    let args = CliArgs::parse();
    let tenant_pcpus = parse_core_list(&args.cores)?;
    info!(
        "Starting pylond: socket={}, tenant_pcpus={tenant_pcpus:?}",
        args.socket.display()
    );

    let pcpu_owners = if let Some(ref path) = args.pcpu_map_file {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open pcpu map file {}", path.display()))?;
        // SAFETY: `file` is opened read/write for the shared `PcpuOwnerEntry` table.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Arc::new(PcpuOwnerTable::from_mmap(mmap)?)
    } else {
        Arc::new(PcpuOwnerTable::new_in_memory())
    };

    let vm_snapshot: Arc<VmSnapshot> = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Launch Plane 2 (Real-Time Matrix Engine on Host Core 1).
    let engine_snapshot = Arc::clone(&vm_snapshot);
    let engine_owners = Arc::clone(&pcpu_owners);
    let engine_stop = Arc::clone(&stop_flag);
    let engine_thread = thread::Builder::new()
        .name("pylond-engine".to_owned())
        .spawn(move || {
            let mut engine = MatrixEngine::new(engine_snapshot, engine_owners, &tenant_pcpus);
            engine.run(&engine_stop)
        })
        .context("failed to spawn Plane 2 engine thread")?;

    // Run Plane 1 (Control Plane on Host Core 0) using a single-threaded Tokio runtime.
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Plane 1 Tokio runtime")?;

    let control = ControlPlane::new(args.socket, vm_snapshot);
    let control_res = rt.block_on(control.run(shutdown_rx));

    stop_flag.store(true, Ordering::Release);
    let _ = shutdown_tx.send(true);
    let _ = engine_thread.join();

    control_res?;
    Ok(())
}
