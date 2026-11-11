# Shared-memory ABI

Every Pylon VM has one 8,192-byte shared page. The VMM, `pylond` and
the guest all map it, and it's the only channel between the guest and
the host on the hot path. The file that backs it also holds an
8,192-byte [host page](#the-host-page) after it, which only the VMM and
`pylond` map. Its layout is defined once, in
[`pylon/pylon-abi/src/lib.rs`](https://github.com/CMKL-APEX/pylon/blob/main/pylon/pylon-abi/src/lib.rs)
(`#![no_std]`, so a guest kernel can use it too). The design is in
[`spec/01-shared-memory-abi.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/01-shared-memory-abi.md).

## Rules

- **One writer per cache line, where possible.** Each structure is
  `#[repr(C, align(64))]`, one 64-byte cache line, so that the guest and
  the host don't keep stealing the same line from each other. Each vCPU
  slot is split in half: the lower 32 bytes for the guest side, the upper
  32 for the host side.
- **Atomics only.** Every byte of the shared page, header and padding
  included, is an `AtomicU8`, `AtomicU32` or `AtomicU64`: the guest can
  write any of them at any time, so the host may hold a Rust reference
  to the page only if nothing in it is a plain integer. No reader takes
  a lock.
- **Fixed size.** The page is exactly two 4 KiB pages. Compile-time
  assertions check every structure's size and alignment, where the
  header, telemetry and slots start, and the offsets of
  `park_timer_deadline_tsc`, `revoke_requested`, `park_on_boot`,
  `host_tid` and `guest_tsc_offset`.

## Layout

| Offset | Size | Structure | Contents |
|---|---|---|---|
| `0x0000` | 64 | `PylonHeader` | Written once by the VMM. |
| `0x0040` | 64 | `PylonTelemetry` | Guest-written demand. |
| `0x0080` | 64 × 64 | `[PylonVcpuSlot; 64]` | One slot per vCPU, up to `MAX_VCPUS` = 64. |
| `0x1080` | 3,968 | `_tail_pad` | Zero padding to 8,192 bytes. |

### `PylonHeader`

| Offset | Field | Type | Value |
|---|---|---|---|
| 0 | `magic` | `u64` | `PYLON_MAGIC` = `0x5059_4C4F_4E56_4D31` (`"PYLONVM1"`) |
| 8 | `version` | `u32` | `PYLON_ABI_VERSION` = 3 (same layout as 1 and 2) |
| 12 | `num_vcpus` | `u32` | Number of slots in use: `max(boot, max)` vCPUs |
| 16 | `vm_id` | `u64` | From `--pylon vm_id=` |
| 24 | `tsc_khz` | `u64` | Counter rate used to convert ticks to time, or `0` if unknown. The VMM writes `0`. `pylond` doesn't read it: it always uses the host counter rate it finds at start. |
| 32 | `_reserved` | 32 bytes | Zero |

`pylond` rejects a registration whose header fails
`PylonHeader::is_valid` (wrong magic or version, or `num_vcpus` not in
1–64).

### `PylonTelemetry`

| Offset | Field | Type | Meaning | Written by today |
|---|---|---|---|---|
| 0 | `runnable_threads` | `u32` | Runnable threads in the guest, running or queued. | `pylon-guest-agent` |
| 4 | `rx_ring_cons_idx` | `u32` | Guest network receive ring's consumer index, compared with the host's doorbell to wake a zero-core VM. | Nothing |
| 8 | `stranded_work_bitmap` | `u64` | Bit *n* set: parked vCPU *n* has queued work. `pylond` wakes those first. | `pv-pylon`, when it parks a CPU with other runnable tasks |
| 16 | `head_wait_tsc` | `u64` | Guest TSC since which runnable threads have waited for a vCPU; `0` if none. | `pylon-guest-agent` |
| 24 | `next_timer_tsc` | `u64` | Guest TSC of the guest's next timer, to wake a zero-core VM. | Nothing (the VMM's `park_timer_deadline_tsc` covers armed timers) |
| 32 | `guest_heartbeat` | `u64` | Incremented on every guest update. | `pylon-guest-agent` |
| 40 | `_pad` | 16 bytes | | |

### `PylonVcpuSlot`

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `state` | `u8` | `PARKED` = 0, `RUNNING` = 1, `PARKING` = 2, `WAKING` = 3 (reserved: nothing writes it any more) |
| 1 | `in_critical_section` | `u8` | Non-zero while the vCPU holds a spinlock, handles an interrupt or has preemption disabled |
| 8 | `last_yield_tsc` | `u64` | Host TSC of the vCPU's last yield |
| 16 | `yield_count` | `u64` | Number of yields |
| 24 | `park_timer_deadline_tsc` | `u64` | Host TSC at which the guest's TSC-deadline timer was due when the VMM parked the vCPU; `0` if none. Cleared when the vCPU's park ends (inside KVM: when it's woken for a core, not when an interrupt lets it run). |
| **32** | `revoke_requested` | `u8` | `1`: `pylond` wants this vCPU to give up its core |
| 33 | `assigned_pcpu` | `u8` | Host CPU assigned by `pylond`; `255` (`PCPU_UNASSIGNED`) if none |
| 34 | `park_on_boot` | `u8` | `1` while the VMM wants the vCPU parked once the guest has booted: set when the vCPU thread starts, for vCPUs beyond `initial_active_vcpus`; cleared by the VMM when it parks the vCPU. Reserved (`0`) before; no version bump. |
| 36 | `host_tid` | `u32` | The vCPU thread's Linux thread ID, written by the VMM |
| 40 | `revoke_deadline_tsc` | `u64` | TSC after which `pylond` stops waiting for a yield |
| 48 | `forced_preempt_count` | `u64` | Revocations whose deadline passed |
| 56 | `guest_tsc_offset` | `u64` | Guest TSC minus host TSC (wrapping), measured by the VMM before the vCPU first runs; `0` if unknown |

Two limits follow from the types: at most 64 vCPUs per VM, and
`assigned_pcpu` can only name host CPUs 0–254.

`PARKED` means the VMM has parked the vCPU. With park inside KVM
(`--pylon park=kvm`, spec 08), the thread sleeps in `KVM_RUN` instead
of in the VMM, and an interrupt can run a `PARKED` vCPU without the VMM
seeing it. `state` stays `PARKED` until `pylond` gives the vCPU a core
([VMM integration](vmm-integration.md#parking-inside-kvm)).

## Who writes what, today

ABI version 2 gave each field one writer (spec 06 §3), and version 3
keeps that. The field docs in `pylon-abi` say which. These are the
rules for well-behaved guests: the guest can write the whole page, so
the VMM and `pylond` act only on the guest's `PARKING` request and
telemetry from it. The park state and thread IDs they act on are in the
[host page](#the-host-page), and the fields of the same name here are
copies for the guest.

| Field | Writer |
|---|---|
| `state` | The guest (`pv-pylon`), only `PARKING` for its own vCPU, over `RUNNING`, or over `PARKED` while parked inside KVM; the VMM, every other transition. Never `pylond`. |
| `revoke_requested` | `pylond` sets it; the VMM clears it when it parks the vCPU. |
| `yield_count`, `last_yield_tsc`, `forced_preempt_count` | The VMM, when it parks the vCPU. |
| `assigned_pcpu`, `revoke_deadline_tsc` | `pylond`. The VMM initializes `assigned_pcpu` to `PCPU_UNASSIGNED`. |
| `park_on_boot` | The VMM sets and clears it. `pylond` reads it: it asks the VMM to park a tagged vCPU (it sets `revoke_requested`) once `guest_heartbeat` is non-zero. |
| `host_tid`, `guest_tsc_offset`, `park_timer_deadline_tsc` | The VMM. |
| Telemetry | The guest: `pylon-guest-agent` writes `runnable_threads`, `head_wait_tsc` and `guest_heartbeat`, and nothing else; `pv-pylon` writes `stranded_work_bitmap`. |

Version 2 has the same layout as version 1. The version changed because
version-1 guest components broke these rules; a version-1 agent refuses
a version-2 page, and the other way round. `park_timer_deadline_tsc` and
`guest_tsc_offset` were added later in version 2, in bytes that were
reserved and zero, and readers treat `0` as "not provided", so they
didn't need a new version. `park_on_boot` took a reserved byte the same
way.

Version 3 (spec 08 §4.4) keeps the layout too, and changes what the
fields mean for a VM that parks inside KVM:

- **A `PARKED` vCPU may be running.** An interrupt runs it without a
  core and without an exit, so the VMM can't write `RUNNING`. Whether it
  runs comes from `scx_pylon`'s woken map.
- **The guest may write `PARKING` over `PARKED`**, when its park thread
  saw a revocation that the VMM then forced. The VMM turns it back into
  `PARKED`.
- **`park_timer_deadline_tsc` only orders vCPUs.** KVM fires the
  guest's timer itself, so `pylond` uses it only to pick which vCPU of a
  VM gets a core first.

The wake `eventfd`s stay: a vCPU parked inside KVM reads its own,
without blocking, after each exit, to learn that `pylond` assigned it a
core. A version-2 guest component or `pylond` refuses a version-3
page.

## The host page

`PylonHostPage` follows the shared page in the same file, at
`PYLON_HOST_PAGE_OFFSET` (8,192), so the file is `PYLON_SHM_FILE_SIZE`
(16,384) bytes. The VMM maps only the shared page into the guest, so
the guest can neither read nor write the host page. `pylond` maps the
whole file it receives at registration and refuses one that is smaller
or whose host header is invalid.

| Offset | Size | Structure | Contents |
|---|---|---|---|
| `0x0000` | 64 | `PylonHostHeader` | `magic` = `PYLON_HOST_MAGIC` (`"PYLONHP1"`), `version` = `PYLON_HOST_VERSION` = 3. Written once by the VMM. |
| `0x0040` | 64 × 64 | `[PylonHostVcpuSlot; 64]` | One slot per vCPU. |
| `0x1040` | 4,032 | `_tail_pad` | Zero padding to 8,192 bytes. |

`PylonHostVcpuSlot` holds the host's copy of each field that decides
who gets a core, and of the vCPU's thread ID:

| Field | Type | Writer |
|---|---|---|
| `state` | `u8` | The VMM: `PARKED` or `RUNNING`, never `PARKING`. |
| `revoke_requested` | `u8` | `pylond` sets it; the VMM clears it when it parks the vCPU. |
| `assigned_pcpu` | `u8` | `pylond`. The VMM initializes it to `PCPU_UNASSIGNED`. |
| `park_on_boot` | `u8` | The VMM. |
| `host_tid` | `u32` | The VMM, when the vCPU thread starts. `pylond` signals this thread and makes it the owner of the CPUs it gives the vCPU in `scx_pylon`. |
| `yield_count` | `u64` | The VMM, before it publishes `PARKED`. |
| `revoke_deadline_tsc` | `u64` | `pylond`, before it sets `revoke_requested`. |
| `park_timer_deadline_tsc` | `u64` | The VMM, when it parks the vCPU; 0 when it runs again. |

Each writer updates the host slot first and then the guest's copy in
`PylonVcpuSlot`, so a guest still learns of a revocation from
`revoke_requested` and can watch its own state. Whether a vCPU parked,
and whether a revocation is still pending, are read only from the host
slot: a guest that writes `PARKED`, a new `yield_count`, or clears
`revoke_requested` in its own slot changes nothing. Neither does a
guest that writes another thread's ID into its `host_tid`.

The host page has its own version, which changes without
`PYLON_ABI_VERSION`, since guests never see it. The VMM and `pylond`
have to be upgraded together. Version 2 added `host_tid`, version 3
`park_timer_deadline_tsc`.

## Time bases

Every `*_tsc` field and `tsc_khz` count host counter ticks: the TSC on
x86_64 and the virtual counter `CNTVCT_EL0` on aarch64. The names and
the layout don't depend on the architecture.

The guest writes `head_wait_tsc` and `next_timer_tsc` with its own
counter. Under KVM that differs from the host's by the vCPU's offset (a
new vCPU's counter starts near 0). The VMM measures the offset when each
vCPU thread starts, by reading the guest's counter (`IA32_TSC` on
x86_64, `KVM_REG_ARM_TIMER_CNT` on aarch64) between two host counter
reads, and stores it in `guest_tsc_offset`.
`PylonSharedPage::guest_to_host_tsc` subtracts vCPU 0's offset, since KVM
keeps a VM's vCPU TSCs in sync. The conversion assumes the guest TSC
runs at the host's rate (no TSC scaling). Every other TSC field is host
TSC.

## Memory ordering

Writers that publish something another party acts on use `Release`, and
the readers that act on it use `Acquire`. For example, `pylond` stores
`revoke_deadline_tsc` and then `revoke_requested = 1` with `Release`, so
a reader that sees the flag with `Acquire` also sees the deadline.
Counters and timestamps that are only sampled use `Relaxed`.

The guest's side of that contract depends on the guest's memory model.
On x86_64, ordinary loads and stores are already ordered strongly enough
for these pairs.

## The page in guest memory

The VMM maps the page as a shared memory slot at `PYLON_DEFAULT_GPA`
(`0xFEDF_0000`), or at `--pylon gpa=`. On x86_64 that address sits
between the IOAPIC and the local APIC, outside the PCI windows. Like a
device BAR, the page is not part of the VMM's guest RAM
(`GuestMemoryMmap`), so the guest's E820 table, the PVH memory map and
ACPI don't list it. `MemoryManager` keeps the mapping alive for the
VM's lifetime. A guest maps it through `/dev/mem`, which
`CONFIG_STRICT_DEVMEM` allows for addresses that aren't RAM, or through
`/dev/pylon` when the `pv-pylon` module is loaded. On x86_64 the
`/dev/mem` mapping then fails, so `pylon-guest-agent` uses `/dev/pylon`
when it exists.

## Changing the ABI

1. Change the spec first
   ([`spec/01-shared-memory-abi.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/01-shared-memory-abi.md)),
   and say who writes the new field.
2. Use reserved or padding bytes, and keep every structure 64 bytes. The
   compile-time assertions fail otherwise.
3. If the change alters a field's meaning or its writer, bump
   `PYLON_ABI_VERSION`. `pylond` refuses pages with another version, so
   update the VMM, `pylond` and guest components together.
4. A change to the host page needs `PYLON_HOST_VERSION` bumped, not
   `PYLON_ABI_VERSION`, and no guest component changes.
5. Update every reader and writer: `vmm/src/pylon.rs`,
   `pylond/src/engine.rs` and `lib.rs`, `pylon-guest-agent`, the C copy
   of the layout in `pylon/guest/linux/pv_pylon_abi.h`, and the
   tests that build pages by hand (`pylond/tests/e2e_pylon.rs`,
   `pylon_bench.rs`).
