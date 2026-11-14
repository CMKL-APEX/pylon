// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! The `pylon-session` thread: registers the VM with `pylond`, finds out when `pylond` goes
//! away, runs the VM unmanaged until a `pylond` listens again, and registers it again
//! (`spec/10-pylond-restart.md`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io, thread};

use log::{info, warn};
use vmm_sys_util::eventfd::EventFd;

use crate::pylon::{Error, PylonVmmState};

/// How often an unmanaged VMM looks for the socket, in case the directory watch misses it.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often the session thread checks that the vCPUs it woke have left `PARKED`, and wakes
/// the rest again: a signal that lands just before a vCPU's `KVM_RUN` sleeps is lost.
const WAKE_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// How many times the VMM retries a registration that ended without a reply, such as one
/// `pylond` turned away at its connection limit, before it waits for a new socket.
const MAX_RETRIES: u32 = 5;

/// Starts the `pylon-session` thread for `state`, which ends once `state` is dropped.
pub(crate) fn spawn(
    state: Weak<PylonVmmState>,
    socket_path: PathBuf,
    stop: Arc<EventFd>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("pylon-session".to_owned())
        .spawn(move || run(&state, &socket_path, &stop))
        .map(drop)
}

/// The socket's identity: a new `pylond` binds a new socket. A new socket can get the inode
/// number of the one it replaces (on `tmpfs`, for one), so its change time, which the bind and
/// the rename into place set, tells the two apart.
fn socket_identity(path: &Path) -> Option<(u64, u64, i64, i64)> {
    fs::metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

/// How a registration attempt ended.
enum Attempt {
    /// `pylond` keeps a session.
    Session(UnixStream),
    /// `pylond` accepted the VM without a session (`pylond` 0.0.8): it can't tell the VMM when
    /// it goes away, so there is nothing more to do.
    NoSession,
    /// Refused, or nothing listens at the socket: wait for a new socket.
    Refused,
    /// No reply: try again soon.
    NoReply,
}

fn attempt(state: &PylonVmmState, socket_path: &Path) -> Attempt {
    let vm_id = state.config().vm_id;
    state.reset_for_registration();
    state.set_managed();
    let result = state.register_with_daemon(socket_path);
    let attempt = match result {
        Ok(Some(stream)) => {
            info!(
                "Pylon: registered VM {vm_id} with pylond, parking vCPUs {}",
                state.park_location()
            );
            return Attempt::Session(stream);
        }
        Ok(None) => {
            info!(
                "Pylon: registered VM {vm_id} with pylond, parking vCPUs {}; this pylond \
                 can't tell the VMM when it stops",
                state.park_location()
            );
            return Attempt::NoSession;
        }
        Err(e @ (Error::DaemonConnect(_) | Error::DaemonRejected(_))) => {
            warn!(
                "Pylon: failed to register VM {vm_id} with pylond at {socket_path:?}: {e}; it \
                 runs unmanaged until a pylond listens there again"
            );
            Attempt::Refused
        }
        Err(e) => {
            warn!("Pylon: failed to register VM {vm_id} with pylond at {socket_path:?}: {e}");
            Attempt::NoReply
        }
    };
    state.set_unmanaged();
    attempt
}

fn run(state: &Weak<PylonVmmState>, socket_path: &Path, stop: &EventFd) {
    let watch = DirWatch::new(socket_path)
        .inspect_err(|e| warn!("Pylon: can't watch for the pylond socket {socket_path:?}: {e}"))
        .ok();
    // The socket last tried, which isn't tried again unless a registration had no reply.
    let mut tried = None;
    let mut retries = 0;
    loop {
        let Some(vmm) = state.upgrade() else {
            return;
        };
        let identity = socket_identity(socket_path);
        let mut wait = SOCKET_POLL_INTERVAL;
        if identity.is_some() && (identity != tried || retries > 0) {
            if !vmm.all_vcpus_running() {
                // A vCPU the last `pylond` parked hasn't run yet: register only once it has
                // (`spec/10-pylond-restart.md` §6 V5).
                vmm.wake_parked_vcpus();
                drop(vmm);
                match wait_for(watch.as_ref(), stop, WAKE_RETRY_INTERVAL) {
                    Wait::Stop => return,
                    Wait::Created => tried = None,
                    Wait::Timeout => {}
                }
                continue;
            }
            tried = identity;
            match attempt(&vmm, socket_path) {
                Attempt::Session(stream) => {
                    retries = 0;
                    drop(vmm);
                    if !wait_for_end(&stream, stop) {
                        return;
                    }
                    let Some(vmm) = state.upgrade() else {
                        return;
                    };
                    if vmm.set_unmanaged() {
                        warn!(
                            "Pylon: pylond closed the session of VM {}; it runs unmanaged \
                             until a pylond listens at {socket_path:?} again",
                            vmm.config().vm_id
                        );
                    }
                    continue;
                }
                Attempt::NoSession => return,
                Attempt::Refused => retries = 0,
                Attempt::NoReply => {
                    retries += 1;
                    if retries > MAX_RETRIES {
                        retries = 0;
                    } else {
                        wait = retry_delay();
                    }
                }
            }
        }
        drop(vmm);
        match wait_for(watch.as_ref(), stop, wait) {
            Wait::Stop => return,
            // The socket's name was created or moved into place: a new `pylond`, whatever
            // its identity.
            Wait::Created => tried = None,
            Wait::Timeout => {}
        }
    }
}

/// Between 100 ms and 1 s, varying between VMMs, so that VMMs turned away together don't
/// come back together.
fn retry_delay() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    Duration::from_millis(100 + u64::from(nanos % 900))
}

/// Polls `fds` for input for up to `timeout` (`None`: no limit), and returns which are ready.
fn poll_in(fds: &[RawFd], timeout: Option<Duration>) -> Vec<bool> {
    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let timeout_ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    // SAFETY: `pollfds` is a valid array of `pollfds.len()` entries for the call.
    let ret = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    if ret <= 0 {
        return vec![false; fds.len()];
    }
    pollfds.iter().map(|p| p.revents != 0).collect()
}

/// How a wait for the socket ended.
enum Wait {
    /// The VMM state is gone (`stop`).
    Stop,
    /// A file of the socket's name was created or moved into its directory.
    Created,
    /// The timeout passed, or something else changed in the directory.
    Timeout,
}

/// Waits up to `timeout` or until something is created in the socket's directory.
fn wait_for(watch: Option<&DirWatch>, stop: &EventFd, timeout: Duration) -> Wait {
    let mut fds = vec![stop.as_raw_fd()];
    if let Some(watch) = watch {
        fds.push(watch.fd.as_raw_fd());
    }
    let ready = poll_in(&fds, Some(timeout));
    if ready[0] {
        return Wait::Stop;
    }
    if ready.get(1) == Some(&true)
        && let Some(watch) = watch
        && watch.drain()
    {
        return Wait::Created;
    }
    Wait::Timeout
}

/// Waits until `pylond` closes the session. Returns `false` once the VMM state is gone
/// (`stop`).
fn wait_for_end(stream: &UnixStream, stop: &EventFd) -> bool {
    let mut buf = [0u8; 64];
    loop {
        let ready = poll_in(&[stop.as_raw_fd(), stream.as_raw_fd()], None);
        if ready[0] {
            return false;
        }
        if !ready[1] {
            continue;
        }
        // `pylond` sends nothing over a session: anything it sends is read and dropped.
        match io::Read::read(&mut &*stream, &mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

/// An `inotify` watch on the socket's directory, for files created or moved into it: `pylond`
/// binds its socket under a temporary name and renames it into place.
struct DirWatch {
    fd: OwnedFd,
    /// The socket's file name, which the events are matched against.
    name: Vec<u8>,
}

impl DirWatch {
    fn new(socket_path: &Path) -> io::Result<Self> {
        let dir = match socket_path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let name = socket_path
            .file_name()
            .map(|name| name.as_bytes().to_vec())
            .unwrap_or_default();
        let dir = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: `inotify_init1` takes flags and returns a new file descriptor or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a new file descriptor owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: `fd` is an inotify instance and `dir` a NUL-terminated path.
        let wd = unsafe {
            libc::inotify_add_watch(
                fd.as_raw_fd(),
                dir.as_ptr(),
                libc::IN_CREATE | libc::IN_MOVED_TO,
            )
        };
        if wd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, name })
    }

    /// Reads the pending events, and returns whether one names the socket.
    fn drain(&self) -> bool {
        // Aligned for `inotify_event`, and large enough for any one event.
        #[repr(C, align(8))]
        struct Buf([u8; 4096]);
        let mut buf = Buf([0; 4096]);
        let mut named = false;
        loop {
            // SAFETY: `buf` is valid for its length; the fd is non-blocking.
            let len =
                unsafe { libc::read(self.fd.as_raw_fd(), buf.0.as_mut_ptr().cast(), buf.0.len()) };
            let Ok(len) = usize::try_from(len) else {
                return named;
            };
            if len == 0 {
                return named;
            }
            named |= events_name(&buf.0[..len], &self.name);
        }
    }
}

/// Whether one of the `inotify` events in `events`, as read from an inotify fd, names `name`.
fn events_name(events: &[u8], name: &[u8]) -> bool {
    const HEADER: usize = size_of::<libc::inotify_event>();
    let mut at = 0;
    while at + HEADER <= events.len() {
        // The header's `len` field: the length of the NUL-padded name that follows it.
        let len_at = at + HEADER - size_of::<u32>();
        let len = u32::from_ne_bytes(events[len_at..len_at + 4].try_into().unwrap()) as usize;
        let end = (at + HEADER + len).min(events.len());
        let event_name = &events[at + HEADER..end];
        let event_name = event_name.split(|&b| b == 0).next().unwrap_or_default();
        if !name.is_empty() && event_name == name {
            return true;
        }
        at += HEADER + len;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use tempfile::TempDir;

    use super::*;

    /// A watch names the socket only when the socket's own name is created or moved in.
    #[test]
    fn test_dir_watch_reports_the_socket_by_name() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("pylond.sock");
        let watch = DirWatch::new(&sock_path).unwrap();
        assert!(!watch.drain());

        fs::write(dir.path().join("other"), b"").unwrap();
        assert!(!watch.drain());

        let tmp_path = dir.path().join("pylond.sock.tmp");
        let _listener = UnixListener::bind(&tmp_path).unwrap();
        assert!(!watch.drain());
        fs::rename(&tmp_path, &sock_path).unwrap();
        assert!(watch.drain());
        assert!(!watch.drain());
    }

    /// A socket bound again at the same path doesn't look like the one it replaces, even if
    /// it gets the same inode number.
    #[test]
    fn test_a_new_socket_has_a_new_identity() {
        let dir = TempDir::new().unwrap();
        let sock_path = dir.path().join("pylond.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let old = socket_identity(&sock_path).unwrap();
        drop(listener);
        fs::remove_file(&sock_path).unwrap();
        thread::sleep(Duration::from_millis(20));
        let _listener = UnixListener::bind(&sock_path).unwrap();
        assert_ne!(socket_identity(&sock_path), Some(old));
    }
}
