// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

use std::fs::OpenOptions;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::result::Result as StdResult;
use std::sync::atomic::{AtomicU32, Ordering};

use libbpf_rs::{AsRawLibbpf, Link, MapCore, MapType, Object, ObjectBuilder, OpenObject};
use memmap2::{MmapMut, MmapOptions};
use thiserror::Error;

/// Maximum number of physical CPUs tracked in the `pylon_pcpu_owners` BPF map.
pub const MAX_PCPUS: usize = 256;

/// Name of the `BPF_F_MMAPABLE` array map inside `scx_pylon.bpf.o`.
pub const PCPU_OWNERS_MAP_NAME: &str = "pylon_pcpu_owners";

/// Name of the `sched_ext_ops` struct_ops map inside `scx_pylon.bpf.o`.
pub const SCX_OPS_MAP_NAME: &str = "pylon_ops";

/// Linux `BPF_F_MMAPABLE` map flag (`1U << 10`).
pub const BPF_F_MMAPABLE: u32 = 1 << 10;

/// Total byte size of the `pylon_pcpu_owners` array map (`256 * 8 = 2048` bytes).
pub const PCPU_OWNER_MAP_BYTES: usize = MAX_PCPUS * size_of::<PcpuOwnerEntry>();

/// Compiled BPF object bytes generated from `src/bpf/scx_pylon.bpf.c`.
pub const SCX_PYLON_BPF_OBJ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/scx_pylon.bpf.o"));

/// Per-pCPU ownership entry shared between `pylond` userspace and `scx_pylon` BPF.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcpuOwnerEntry {
    /// Target vCPU host kernel thread ID (`gettid()`), or `0` when unowned/idle.
    pub target_tid: u32,
    /// Ownership and preemption control flags (`PcpuOwnerFlags`).
    pub flags: u32,
}

const _: () = assert!(size_of::<PcpuOwnerEntry>() == 8);
const _: () = assert!(offset_of!(PcpuOwnerEntry, target_tid) == 0);
const _: () = assert!(offset_of!(PcpuOwnerEntry, flags) == 4);

/// Bitflags stored in [`PcpuOwnerEntry::flags`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcpuOwnerFlags(pub u32);

impl PcpuOwnerFlags {
    /// No special flags set.
    pub const NONE: Self = Self(0);
    /// Physical CPU slot is actively owned by a vCPU thread.
    pub const ACTIVE: Self = Self(1 << 0);
    /// Immediate preemption requested for a replaced donor vCPU.
    pub const PREEMPT: Self = Self(1 << 1);

    /// Returns the raw `u32` representation of the flags.
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Creates a `PcpuOwnerFlags` wrapper from raw bits.
    #[inline]
    pub const fn from_bits_retain(bits: u32) -> Self {
        Self(bits)
    }

    /// Returns `true` if all bits in `other` are set in `self`.
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Returns the bitwise OR of `self` and `other`.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Atomic view of a single [`PcpuOwnerEntry`] slot in memory-mapped space.
#[repr(C)]
struct AtomicPcpuOwnerSlot {
    target_tid: AtomicU32,
    flags: AtomicU32,
}

const _: () = assert!(size_of::<AtomicPcpuOwnerSlot>() == size_of::<PcpuOwnerEntry>());

/// Errors returned by `scx-pylon` operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Failed to parse, open, load, or attach the BPF object via `libbpf`.
    #[error("BPF operation failed")]
    Bpf(#[source] libbpf_rs::Error),

    /// Required BPF map was not found in the embedded object.
    #[error("Missing required BPF map '{map_name}'")]
    MissingMap { map_name: &'static str },

    /// BPF map metadata did not match the architectural contract.
    #[error("Invalid metadata on BPF map '{map_name}': {reason}")]
    InvalidMapMetadata {
        map_name: &'static str,
        reason: String,
    },

    /// Physical CPU index is out of bounds (`>= MAX_PCPUS`).
    #[error("Physical CPU index {pcpu} exceeds maximum ({max})")]
    PcpuOutOfBounds { pcpu: usize, max: usize },

    /// Failed to memory-map or initialize backing storage for the owner map.
    #[error("Memory-map I/O operation failed")]
    Io(#[source] io::Error),
}

/// Result alias for `scx-pylon`.
pub type Result<T> = StdResult<T, Error>;

/// Validated metadata for the `pylon_pcpu_owners` BPF map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcpuMapMetadata {
    /// Map name (`"pylon_pcpu_owners"`).
    pub name: String,
    /// BPF map type (`BPF_MAP_TYPE_ARRAY`).
    pub map_type: MapType,
    /// Key size in bytes (`4`).
    pub key_size: u32,
    /// Value size in bytes (`8`).
    pub value_size: u32,
    /// Maximum number of entries (`256`).
    pub max_entries: u32,
    /// Raw BPF map flags (must include `BPF_F_MMAPABLE`).
    pub map_flags: u32,
}

/// Zero-syscall memory-mapped handle to the 256-entry `pylon_pcpu_owners` array.
///
/// Can be backed by a live kernel `BPF_F_MMAPABLE` array map FD, a POSIX shared
/// memory file, or an anonymous page for unprivileged unit tests.
pub struct PcpuOwnerMap {
    mmap: MmapMut,
}

impl PcpuOwnerMap {
    /// Creates an anonymous memory-mapped `PcpuOwnerMap` for unprivileged
    /// userspace testing or simulation without requiring `CAP_BPF`.
    pub fn new_anonymous() -> Result<Self> {
        let mmap = MmapOptions::new()
            .len(PCPU_OWNER_MAP_BYTES)
            .map_anon()
            .map_err(Error::Io)?;
        Ok(Self { mmap })
    }

    /// Creates or opens a file-backed shared memory `PcpuOwnerMap` at `path`.
    pub fn from_shm(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.as_ref())
            .map_err(Error::Io)?;
        file.set_len(PCPU_OWNER_MAP_BYTES as u64)
            .map_err(Error::Io)?;
        // SAFETY: The backing file has been resized to `PCPU_OWNER_MAP_BYTES`
        // and all accesses go through `AtomicU32` operations in `AtomicPcpuOwnerSlot`.
        let mmap = unsafe {
            MmapOptions::new()
                .len(PCPU_OWNER_MAP_BYTES)
                .map_mut(&file)
                .map_err(Error::Io)?
        };
        Ok(Self { mmap })
    }

    /// Memory-maps a loaded kernel `pylon_pcpu_owners` `BPF_F_MMAPABLE` map FD
    /// (`PROT_READ | PROT_WRITE`, `MAP_SHARED`).
    pub fn from_map_fd(fd: BorrowedFd<'_>) -> Result<Self> {
        // SAFETY: `fd` refers to a validated `BPF_F_MMAPABLE` BPF array map of
        // at least `PCPU_OWNER_MAP_BYTES` bytes, and all concurrent accesses
        // use atomic loads/stores on `AtomicPcpuOwnerSlot`.
        let mmap = unsafe {
            MmapOptions::new()
                .len(PCPU_OWNER_MAP_BYTES)
                .map_mut(fd.as_raw_fd())
                .map_err(Error::Io)?
        };
        Ok(Self { mmap })
    }

    #[inline]
    fn slots(&self) -> &[AtomicPcpuOwnerSlot; MAX_PCPUS] {
        debug_assert!(self.mmap.len() >= PCPU_OWNER_MAP_BYTES);
        // SAFETY: `self.mmap` is page-aligned by `mmap(2)` and guaranteed to be
        // at least `PCPU_OWNER_MAP_BYTES` (`256 * 8 = 2048` bytes) long.
        // `AtomicPcpuOwnerSlot` has size 8 and alignment 4 with no invalid bit patterns.
        unsafe { &*(self.mmap.as_ptr() as *const [AtomicPcpuOwnerSlot; MAX_PCPUS]) }
    }

    /// Atomically assigns `pcpu` ownership to `target_tid` with `flags` using
    /// zero syscalls.
    #[inline]
    pub fn set_owner(&self, pcpu: usize, target_tid: u32, flags: PcpuOwnerFlags) -> Result<()> {
        let slot = self.slots().get(pcpu).ok_or(Error::PcpuOutOfBounds {
            pcpu,
            max: MAX_PCPUS - 1,
        })?;
        slot.flags.store(flags.bits(), Ordering::Release);
        slot.target_tid.store(target_tid, Ordering::Release);
        Ok(())
    }

    /// Atomically clears ownership on `pcpu` (`target_tid = 0`, `flags = 0`).
    #[inline]
    pub fn clear_owner(&self, pcpu: usize) -> Result<()> {
        self.set_owner(pcpu, 0, PcpuOwnerFlags::NONE)
    }

    /// Atomically reads the current [`PcpuOwnerEntry`] for `pcpu`.
    #[inline]
    pub fn get_owner(&self, pcpu: usize) -> Result<PcpuOwnerEntry> {
        let slot = self.slots().get(pcpu).ok_or(Error::PcpuOutOfBounds {
            pcpu,
            max: MAX_PCPUS - 1,
        })?;
        let target_tid = slot.target_tid.load(Ordering::Acquire);
        let flags = slot.flags.load(Ordering::Acquire);
        Ok(PcpuOwnerEntry { target_tid, flags })
    }

    /// Scans all 256 pCPU slots to find the first physical CPU currently owned
    /// by `tid` (returns `None` if `tid == 0` or not found).
    #[inline]
    pub fn find_pcpu_for_tid(&self, tid: u32) -> Option<usize> {
        if tid == 0 {
            return None;
        }
        let slots = self.slots();
        for (pcpu, slot) in slots.iter().enumerate() {
            if slot.target_tid.load(Ordering::Acquire) == tid {
                return Some(pcpu);
            }
        }
        None
    }
}

/// Userspace loader and handle for the `scx_pylon` `sched_ext` BPF dispatcher.
pub struct ScxPylonUserspace {
    _obj: Object,
    _link: Link,
    owner_map: PcpuOwnerMap,
}

impl ScxPylonUserspace {
    /// Opens the embedded `scx_pylon.bpf.o` ELF image in userspace memory and
    /// verifies the `pylon_pcpu_owners` map contract without requiring `CAP_BPF`.
    pub fn verify_embedded_object() -> Result<PcpuMapMetadata> {
        let open_obj = Self::open_embedded()?;
        Self::validate_open_object_map(&open_obj)
    }

    /// Opens the embedded `scx_pylon.bpf.o` ELF image via `libbpf-rs` and
    /// validates its map contract before kernel loading.
    pub fn open_embedded() -> Result<OpenObject> {
        let mut builder = ObjectBuilder::default();
        let open_obj = builder.open_memory(SCX_PYLON_BPF_OBJ).map_err(Error::Bpf)?;
        Self::validate_open_object_map(&open_obj)?;
        Ok(open_obj)
    }

    fn validate_open_object_map(open_obj: &OpenObject) -> Result<PcpuMapMetadata> {
        let map = open_obj
            .maps()
            .find(|m| m.name() == PCPU_OWNERS_MAP_NAME)
            .ok_or(Error::MissingMap {
                map_name: PCPU_OWNERS_MAP_NAME,
            })?;

        let map_type = map.map_type();
        let raw_map = map.as_libbpf_object().as_ptr();
        // SAFETY: `raw_map` is a valid non-null `bpf_map` pointer owned by `open_obj`.
        let key_size = unsafe { libbpf_sys::bpf_map__key_size(raw_map) };
        // SAFETY: `raw_map` is a valid non-null `bpf_map` pointer owned by `open_obj`.
        let value_size = unsafe { libbpf_sys::bpf_map__value_size(raw_map) };
        // SAFETY: `raw_map` is a valid non-null `bpf_map` pointer owned by `open_obj`.
        let max_entries = unsafe { libbpf_sys::bpf_map__max_entries(raw_map) };
        // SAFETY: `raw_map` is a valid non-null `bpf_map` pointer owned by `open_obj`.
        let map_flags = unsafe { libbpf_sys::bpf_map__map_flags(raw_map) };

        if map_type != MapType::Array {
            return Err(Error::InvalidMapMetadata {
                map_name: PCPU_OWNERS_MAP_NAME,
                reason: format!("expected MapType::Array, found {map_type:?}"),
            });
        }
        if max_entries != MAX_PCPUS as u32 {
            return Err(Error::InvalidMapMetadata {
                map_name: PCPU_OWNERS_MAP_NAME,
                reason: format!("expected max_entries == {MAX_PCPUS}, found {max_entries}"),
            });
        }
        if key_size != size_of::<u32>() as u32 {
            return Err(Error::InvalidMapMetadata {
                map_name: PCPU_OWNERS_MAP_NAME,
                reason: format!("expected key_size == 4, found {key_size}"),
            });
        }
        if value_size != size_of::<PcpuOwnerEntry>() as u32 {
            return Err(Error::InvalidMapMetadata {
                map_name: PCPU_OWNERS_MAP_NAME,
                reason: format!("expected value_size == 8, found {value_size}"),
            });
        }
        if (map_flags & BPF_F_MMAPABLE) == 0 {
            return Err(Error::InvalidMapMetadata {
                map_name: PCPU_OWNERS_MAP_NAME,
                reason: format!(
                    "BPF_F_MMAPABLE (0x{BPF_F_MMAPABLE:x}) not set in map_flags (0x{map_flags:x})"
                ),
            });
        }

        Ok(PcpuMapMetadata {
            name: map.name().to_string_lossy().into_owned(),
            map_type,
            key_size,
            value_size,
            max_entries,
            map_flags,
        })
    }

    /// Loads `scx_pylon.bpf.o` into the kernel, attaches the `sched_ext_ops`
    /// struct_ops dispatcher, and `mmap`s `pylon_pcpu_owners` into userspace.
    ///
    /// Requires `CAP_BPF` / `CAP_SYS_ADMIN` and a kernel with `CONFIG_SCHED_EXT=y`.
    pub fn load_and_attach() -> Result<Self> {
        let open_obj = Self::open_embedded()?;
        let mut obj = open_obj.load().map_err(Error::Bpf)?;

        let owner_map = {
            let bpf_map = obj
                .maps()
                .find(|m| m.name() == PCPU_OWNERS_MAP_NAME)
                .ok_or(Error::MissingMap {
                    map_name: PCPU_OWNERS_MAP_NAME,
                })?;
            PcpuOwnerMap::from_map_fd(bpf_map.as_fd())?
        };

        let mut ops_map = obj
            .maps_mut()
            .find(|m| m.name() == SCX_OPS_MAP_NAME)
            .ok_or(Error::MissingMap {
                map_name: SCX_OPS_MAP_NAME,
            })?;
        let link = ops_map.attach_struct_ops().map_err(Error::Bpf)?;

        log::info!("scx_pylon loaded and attached; mapped {MAX_PCPUS} pCPU owner slots");

        Ok(Self {
            _obj: obj,
            _link: link,
            owner_map,
        })
    }

    /// Returns a reference to the zero-syscall memory-mapped `pylon_pcpu_owners` map.
    #[inline]
    pub const fn owner_map(&self) -> &PcpuOwnerMap {
        &self.owner_map
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process};

    use super::*;

    #[test]
    fn test_pcpu_owner_entry_layout() {
        assert_eq!(size_of::<PcpuOwnerEntry>(), 8);
        assert_eq!(offset_of!(PcpuOwnerEntry, target_tid), 0);
        assert_eq!(offset_of!(PcpuOwnerEntry, flags), 4);
    }

    #[test]
    fn test_open_and_verify_embedded_bpf_object() {
        assert!(
            !SCX_PYLON_BPF_OBJ.is_empty(),
            "Embedded scx_pylon.bpf.o must not be empty"
        );
        let meta = ScxPylonUserspace::verify_embedded_object()
            .expect("scx_pylon.bpf.o must open and pass map metadata verification");
        assert_eq!(meta.name, PCPU_OWNERS_MAP_NAME);
        assert_eq!(meta.map_type, MapType::Array);
        assert_eq!(meta.max_entries, 256);
        assert_eq!(meta.key_size, 4);
        assert_eq!(meta.value_size, 8);
        assert_ne!(meta.map_flags & BPF_F_MMAPABLE, 0);

        let open_obj = ScxPylonUserspace::open_embedded().expect("open_embedded must succeed");
        let prog_names: Vec<String> = open_obj
            .progs()
            .map(|p| p.name().to_string_lossy().into_owned())
            .collect();
        assert!(
            prog_names.iter().any(|n| n == "pylon_select_cpu"),
            "Expected pylon_select_cpu program in {prog_names:?}"
        );
        assert!(
            prog_names.iter().any(|n| n == "pylon_dispatch"),
            "Expected pylon_dispatch program in {prog_names:?}"
        );
    }

    #[test]
    fn test_pcpu_owner_map_atomic_operations() {
        let map = PcpuOwnerMap::new_anonymous().expect("Failed to create anonymous PcpuOwnerMap");

        // All 256 slots initially empty
        for pcpu in 0..MAX_PCPUS {
            let entry = map.get_owner(pcpu).expect("get_owner must succeed");
            assert_eq!(entry, PcpuOwnerEntry::default());
        }
        assert_eq!(map.find_pcpu_for_tid(0), None);
        assert_eq!(map.find_pcpu_for_tid(1234), None);

        // Populate all 256 pCPU slots and verify lookup and atomic values
        for pcpu in 0..MAX_PCPUS {
            let tid = 10_000 + pcpu as u32;
            let flags = PcpuOwnerFlags::ACTIVE.union(PcpuOwnerFlags::PREEMPT);
            map.set_owner(pcpu, tid, flags)
                .expect("set_owner must succeed");
        }

        for pcpu in 0..MAX_PCPUS {
            let expected_tid = 10_000 + pcpu as u32;
            let entry = map.get_owner(pcpu).expect("get_owner must succeed");
            assert_eq!(entry.target_tid, expected_tid);
            assert_eq!(
                PcpuOwnerFlags::from_bits_retain(entry.flags),
                PcpuOwnerFlags::ACTIVE.union(PcpuOwnerFlags::PREEMPT)
            );
            assert!(PcpuOwnerFlags::from_bits_retain(entry.flags).contains(PcpuOwnerFlags::ACTIVE));
            assert_eq!(map.find_pcpu_for_tid(expected_tid), Some(pcpu));
        }

        // Clear a slot and verify it is no longer owned
        map.clear_owner(42).expect("clear_owner must succeed");
        assert_eq!(
            map.get_owner(42).expect("get_owner must succeed"),
            PcpuOwnerEntry::default()
        );
        assert_eq!(map.find_pcpu_for_tid(10_042), None);

        // Out-of-bounds pCPU index must return Error::PcpuOutOfBounds
        assert!(matches!(
            map.get_owner(MAX_PCPUS),
            Err(Error::PcpuOutOfBounds {
                pcpu: 256,
                max: 255
            })
        ));
        assert!(matches!(
            map.set_owner(MAX_PCPUS, 99, PcpuOwnerFlags::ACTIVE),
            Err(Error::PcpuOutOfBounds {
                pcpu: 256,
                max: 255
            })
        ));
    }

    #[test]
    fn test_pcpu_owner_map_from_shm() {
        let pid = process::id();
        let shm_path = env::temp_dir().join(format!("scx-pylon-test-{pid}.shm"));
        let _ = fs::remove_file(&shm_path);

        let writer = PcpuOwnerMap::from_shm(&shm_path).expect("from_shm writer must succeed");
        let reader = PcpuOwnerMap::from_shm(&shm_path).expect("from_shm reader must succeed");

        writer
            .set_owner(7, 4242, PcpuOwnerFlags::ACTIVE)
            .expect("writer set_owner must succeed");
        let entry = reader.get_owner(7).expect("reader get_owner must succeed");
        assert_eq!(entry.target_tid, 4242);
        assert_eq!(entry.flags, PcpuOwnerFlags::ACTIVE.bits());
        assert_eq!(reader.find_pcpu_for_tid(4242), Some(7));

        let _ = fs::remove_file(&shm_path);
    }
}
