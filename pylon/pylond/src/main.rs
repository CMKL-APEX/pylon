// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::ffi::CString;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{io, mem, ptr, result, thread};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use log::{info, warn};
use memmap2::MmapMut;
use pylond::control::{ControlPlane, run_prometheus_server};
use pylond::engine::{DEFAULT_TSC_KHZ, MatrixEngine, calibrate_tsc_khz};
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

    /// Permission bits for the control socket, in octal (e.g. 660). Default: the process umask.
    #[arg(long, value_parser = parse_socket_mode)]
    socket_mode: Option<u32>,

    /// Group (name or GID) to own the control socket, for VMMs that don't run as root.
    #[arg(long, value_parser = parse_group)]
    socket_group: Option<u32>,

    /// Optional TCP port to expose Prometheus metrics over HTTP (e.g. 9100).
    #[arg(long)]
    metrics_port: Option<u16>,

    /// Host address for Prometheus metrics HTTP server.
    #[arg(long, default_value = "127.0.0.1")]
    metrics_addr: String,
}

fn parse_socket_mode(spec: &str) -> result::Result<u32, String> {
    let mode = u32::from_str_radix(spec.trim_start_matches("0o"), 8)
        .map_err(|e| format!("invalid octal mode {spec:?}: {e}"))?;
    if mode > 0o777 {
        return Err(format!("mode {spec} has bits beyond 0777"));
    }
    Ok(mode)
}

/// Resolves a group name or numeric GID.
fn parse_group(spec: &str) -> result::Result<u32, String> {
    if let Ok(gid) = spec.parse::<u32>() {
        return Ok(gid);
    }
    let name = CString::new(spec).map_err(|_| format!("invalid group name {spec:?}"))?;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    loop {
        // SAFETY: all-zero is a valid `group`, which `getgrnam_r` fills in.
        let mut group: libc::group = unsafe { mem::zeroed() };
        let mut found: *mut libc::group = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated, and `group`, `buf` and `found` are valid for writes
        // of the sizes passed; `getgrnam_r` is the reentrant form.
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                &mut group,
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        match rc {
            0 if found.is_null() => return Err(format!("no group named {spec:?}")),
            0 => return Ok(group.gr_gid),
            libc::ERANGE if buf.len() < 1024 * 1024 => buf.resize(buf.len() * 2, 0),
            err => {
                return Err(format!(
                    "looking up group {spec:?}: {}",
                    io::Error::from_raw_os_error(err)
                ));
            }
        }
    }
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

    // VMMs leave the header's TSC rate unset, so measure the host's once.
    let host_tsc_khz = calibrate_tsc_khz(Duration::from_millis(50));
    match host_tsc_khz {
        Some(tsc_khz) => info!("Host TSC rate: {tsc_khz} kHz"),
        None => warn!("Could not measure the host TSC rate; assuming {DEFAULT_TSC_KHZ} kHz"),
    }
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

    // scx_pylon keeps SMT siblings of a running vCPU free of other VMs, so the engine must not
    // give two VMs sibling threads of one core.
    let smt_ring = scx.as_ref().map(|_| scx_pylon::host_smt_ring());

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
            if let Some(tsc_khz) = host_tsc_khz {
                engine.set_host_tsc_khz(tsc_khz);
            }
            if let Some(ring) = smt_ring {
                engine.set_smt_ring(&ring);
            }
            engine.run(&engine_stop)
        })
        .context("failed to spawn Plane 2 engine thread")?;

    // Run Plane 1 (Control Plane on Host Core 0) using a single-threaded Tokio runtime.
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Plane 1 Tokio runtime")?;

    let control = ControlPlane::new(args.socket, Arc::clone(&vm_snapshot))
        .with_socket_permissions(args.socket_mode, args.socket_group);
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
    fn socket_mode_and_group_parse() {
        assert_eq!(parse_socket_mode("660"), Ok(0o660));
        assert_eq!(parse_socket_mode("0o640"), Ok(0o640));
        parse_socket_mode("1777").unwrap_err();
        parse_socket_mode("rw").unwrap_err();
        assert_eq!(parse_group("0"), Ok(0));
        assert_eq!(parse_group("root"), Ok(0));
        parse_group("no-such-group-pylon").unwrap_err();
    }

    #[test]
    fn socket_options_reach_the_parser() {
        let parsed = args(&["--socket-mode", "660", "--socket-group", "0"]);
        assert_eq!(parsed.socket_mode, Some(0o660));
        assert_eq!(parsed.socket_group, Some(0));
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
