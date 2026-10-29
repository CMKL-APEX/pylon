# Shared-memory ABI

Every Pylon VM has one 8,192-byte shared page. The VMM, `pylond` and
the guest all map it, and it's the only channel between the guest and
the host on the hot path. Its layout is defined once, in
[`pylon/pylon-abi/src/lib.rs`](https://github.com/CMKL-APEX/pylon/blob/main/pylon/pylon-abi/src/lib.rs)
(`#![no_std]`, so a guest kernel can use it too). The design is in
[`spec/01-shared-memory-abi.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/01-shared-memory-abi.md).

## Rules

- **One writer per cache line, where possible.** Each structure is
  `#[repr(C, align(64))]`, one 64-byte cache line, so that the guest and
  the host don't keep stealing the same line from each other. Each vCPU
  slot is split in half: the lower 32 bytes for the guest side, the upper
  32 for the host side.
- **Atomics only.** After the VMM initializes the page, every field
  that changes is an `AtomicU8`, `AtomicU32` or `AtomicU64`. No reader
  takes a lock.
- **Fixed size.** The page is exactly two 4 KiB pages. Compile-time
  assertions check every structure's size and `revoke_requested`'s
  offset.

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
| 8 | `version` | `u32` | `PYLON_ABI_VERSION` = 2 (same layout as 1; stricter writer rules) |
| 12 | `num_vcpus` | `u32` | Number of slots in use: `max(boot, max)` vCPUs |
| 16 | `vm_id` | `u64` | From `--pylon vm_id=` |
| 24 | `tsc_khz` | `u64` | TSC rate used to convert cycles to time, or `0` if unknown. The VMM writes `0`; `pylond` then uses the host TSC rate it measures at start. |
| 32 | `_reserved` | 32 bytes | Zero |

`pylond` rejects a registration whose header fails
`PylonHeader::is_valid` (wrong magic or version, or `num_vcpus` not in
1–64).

### `PylonTelemetry`

| Offset | Field | Type | Meaning | Written by today |
|---|---|---|---|---|
| 0 | `runnable_threads` | `u32` | Runnable threads in the guest, running or queued. | `pylon-guest-agent` |
| 4 | `rx_ring_cons_idx` | `u32` | Guest network receive ring's consumer index, compared with the host's doorbell to wake a zero-core VM. | Nothing |
| 8 | `stranded_work_bitmap` | `u64` | Bit *n* set: parked vCPU *n* has queued work. `pylond` wakes those first. | Nothing |
| 16 | `head_wait_tsc` | `u64` | TSC when the oldest runnable thread started waiting; `0` if none. | `pylon-guest-agent` |
| 24 | `next_timer_tsc` | `u64` | TSC of the guest's next timer, to wake a zero-core VM. | Nothing |
| 32 | `guest_heartbeat` | `u64` | Incremented on every guest update. | `pylon-guest-agent` |
| 40 | `_pad` | 16 bytes | | |

### `PylonVcpuSlot`

| Offset | Field | Type | Meaning |
|---|---|---|---|
| 0 | `state` | `u8` | `PARKED` = 0, `RUNNING` = 1, `PARKING` = 2, `WAKING` = 3 |
| 1 | `in_critical_section` | `u8` | Non-zero while the vCPU holds a spinlock, handles an interrupt or has preemption disabled |
| 8 | `last_yield_tsc` | `u64` | TSC of the vCPU's last yield |
| 16 | `yield_count` | `u64` | Number of yields |
| 24 | `_reserved_guest` | 8 bytes | |
| **32** | `revoke_requested` | `u8` | `1`: `pylond` wants this vCPU to give up its core |
| 33 | `assigned_pcpu` | `u8` | Host CPU assigned by `pylond`; `255` (`PCPU_UNASSIGNED`) if none |
| 36 | `host_tid` | `u32` | The vCPU thread's Linux thread ID, written by the VMM |
| 40 | `revoke_deadline_tsc` | `u64` | TSC after which `pylond` stops waiting for a yield |
| 48 | `forced_preempt_count` | `u64` | Revocations whose deadline passed |
| 56 | `_reserved_host` | 8 bytes | |

Two limits follow from the types: at most 64 vCPUs per VM, and
`assigned_pcpu` can only name host CPUs 0–254.

## Who writes what, today

ABI version 2 gives each field one writer (spec 06 §3), and the field
docs in `pylon-abi` say which:

| Field | Writer |
|---|---|
| `state` | The guest, only `RUNNING → PARKING` for its own vCPU; the VMM, every other transition. Never `pylond`. |
| `revoke_requested` | `pylond` sets it; the VMM clears it when it parks the vCPU. |
| `yield_count`, `last_yield_tsc`, `forced_preempt_count` | The VMM, when it parks the vCPU. |
| `assigned_pcpu`, `revoke_deadline_tsc` | `pylond`. The VMM initializes `assigned_pcpu` to `PCPU_UNASSIGNED`. |
| `host_tid` | The VMM. |
| Telemetry | The guest (today `pylon-guest-agent`, which writes nothing else). |

Version 2 has the same layout as version 1. The version changed because
version-1 guest components broke these rules; a version-1 agent refuses
a version-2 page, and the other way round.

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
(`0xFEDF_0000`), or at `--pylon gpa=`. That address sits in the 32-bit
reserved range below the local APIC on x86_64. The page is also added
to the VMM's guest memory map, so it appears in the guest's E820 table
as an 8 KiB RAM range ([Status](../status.md#what-doesnt-work-yet)).

## Changing the ABI

1. Change the spec first
   ([`spec/01-shared-memory-abi.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/01-shared-memory-abi.md)),
   and say who writes the new field.
2. Use reserved or padding bytes, and keep every structure 64 bytes. The
   compile-time assertions fail otherwise.
3. If the change alters a field's meaning or its writer, bump
   `PYLON_ABI_VERSION`. `pylond` refuses pages with another version, so
   update the VMM, `pylond` and guest components together.
4. Update every reader and writer: `vmm/src/pylon.rs`,
   `pylond/src/engine.rs` and `lib.rs`, `pylon-guest-agent`, and the
   tests that build pages by hand (`pylond/tests/e2e_pylon.rs`,
   `pylon_bench.rs`).
