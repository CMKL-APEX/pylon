// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::mem::{size_of, size_of_val, zeroed};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{PermissionsExt, chown};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::ptr::{copy_nonoverlapping, null_mut};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use log::{info, warn};
use pylon_abi::{MAX_VCPUS, PCPU_UNASSIGNED, VCPU_STATE_PARKED};
use tokio::io::Interest;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::{
    Error, PARK_LATENCY_BUCKETS_US, VmHandle, VmPolicyUpdateRequest, VmPolicyUpdateResponse,
    VmRegistrationRequest, VmRegistrationResponse, VmSnapshot, VmStats, VmStatsRequest,
    VmStatsResponse, pin_current_thread_to_core,
};

/// Maximum number of file descriptors passed in a single `SCM_RIGHTS` registration message
/// (`1` shared memory fd + `MAX_VCPUS` kick irqfds + `MAX_VCPUS` wake eventfds = 129).
pub const MAX_REGISTRATION_FDS: usize = 1 + 2 * MAX_VCPUS;

/// Maximum JSON payload size for a `VmRegistrationRequest`.
const MAX_PAYLOAD_BYTES: usize = 4096;

/// Plane 1: Control Plane Unix domain socket server.
///
/// Runs on Host Core 0, accepts incoming VM registrations with `SCM_RIGHTS` file descriptors,
/// memory-maps each VM's `PylonSharedPage`, and publishes immutable `Arc<Vec<VmHandle>>`
/// snapshots to Plane 2 via `ArcSwap`.
pub struct ControlPlane {
    socket_path: PathBuf,
    vm_snapshot: Arc<VmSnapshot>,
    socket_mode: Option<u32>,
    socket_gid: Option<u32>,
    vcpu_domains: Option<Box<dyn VcpuDomainTable>>,
}

/// Where Plane 1 lists registered VMs' vCPU threads with their VMM's PID: `scx_pylon`'s
/// `pylon_vcpu_dom` map, which makes `scx_pylon` keep a listed thread's SMT siblings apart
/// before the thread ever owns a pCPU.
pub trait VcpuDomainTable: Send + Sync {
    /// Lists thread `tid` as a vCPU of the VMM process `pid`.
    fn insert(&self, tid: u32, pid: u32) -> scx_pylon::Result<()>;
    /// Removes thread `tid`.
    fn remove(&self, tid: u32) -> scx_pylon::Result<()>;
}

impl VcpuDomainTable for scx_pylon::VcpuDomainMap {
    fn insert(&self, tid: u32, pid: u32) -> scx_pylon::Result<()> {
        scx_pylon::VcpuDomainMap::insert(self, tid, pid)
    }

    fn remove(&self, tid: u32) -> scx_pylon::Result<()> {
        scx_pylon::VcpuDomainMap::remove(self, tid)
    }
}

/// Brings `table` in line with `current`, the vCPU threads of the registered VMs by thread
/// ID, with their VMM's PID. `listed` holds what the table holds, so only changes are
/// written. A thread that fails to be listed is tried again next time.
pub fn sync_vcpu_domains(
    table: &dyn VcpuDomainTable,
    listed: &mut HashMap<u32, u32>,
    current: HashMap<u32, u32>,
) {
    listed.retain(|tid, pid| {
        if current.get(tid) == Some(pid) {
            return true;
        }
        if let Err(err) = table.remove(*tid) {
            warn!("Failed to remove vCPU thread {tid} from scx_pylon: {err}");
        }
        false
    });
    for (tid, pid) in current {
        if listed.contains_key(&tid) {
            continue;
        }
        match table.insert(tid, pid) {
            Ok(()) => {
                listed.insert(tid, pid);
            }
            Err(err) => warn!("Failed to list vCPU thread {tid} in scx_pylon: {err}"),
        }
    }
}

/// How often Plane 1 looks for VMs whose VMM has exited.
const VM_PRUNE_INTERVAL: Duration = Duration::from_secs(1);

impl ControlPlane {
    /// Creates a new `ControlPlane` bound to `socket_path` and updating `vm_snapshot`.
    pub fn new(socket_path: impl Into<PathBuf>, vm_snapshot: Arc<VmSnapshot>) -> Self {
        Self {
            socket_path: socket_path.into(),
            vm_snapshot,
            socket_mode: None,
            socket_gid: None,
            vcpu_domains: None,
        }
    }

    /// Lists registered VMs' vCPU threads in `table`, so `scx_pylon` isolates them from the
    /// start. The list is brought up to date once a second.
    #[must_use]
    pub fn with_vcpu_domains(mut self, table: impl VcpuDomainTable + 'static) -> Self {
        self.vcpu_domains = Some(Box::new(table));
        self
    }

    /// Sets the socket's permission bits and group once it is created. Without them, the
    /// socket keeps the process umask and group, which root's umask makes unwritable for
    /// non-root VMMs.
    #[must_use]
    pub fn with_socket_permissions(mut self, mode: Option<u32>, gid: Option<u32>) -> Self {
        self.socket_mode = mode;
        self.socket_gid = gid;
        self
    }

    /// Returns a reference to the shared `VmSnapshot` (`ArcSwap<Vec<VmHandle>>`).
    pub fn vm_snapshot(&self) -> &Arc<VmSnapshot> {
        &self.vm_snapshot
    }

    /// Pins the calling thread to Host Core 0 and runs the asynchronous registration server
    /// until `shutdown_rx` signals `true`.
    pub async fn run(&self, mut shutdown_rx: watch::Receiver<bool>) -> Result<(), Error> {
        if let Err(err) = pin_current_thread_to_core(0) {
            warn!("Plane 1 could not pin to Core 0: {err}");
        }

        if let Some(parent) = self.socket_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        if self.socket_path.exists() {
            fs::remove_file(&self.socket_path).map_err(Error::Io)?;
        }

        let listener = UnixListener::bind(&self.socket_path).map_err(Error::Io)?;
        if let Some(gid) = self.socket_gid {
            chown(&self.socket_path, None, Some(gid)).map_err(Error::Io)?;
        }
        if let Some(mode) = self.socket_mode {
            fs::set_permissions(&self.socket_path, fs::Permissions::from_mode(mode))
                .map_err(Error::Io)?;
        }
        info!(
            "Plane 1 (Control) listening on {}",
            self.socket_path.display()
        );

        let mut prune = interval(VM_PRUNE_INTERVAL);
        prune.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut listed_vcpus = HashMap::new();
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = prune.tick() => {
                    Self::remove_exited_vms(&self.vm_snapshot);
                    for vm in self.vm_snapshot.load().iter() {
                        let asked = vm.park_booted_vcpus();
                        if asked > 0 {
                            info!("VM {}: asked {asked} booted vCPU(s) to park", vm.vm_id());
                        }
                    }
                    if let Some(table) = &self.vcpu_domains {
                        let current = self
                            .vm_snapshot
                            .load()
                            .iter()
                            .flat_map(VmHandle::vcpu_threads)
                            .collect();
                        sync_vcpu_domains(table.as_ref(), &mut listed_vcpus, current);
                    }
                }
                accept_res = listener.accept() => {
                    let (stream, _) = accept_res.map_err(Error::Io)?;
                    let snapshot = Arc::clone(&self.vm_snapshot);
                    tokio::spawn(async move {
                        if let Err(err) = handle_client_connection(stream, snapshot).await {
                            warn!("Failed to handle VM registration client: {err}");
                        }
                    });
                }
            }
        }

        let _ = fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Removes VMs whose VMM process has exited. Plane 2 then frees their cores. Returns the
    /// number removed.
    pub fn remove_exited_vms(vm_snapshot: &VmSnapshot) -> usize {
        let current = vm_snapshot.load();
        let (exited, alive): (Vec<VmHandle>, Vec<VmHandle>) = current
            .iter()
            .cloned()
            .partition(|vm| vm.vmm_exited() == Some(true));
        for vm in &exited {
            info!("VMM of VM {} exited; removed its registration", vm.vm_id());
        }
        if !exited.is_empty() {
            vm_snapshot.store(Arc::new(alive));
        }
        exited.len()
    }

    /// Registers a `VmHandle` directly into the `ArcSwap` snapshot, replacing any existing entry
    /// with the same `vm_id`.
    pub fn publish_vm_handle(vm_snapshot: &VmSnapshot, handle: VmHandle) {
        let current = vm_snapshot.load();
        let mut next: Vec<VmHandle> = current
            .iter()
            .filter(|existing| existing.vm_id() != handle.vm_id())
            .cloned()
            .collect();
        next.push(handle);
        vm_snapshot.store(Arc::new(next));
    }
}

async fn handle_client_connection(
    stream: UnixStream,
    vm_snapshot: Arc<VmSnapshot>,
) -> Result<(), Error> {
    let (payload, mut fds) = recv_msg_with_fds_async(&stream).await?;

    if fds.is_empty() {
        if let Ok(stats_req) = serde_json::from_slice::<VmStatsRequest>(&payload)
            && (stats_req.action.as_deref() == Some("stats")
                || stats_req.action.as_deref() == Some("metrics")
                || stats_req.action.as_deref() == Some("status"))
        {
            let snapshot = vm_snapshot.load();
            let mut vms_stats = Vec::new();
            for vm in snapshot.iter() {
                if let Some(target_id) = stats_req.vm_id
                    && vm.vm_id() != target_id
                {
                    continue;
                }
                let page = vm.shared_page();
                let mut current_cores = 0u32;
                let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
                for vcpu_slot in page.vcpus.iter().take(num_vcpus) {
                    if vcpu_slot.assigned_pcpu.load(Ordering::Acquire) != PCPU_UNASSIGNED
                        && vcpu_slot.state.load(Ordering::Acquire) != VCPU_STATE_PARKED
                    {
                        current_cores += 1;
                    }
                }
                vms_stats.push(VmStats {
                    vm_id: vm.vm_id(),
                    num_vcpus: vm.num_vcpus(),
                    min_cores: vm.min_cores(),
                    max_cores: vm.max_cores(),
                    priority_weight: vm.priority_weight(),
                    current_cores,
                    allocated_core_microseconds: vm.allocated_core_usec(),
                    cooperative_yields: vm.cooperative_yield_count(),
                    forced_preemptions: vm.forced_preempt_count(),
                    runnable_threads: page.telemetry.runnable_threads.load(Ordering::Acquire),
                    handoffs_without_park: vm.handoff_without_park_count(),
                });
            }
            let resp = VmStatsResponse {
                accepted: true,
                message: "stats retrieved".to_owned(),
                vms: vms_stats,
            };
            return send_json_response_async(&stream, &resp).await;
        }

        match serde_json::from_slice::<VmPolicyUpdateRequest>(&payload) {
            Ok(req) => {
                let snapshot = vm_snapshot.load();
                let resp = if let Some(vm) = snapshot.iter().find(|v| v.vm_id() == req.vm_id) {
                    match vm.update_policy(req.min_cores, req.max_cores, req.priority_weight) {
                        Ok((min, max, weight)) => {
                            info!(
                                "Updated policy for VM {} (min_cores={}, max_cores={}, weight={})",
                                req.vm_id, min, max, weight
                            );
                            VmPolicyUpdateResponse {
                                accepted: true,
                                message: "policy updated".to_owned(),
                                min_cores: Some(min),
                                max_cores: Some(max),
                                priority_weight: Some(weight),
                            }
                        }
                        Err(err) => VmPolicyUpdateResponse {
                            accepted: false,
                            message: err.to_string(),
                            min_cores: None,
                            max_cores: None,
                            priority_weight: None,
                        },
                    }
                } else {
                    VmPolicyUpdateResponse {
                        accepted: false,
                        message: format!("VM {} not found", req.vm_id),
                        min_cores: None,
                        max_cores: None,
                        priority_weight: None,
                    }
                };
                send_json_response_async(&stream, &resp).await
            }
            Err(err) => {
                let resp = VmPolicyUpdateResponse {
                    accepted: false,
                    message: format!("invalid control request: {err}"),
                    min_cores: None,
                    max_cores: None,
                    priority_weight: None,
                };
                send_json_response_async(&stream, &resp).await
            }
        }
    } else {
        let req: VmRegistrationRequest = serde_json::from_slice(&payload).map_err(Error::Json)?;
        let expected_vcpus = req.num_vcpus as usize;
        let expected_fds = 1 + 2 * expected_vcpus;
        let response = if fds.len() == expected_fds {
            let shm_fd = fds.remove(0);
            let wake_eventfds = fds.split_off(expected_vcpus);
            let kick_irqfds = fds;

            // `pylond` signals the registering VMM's vCPU threads when a revocation's deadline
            // passes, so bind the VM to the process at the other end of this socket.
            let peer_pid = stream.peer_cred().ok().and_then(|cred| cred.pid());
            let handle =
                VmHandle::new(&req, shm_fd, kick_irqfds, wake_eventfds).and_then(|handle| {
                    match peer_pid {
                        Some(pid) if pid > 0 => handle.with_vmm_pid(pid),
                        _ => Ok(handle),
                    }
                });
            // A `vm_id` stays taken while the VMM that registered it runs: replacing it would
            // leave that VM unmanaged. Plane 1 runs on one thread, and nothing below awaits,
            // so this check and the publish can't interleave with another registration.
            let taken = vm_snapshot
                .load()
                .iter()
                .any(|vm| vm.vm_id() == req.vm_id && vm.vmm_exited() != Some(true));
            let handle = if taken {
                Err(Error::InvalidRegistration {
                    vm_id: req.vm_id,
                    reason: "a running VMM already registered this vm_id".to_owned(),
                })
            } else {
                handle
            };
            match handle {
                Ok(handle) => {
                    info!(
                        "Registered VM {} (vcpus={}, min_cores={}, max_cores={}, weight={})",
                        req.vm_id, req.num_vcpus, req.min_cores, req.max_cores, req.priority_weight
                    );
                    ControlPlane::publish_vm_handle(&vm_snapshot, handle);
                    VmRegistrationResponse {
                        accepted: true,
                        message: "registered".to_owned(),
                    }
                }
                Err(err) => {
                    // The VMM doesn't wait for the reply, so say it here.
                    warn!("Rejected registration of VM {}: {err}", req.vm_id);
                    VmRegistrationResponse {
                        accepted: false,
                        message: err.to_string(),
                    }
                }
            }
        } else {
            let message = format!("expected {expected_fds} FDs, got {}", fds.len());
            warn!("Rejected registration of VM {}: {message}", req.vm_id);
            VmRegistrationResponse {
                accepted: false,
                message,
            }
        };

        match send_json_response_async(&stream, &response).await {
            // The VMM closes the socket without reading the reply.
            Err(Error::Io(err))
                if matches!(
                    err.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
            {
                Ok(())
            }
            res => res,
        }
    }
}

async fn recv_msg_with_fds_async(stream: &UnixStream) -> Result<(Vec<u8>, Vec<OwnedFd>), Error> {
    loop {
        stream.ready(Interest::READABLE).await.map_err(Error::Io)?;
        match stream.try_io(Interest::READABLE, || recv_msg_with_fds(stream.as_raw_fd())) {
            Ok(res) => return Ok(res),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(Error::Io(err)),
        }
    }
}

async fn send_json_response_async<T: serde::Serialize>(
    stream: &UnixStream,
    resp: &T,
) -> Result<(), Error> {
    let encoded = serde_json::to_vec(resp).map_err(Error::Json)?;
    let mut written = 0usize;
    while written < encoded.len() {
        stream.ready(Interest::WRITABLE).await.map_err(Error::Io)?;
        match stream.try_write(&encoded[written..]) {
            Ok(n) => written += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => return Err(Error::Io(err)),
        }
    }
    Ok(())
}

/// Synchronous client helper that sends a `VmRegistrationRequest` and its `SCM_RIGHTS` file
/// descriptors (`shm_fd`, `kick_irqfds`, `wake_eventfds`) to `pylond` and reads the response.
pub fn send_registration(
    socket_path: &Path,
    req: &VmRegistrationRequest,
    shm_fd: RawFd,
    kick_irqfds: &[RawFd],
    wake_eventfds: &[RawFd],
) -> Result<VmRegistrationResponse, Error> {
    let mut stream = StdUnixStream::connect(socket_path).map_err(Error::Io)?;
    let payload = serde_json::to_vec(req).map_err(Error::Json)?;

    let mut all_fds = Vec::with_capacity(1 + kick_irqfds.len() + wake_eventfds.len());
    all_fds.push(shm_fd);
    all_fds.extend_from_slice(kick_irqfds);
    all_fds.extend_from_slice(wake_eventfds);

    send_msg_with_fds(stream.as_raw_fd(), &payload, &all_fds).map_err(Error::Io)?;
    stream.flush().map_err(Error::Io)?;

    let mut resp_buf = Vec::new();
    stream.read_to_end(&mut resp_buf).map_err(Error::Io)?;
    let resp: VmRegistrationResponse = serde_json::from_slice(&resp_buf).map_err(Error::Json)?;
    Ok(resp)
}

/// Synchronous client helper that sends a `VmPolicyUpdateRequest` over `/run/pylon/pylond.sock`
/// and reads the `VmPolicyUpdateResponse`.
pub fn send_policy_update(
    socket_path: &Path,
    req: &VmPolicyUpdateRequest,
) -> Result<VmPolicyUpdateResponse, Error> {
    let mut stream = StdUnixStream::connect(socket_path).map_err(Error::Io)?;
    let payload = serde_json::to_vec(req).map_err(Error::Json)?;

    send_msg_with_fds(stream.as_raw_fd(), &payload, &[]).map_err(Error::Io)?;
    stream.flush().map_err(Error::Io)?;
    let _ = stream.shutdown(Shutdown::Write);

    let mut resp_buf = Vec::new();
    stream.read_to_end(&mut resp_buf).map_err(Error::Io)?;
    let resp: VmPolicyUpdateResponse = serde_json::from_slice(&resp_buf).map_err(Error::Json)?;
    Ok(resp)
}

/// Synchronous client helper that queries `pylond` over `/run/pylon/pylond.sock`
/// for real-time VM statistics and core residency metering.
pub fn query_vm_stats(socket_path: &Path, vm_id: Option<u64>) -> Result<VmStatsResponse, Error> {
    let mut stream = StdUnixStream::connect(socket_path).map_err(Error::Io)?;
    let req = VmStatsRequest {
        action: Some("stats".to_string()),
        vm_id,
    };
    let payload = serde_json::to_vec(&req).map_err(Error::Json)?;

    send_msg_with_fds(stream.as_raw_fd(), &payload, &[]).map_err(Error::Io)?;
    stream.flush().map_err(Error::Io)?;
    let _ = stream.shutdown(Shutdown::Write);

    let mut resp_buf = Vec::new();
    stream.read_to_end(&mut resp_buf).map_err(Error::Io)?;
    let resp: VmStatsResponse = serde_json::from_slice(&resp_buf).map_err(Error::Json)?;
    Ok(resp)
}

/// Sends a datagram/stream message with `SCM_RIGHTS` file descriptors over a Unix socket.
pub fn send_msg_with_fds(sock_fd: RawFd, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_REGISTRATION_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many file descriptors for SCM_RIGHTS",
        ));
    }

    let mut iov = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast::<libc::c_void>(),
        iov_len: payload.len(),
    };

    let fd_bytes = size_of_val(fds);
    // SAFETY: `CMSG_SPACE` is a pure arithmetic macro computing the required buffer size.
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: `msghdr` is plain-old-data and safe to zero-initialize.
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;

    if !fds.is_empty() {
        msg.msg_control = cmsg_buf.as_mut_ptr().cast::<libc::c_void>();
        msg.msg_controllen = cmsg_space;

        // SAFETY: `msg` has a properly sized `msg_control` buffer for `fd_bytes` of ancillary data.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as usize;
            let data_ptr = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            copy_nonoverlapping(fds.as_ptr(), data_ptr, fds.len());
        }
    }

    // SAFETY: `sock_fd` is a valid socket fd and `msg` points to valid stack/heap buffers.
    let sent = unsafe { libc::sendmsg(sock_fd, &raw const msg, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receives a message payload and any attached `SCM_RIGHTS` file descriptors from a Unix socket.
pub fn recv_msg_with_fds(sock_fd: RawFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut payload_buf = vec![0u8; MAX_PAYLOAD_BYTES];
    let mut iov = libc::iovec {
        iov_base: payload_buf.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: payload_buf.len(),
    };

    let max_fd_bytes = MAX_REGISTRATION_FDS * size_of::<RawFd>();
    // SAFETY: `CMSG_SPACE` is a pure arithmetic macro computing the required buffer size.
    let cmsg_space = unsafe { libc::CMSG_SPACE(max_fd_bytes as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: `msghdr` is plain-old-data and safe to zero-initialize.
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_name = null_mut();
    msg.msg_namelen = 0;
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = cmsg_space;

    // SAFETY: `sock_fd` is a valid socket fd and `msg` points to valid mutable buffers.
    let n = unsafe { libc::recvmsg(sock_fd, &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed connection before sending registration",
        ));
    }

    payload_buf.truncate(n as usize);
    let mut received_fds = Vec::new();

    // SAFETY: Iterating over ancillary control headers populated by `recvmsg` within `cmsg_buf`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                let base_len = libc::CMSG_LEN(0) as usize;
                let data_bytes = (*cmsg).cmsg_len.saturating_sub(base_len);
                let fd_count = data_bytes / size_of::<RawFd>();
                for idx in 0..fd_count {
                    let raw_fd = data_ptr.add(idx).read_unaligned();
                    received_fds.push(OwnedFd::from_raw_fd(raw_fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
    }

    Ok((payload_buf, received_fds))
}

/// Runs a lightweight asynchronous Prometheus HTTP metrics exporter on `bind_addr`.
pub async fn run_prometheus_server(
    bind_addr: String,
    vm_snapshot: Arc<VmSnapshot>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let listener = match TcpListener::bind(&bind_addr).await {
        Ok(l) => {
            info!("Prometheus metrics server listening on http://{bind_addr}/metrics");
            l
        }
        Err(err) => {
            warn!("Failed to bind Prometheus metrics server on {bind_addr}: {err}");
            return;
        }
    };

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accept_res = listener.accept() => {
                let (mut stream, _) = match accept_res {
                    Ok(conn) => conn,
                    Err(err) => {
                        warn!("Metrics listener accept error: {err}");
                        continue;
                    }
                };
                let snapshot = Arc::clone(&vm_snapshot);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req_str = String::from_utf8_lossy(&buf[..n]);
                    let path = req_str
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");

                    if path == "/metrics" || path == "/" {
                        let mut body = String::new();
                        body.push_str("# HELP pylon_vm_allocated_cores Current physical cores assigned to VM\n");
                        body.push_str("# TYPE pylon_vm_allocated_cores gauge\n");
                        body.push_str("# HELP pylon_vm_core_microseconds_total Total physical core microseconds allocated to VM\n");
                        body.push_str("# TYPE pylon_vm_core_microseconds_total counter\n");
                        body.push_str("# HELP pylon_vm_cooperative_yields_total Total voluntary cooperative core yields\n");
                        body.push_str("# TYPE pylon_vm_cooperative_yields_total counter\n");
                        body.push_str("# HELP pylon_vm_forced_preemptions_total Total forced preemptions when grace period expired\n");
                        body.push_str("# TYPE pylon_vm_forced_preemptions_total counter\n");
                        body.push_str("# HELP pylon_vm_runnable_threads Current guest runnable threads\n");
                        body.push_str("# TYPE pylon_vm_runnable_threads gauge\n");
                        body.push_str("# HELP pylon_vm_min_cores Minimum configured physical cores\n");
                        body.push_str("# TYPE pylon_vm_min_cores gauge\n");
                        body.push_str("# HELP pylon_vm_max_cores Maximum configured physical cores\n");
                        body.push_str("# TYPE pylon_vm_max_cores gauge\n");

                        let vms = snapshot.load();
                        for vm in vms.iter() {
                            let vid = vm.vm_id();
                            let page = vm.shared_page();
                            let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
                            let mut current_cores = 0u32;
                            for vcpu_slot in page.vcpus.iter().take(num_vcpus) {
                                if vcpu_slot.assigned_pcpu.load(Ordering::Acquire) != PCPU_UNASSIGNED
                                    && vcpu_slot.state.load(Ordering::Acquire) != VCPU_STATE_PARKED
                                {
                                    current_cores += 1;
                                }
                            }
                            let runnable = page.telemetry.runnable_threads.load(Ordering::Acquire);
                            body.push_str(&format!("pylon_vm_allocated_cores{{vm_id=\"{vid}\"}} {current_cores}\n"));
                            body.push_str(&format!("pylon_vm_core_microseconds_total{{vm_id=\"{vid}\"}} {}\n", vm.allocated_core_usec()));
                            body.push_str(&format!("pylon_vm_cooperative_yields_total{{vm_id=\"{vid}\"}} {}\n", vm.cooperative_yield_count()));
                            body.push_str(&format!("pylon_vm_forced_preemptions_total{{vm_id=\"{vid}\"}} {}\n", vm.forced_preempt_count()));
                            body.push_str(&format!("pylon_vm_runnable_threads{{vm_id=\"{vid}\"}} {runnable}\n"));
                            body.push_str(&format!("pylon_vm_min_cores{{vm_id=\"{vid}\"}} {}\n", vm.min_cores()));
                            body.push_str(&format!("pylon_vm_max_cores{{vm_id=\"{vid}\"}} {}\n", vm.max_cores()));
                        }

                        body.push_str("# HELP pylon_vm_handoffs_without_park_total Revocations handed over although the donor vCPU never parked\n");
                        body.push_str("# TYPE pylon_vm_handoffs_without_park_total counter\n");
                        for vm in vms.iter() {
                            body.push_str(&format!(
                                "pylon_vm_handoffs_without_park_total{{vm_id=\"{}\"}} {}\n",
                                vm.vm_id(),
                                vm.handoff_without_park_count()
                            ));
                        }
                        body.push_str("# HELP pylon_vm_park_latency_microseconds Time from a revocation request to the donor vCPU parking\n");
                        body.push_str("# TYPE pylon_vm_park_latency_microseconds histogram\n");
                        for vm in vms.iter() {
                            let vid = vm.vm_id();
                            let latency = vm.park_latency();
                            let cumulative = latency.cumulative_buckets();
                            for (bound, count) in PARK_LATENCY_BUCKETS_US.iter().zip(cumulative.iter()) {
                                body.push_str(&format!(
                                    "pylon_vm_park_latency_microseconds_bucket{{vm_id=\"{vid}\",le=\"{bound}\"}} {count}\n"
                                ));
                            }
                            body.push_str(&format!(
                                "pylon_vm_park_latency_microseconds_bucket{{vm_id=\"{vid}\",le=\"+Inf\"}} {}\n",
                                cumulative[PARK_LATENCY_BUCKETS_US.len()]
                            ));
                            body.push_str(&format!(
                                "pylon_vm_park_latency_microseconds_sum{{vm_id=\"{vid}\"}} {}\n",
                                latency.sum_us()
                            ));
                            body.push_str(&format!(
                                "pylon_vm_park_latency_microseconds_count{{vm_id=\"{vid}\"}} {}\n",
                                latency.count()
                            ));
                        }

                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else {
                        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes()).await;
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A `VcpuDomainTable` that records its contents and can refuse inserts.
    #[derive(Default)]
    struct FakeTable {
        entries: Mutex<HashMap<u32, u32>>,
        refuse: Mutex<bool>,
    }

    impl VcpuDomainTable for FakeTable {
        fn insert(&self, tid: u32, pid: u32) -> scx_pylon::Result<()> {
            if *self.refuse.lock().unwrap() {
                return Err(scx_pylon::Error::Io(io::Error::other("map full")));
            }
            self.entries.lock().unwrap().insert(tid, pid);
            Ok(())
        }

        fn remove(&self, tid: u32) -> scx_pylon::Result<()> {
            self.entries.lock().unwrap().remove(&tid);
            Ok(())
        }
    }

    #[tokio::test]
    async fn registration_ignores_a_vmm_that_closed_the_socket() {
        let (vmm, daemon) = StdUnixStream::pair().unwrap();
        let req = VmRegistrationRequest {
            vm_id: 1,
            num_vcpus: 1,
            min_cores: 0,
            max_cores: 1,
            priority_weight: 1,
            kick_signal: None,
        };
        // One FD instead of three: rejected, but the reply is still sent.
        let payload = serde_json::to_vec(&req).unwrap();
        let null = fs::File::open("/dev/null").unwrap();
        send_msg_with_fds(vmm.as_raw_fd(), &payload, &[null.as_raw_fd()]).unwrap();
        drop(vmm);

        daemon.set_nonblocking(true).unwrap();
        let daemon = UnixStream::from_std(daemon).unwrap();
        let snapshot = Arc::new(VmSnapshot::from_pointee(Vec::new()));
        handle_client_connection(daemon, snapshot).await.unwrap();
    }

    #[test]
    fn sync_vcpu_domains_follows_the_registered_threads() {
        let table = FakeTable::default();
        let mut listed = HashMap::new();

        sync_vcpu_domains(
            &table,
            &mut listed,
            HashMap::from([(10, 1), (11, 1), (20, 2)]),
        );
        assert_eq!(
            *table.entries.lock().unwrap(),
            HashMap::from([(10, 1), (11, 1), (20, 2)])
        );

        // VM 2 is gone, VM 1 lost a thread, and thread 11's ID now belongs to VM 3.
        sync_vcpu_domains(&table, &mut listed, HashMap::from([(10, 1), (11, 3)]));
        assert_eq!(
            *table.entries.lock().unwrap(),
            HashMap::from([(10, 1), (11, 3)])
        );
        assert_eq!(listed, *table.entries.lock().unwrap());
    }

    #[test]
    fn sync_vcpu_domains_retries_a_failed_insert() {
        let table = FakeTable::default();
        let mut listed = HashMap::new();

        *table.refuse.lock().unwrap() = true;
        sync_vcpu_domains(&table, &mut listed, HashMap::from([(10, 1)]));
        assert!(listed.is_empty());

        *table.refuse.lock().unwrap() = false;
        sync_vcpu_domains(&table, &mut listed, HashMap::from([(10, 1)]));
        assert_eq!(*table.entries.lock().unwrap(), HashMap::from([(10, 1)]));
    }
}
