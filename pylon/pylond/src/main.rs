// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use log::{info, warn};
use memmap2::MmapMut;
use pylond::control::{ControlPlane, run_prometheus_server};
use pylond::engine::MatrixEngine;
use pylond::{DEFAULT_SOCKET_PATH, MAX_PCPUS, PcpuOwnerTable, VmSnapshot};
use tokio::runtime::Builder;
use tokio::sync::watch;

/// Pylon Node Core Arbiter Daemon (`pylond`).
#[derive(Debug, Parser)]
#[command(
    name = "pylond",
    version,
    about = "Pylon microsecond node core arbiter daemon"
)]
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

    /// Attach the live kernel sched_ext BPF dispatcher (scx-pylon), and fail if it can't be.
    /// Without this or `--no-scx`, it is attached when the kernel has sched_ext and no
    /// `--pcpu-map-file` is given, falling back to the in-memory table if attaching fails.
    #[arg(long, conflicts_with = "no_scx")]
    enable_scx: bool,

    /// Never attach the sched_ext BPF dispatcher.
    #[arg(long)]
    no_scx: bool,

    /// Optional TCP port to expose Prometheus metrics over HTTP (e.g. 9100).
    #[arg(long)]
    metrics_port: Option<u16>,

    /// Host address for Prometheus metrics HTTP server.
    #[arg(long, default_value = "127.0.0.1")]
    metrics_addr: String,
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

/// Present when the running kernel was built with `CONFIG_SCHED_CLASS_EXT`.
const SCHED_EXT_SYSFS: &str = "/sys/kernel/sched_ext";

/// Whether to attach scx-pylon at startup.
#[derive(Debug, PartialEq, Eq)]
enum ScxAttach {
    /// `--enable-scx`: attach, or exit.
    Required,
    /// The kernel has sched_ext: attach, or fall back to the in-memory table.
    Try,
    No,
}

fn scx_attach(args: &CliArgs, kernel_has_sched_ext: bool) -> ScxAttach {
    if args.enable_scx {
        ScxAttach::Required
    } else if args.no_scx || args.pcpu_map_file.is_some() || !kernel_has_sched_ext {
        ScxAttach::No
    } else {
        ScxAttach::Try
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let args = CliArgs::parse();
    let tenant_pcpus = parse_core_list(&args.cores)?;
    info!(
        "Starting pylond: socket={}, tenant_pcpus={tenant_pcpus:?}",
        args.socket.display()
    );

    let scx = match scx_attach(&args, Path::new(SCHED_EXT_SYSFS).exists()) {
        ScxAttach::Required => {
            info!("Loading and attaching scx_pylon sched_ext BPF dispatcher to kernel...");
            Some(
                scx_pylon::ScxPylonUserspace::load_and_attach()
                    .context("failed to load and attach scx_pylon sched_ext dispatcher")?,
            )
        }
        ScxAttach::Try => {
            info!("Kernel has sched_ext; attaching scx_pylon BPF dispatcher...");
            match scx_pylon::ScxPylonUserspace::load_and_attach() {
                Ok(scx) => Some(scx),
                Err(e) => {
                    warn!("scx_pylon not attached, using the in-memory pCPU table: {e:#}");
                    None
                }
            }
        }
        ScxAttach::No => None,
    };

    let (pcpu_owners, _scx_handle) = if let Some(scx) = scx {
        let (owner_map, link, obj) = scx.into_parts();
        info!("scx_pylon attached successfully; kernel sched_ext is active");
        let table = PcpuOwnerTable::from_mmap(owner_map.into_mmap())
            .context("failed to create PcpuOwnerTable from scx_pylon mmap")?;
        (Arc::new(table), Some((link, obj)))
    } else if let Some(ref path) = args.pcpu_map_file {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open pcpu map file {}", path.display()))?;
        // SAFETY: `file` is opened read/write for the shared `PcpuOwnerEntry` table.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        (Arc::new(PcpuOwnerTable::from_mmap(mmap)?), None)
    } else {
        (Arc::new(PcpuOwnerTable::new_in_memory()), None)
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

    let control = ControlPlane::new(args.socket, Arc::clone(&vm_snapshot));
    if let Some(port) = args.metrics_port {
        let bind_addr = format!("{}:{}", args.metrics_addr, port);
        let metrics_snapshot = Arc::clone(&vm_snapshot);
        let metrics_shutdown = shutdown_rx.clone();
        rt.spawn(async move {
            run_prometheus_server(bind_addr, metrics_snapshot, metrics_shutdown).await;
        });
    }
    let control_res = rt.block_on(control.run(shutdown_rx));

    stop_flag.store(true, Ordering::Release);
    let _ = shutdown_tx.send(true);
    let _ = engine_thread.join();

    control_res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::iter;

    use super::*;

    fn args(flags: &[&str]) -> CliArgs {
        CliArgs::try_parse_from(iter::once("pylond").chain(flags.iter().copied())).unwrap()
    }

    #[test]
    fn scx_attaches_when_the_kernel_has_sched_ext() {
        assert_eq!(scx_attach(&args(&[]), true), ScxAttach::Try);
        assert_eq!(scx_attach(&args(&[]), false), ScxAttach::No);
    }

    #[test]
    fn scx_flags_override_detection() {
        assert_eq!(
            scx_attach(&args(&["--enable-scx"]), false),
            ScxAttach::Required
        );
        assert_eq!(scx_attach(&args(&["--no-scx"]), true), ScxAttach::No);
        CliArgs::try_parse_from(["pylond", "--enable-scx", "--no-scx"]).unwrap_err();
    }

    #[test]
    fn a_pcpu_map_file_keeps_scx_off_unless_required() {
        let map = ["--pcpu-map-file", "/run/pylon/owners"];
        assert_eq!(scx_attach(&args(&map), true), ScxAttach::No);
        let required = [map[0], map[1], "--enable-scx"];
        assert_eq!(scx_attach(&args(&required), true), ScxAttach::Required);
    }
}
