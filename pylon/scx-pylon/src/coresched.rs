// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

//! Guard against kernel core scheduling while `scx_pylon` is attached.
//!
//! Since v6.19, sched_ext can release the rq lock in the middle of a
//! core-scheduling pick. Kernels without the upstream fixes (f2da9587118d,
//! d954004205c1 and the rest of that series) then corrupt the run-queue clock
//! and `rq->core_pick`, which ends in a NULL dereference or a hard hang
//! (CMKL-APEX/pylon#15). Core scheduling turns on host-wide as soon as any task
//! has a cookie, so on such a kernel `scx_pylon` must not run next to one.

use std::ffi::OsStr;
use std::fmt::{self, Display};
use std::{fs, io};

use clap::ValueEnum;
use libbpf_rs::Btf;
use libbpf_rs::btf::types::Struct;

const PR_SCHED_CORE: libc::c_int = 62;
const PR_SCHED_CORE_GET: libc::c_ulong = 0;
const PR_SCHED_CORE_SCOPE_THREAD: libc::c_ulong = 0;

/// Whether to guard against core scheduling while `scx_pylon` is attached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum GuardMode {
    /// Guard unless the kernel has the fixes or can't core-schedule.
    #[default]
    Auto,
    /// Guard whenever the kernel can core-schedule.
    On,
    /// Never guard.
    Off,
}

/// What the running kernel can do with core scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelCoreSched {
    /// Built without `CONFIG_SCHED_CORE`.
    Unsupported,
    /// No SMT siblings, so no task can get a cookie. SMT turned on at run
    /// time isn't noticed.
    NoSmt,
    /// Has `scx_rq.lock_drop_seq`, added by f2da9587118d, on which the fix
    /// d954004205c1 depends.
    Fixed,
    /// Can core-schedule and lacks the fixes.
    Affected,
}

impl Display for KernelCoreSched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "kernel has no core scheduling",
            Self::NoSmt => "host has no SMT siblings",
            Self::Fixed => "kernel has the sched_ext core-scheduling fixes",
            Self::Affected => "kernel lacks the sched_ext core-scheduling fixes (pylon#15)",
        })
    }
}

impl KernelCoreSched {
    /// Probes the running kernel.
    pub fn detect() -> Self {
        let mut cookie: u64 = 0;
        // SAFETY: PR_SCHED_CORE_GET on the calling thread writes one u64 to
        // `cookie`, which is valid for writes.
        let ret = unsafe {
            libc::prctl(
                PR_SCHED_CORE,
                PR_SCHED_CORE_GET,
                0 as libc::c_ulong,
                PR_SCHED_CORE_SCOPE_THREAD,
                &mut cookie as *mut u64 as libc::c_ulong,
            )
        };
        if ret == -1 {
            return match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENODEV) => Self::NoSmt,
                _ => Self::Unsupported,
            };
        }
        match Btf::from_vmlinux() {
            Ok(btf) if struct_has_member(&btf, "scx_rq", "lock_drop_seq") => Self::Fixed,
            _ => Self::Affected,
        }
    }

    /// Whether `scx_pylon` needs the guard under `mode`.
    pub fn needs_guard(self, mode: GuardMode) -> bool {
        match mode {
            GuardMode::Auto => self == Self::Affected,
            GuardMode::On => self != Self::Unsupported,
            GuardMode::Off => false,
        }
    }
}

/// Whether the BTF struct `name` has a member called `member`.
fn struct_has_member(btf: &Btf<'_>, name: &str, member: &str) -> bool {
    btf.type_by_name::<Struct<'_>>(name)
        .is_some_and(|s| s.iter().any(|m| m.name == Some(OsStr::new(member))))
}

/// A thread that has a core scheduling cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookiedThread {
    pub tid: i32,
    pub comm: String,
}

/// Returns the core scheduling cookie of thread `tid`.
fn thread_cookie(tid: i32) -> io::Result<u64> {
    let mut cookie: u64 = 0;
    // SAFETY: PR_SCHED_CORE_GET writes one u64 to `cookie`, which is valid
    // for writes.
    let ret = unsafe {
        libc::prctl(
            PR_SCHED_CORE,
            PR_SCHED_CORE_GET,
            tid as libc::c_ulong,
            PR_SCHED_CORE_SCOPE_THREAD,
            &mut cookie as *mut u64 as libc::c_ulong,
        )
    };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(cookie)
}

/// Finds a thread on the host with a core scheduling cookie. Reading another
/// process's cookie needs ptrace access to it, so run this as root.
pub fn find_cookied_thread() -> io::Result<Option<CookiedThread>> {
    let numeric = |entry: io::Result<fs::DirEntry>| {
        entry
            .ok()
            .and_then(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
    };
    for pid in fs::read_dir("/proc")?.filter_map(numeric) {
        // The process may exit while we look.
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for tid in tasks.filter_map(numeric) {
            match thread_cookie(tid) {
                Ok(0) => {}
                Ok(_) => {
                    let comm = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
                        .map(|c| c.trim_end().to_owned())
                        .unwrap_or_default();
                    return Ok(Some(CookiedThread { tid, comm }));
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn test_needs_guard() {
        use GuardMode::*;
        use KernelCoreSched::*;
        for kernel in [Unsupported, NoSmt, Fixed, Affected] {
            assert!(!kernel.needs_guard(Off));
            assert_eq!(kernel.needs_guard(Auto), kernel == Affected);
            assert_eq!(kernel.needs_guard(On), kernel != Unsupported);
        }
    }

    #[test]
    fn test_struct_has_member() {
        if !Path::new("/sys/kernel/btf/vmlinux").exists() {
            return;
        }
        let btf = Btf::from_vmlinux().unwrap();
        assert!(struct_has_member(&btf, "task_struct", "pid"));
        assert!(!struct_has_member(&btf, "task_struct", "no_such_member"));
        assert!(!struct_has_member(&btf, "no_such_struct", "pid"));
    }

    #[test]
    fn test_own_thread_has_no_cookie() {
        // Kernels without core scheduling refuse the query.
        if let Ok(cookie) = thread_cookie(0) {
            assert_eq!(cookie, 0);
        }
    }
}
