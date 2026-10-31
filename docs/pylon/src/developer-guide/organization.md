# Repository and code organization

## Upstream isolation

`CMKL-APEX/pylon` tracks upstream Cloud Hypervisor, and upstream changes
are merged in regularly. To keep those merges cheap, Pylon follows the
rule in
[`spec/00-architecture.md` §2](https://github.com/CMKL-APEX/pylon/blob/main/spec/00-architecture.md#2-repository-directory-layout--upstream-isolation-rules):

- New crates live under `pylon/`, where upstream never writes.
- VMM-side logic lives in one new file, `vmm/src/pylon.rs`.
- Upstream files get only short hook calls into that file.

When you add VMM behavior, put it in `vmm/src/pylon.rs` (or a sibling
new file) and add the smallest possible call in the upstream file.

## Crates

| Crate | Path | Lines | Kind | What it is |
|---|---|---|---|---|
| `pylon-abi` | [`pylon/pylon-abi`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylon-abi) | ~290 | `#![no_std]` library | The shared page's types and constants, and compile-time layout assertions. |
| `pylond` | [`pylon/pylond`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylond) | ~3,300 | Library + binaries `pylond`, `pylon-bench` | The arbiter. |
| `scx-pylon` | [`pylon/scx-pylon`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/scx-pylon) | ~880 | Library + binary `scx_pylon` | The BPF scheduler, its build script and loader. |
| `pylon-guest-agent` | [`pylon/pylon-guest-agent`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylon-guest-agent) | ~620 | Library + binary | The userspace guest daemon. |

`pylon/guest/linux/` isn't a crate: it is the `pv-pylon` Linux kernel
module, in C, built with Kbuild (`make -C pylon/guest/linux`). See [the
guest module](#pylonguestlinux) below.

Line counts include tests and were taken on 2026-10-29.

## Files

### `pylon-abi`

| File | Contents |
|---|---|
| `src/lib.rs` | `PylonHeader`, `PylonTelemetry`, `PylonVcpuSlot`, `PylonSharedPage`, the `VCPU_STATE_*` and other constants, and `const` assertions on every size and key offset. |

### `pylond`

| File | Contents |
|---|---|
| `src/lib.rs` | Shared types: `VmHandle` (one registered VM: its mapped page, `eventfd`s, policy and counters), `VmSnapshot` (`ArcSwap<Vec<VmHandle>>`), the pCPU owner table, `CoreState`, the JSON request and response types, and CPU pinning. |
| `src/control.rs` | Plane 1: `ControlPlane`, the Unix socket server, `SCM_RIGHTS` file descriptor passing, registration, policy and stats handling, the Prometheus HTTP server, and client helpers (`send_registration` and the like). |
| `src/engine.rs` | Plane 2: `MatrixEngine`, the polling loop, the counter, rate and timed-wait helpers (TSC and `TPAUSE` on x86_64, `CNTVCT_EL0` and `WFET` on aarch64). |
| `src/main.rs` | The `pylond` binary: options, `scx_pylon` attach policy, thread start-up. |
| `src/bin/pylon_bench.rs` | The Pylon vs. ACPI hotplug model benchmark. |
| `tests/e2e_pylon.rs` | End-to-end tests: a real control plane and engine, with VMs simulated by threads. |

### `scx-pylon`

| File | Contents |
|---|---|
| `src/bpf/scx_pylon.bpf.c` | The `sched_ext` scheduler: the `pylon_pcpu_owners` and `pylon_smt_next` maps, the owner, vCPU and host queues, and SMT isolation. |
| `src/bpf/vmlinux.h` | Kernel type definitions for the BPF build (generated; excluded from linters). |
| `build.rs` | Compiles the BPF object with `clang -target bpf` into `OUT_DIR`. |
| `src/lib.rs` | `ScxPylonUserspace` (open, load, attach) and `PcpuOwnerMap`, the userspace view of the map. |
| `src/main.rs` | The standalone `scx_pylon` tool. |
| `tests/live_attach.rs` | Attaches to a real `sched_ext` kernel. Marked `#[ignore]` because it needs root: `sudo -E cargo test -p scx-pylon -- --ignored`. |

### `pylon-guest-agent`

| File | Contents |
|---|---|
| `src/lib.rs` | `PylonMemoryBackend` (`/dev/mem` or file), `TelemetryReader` and its `/proc` implementation, `PylonGuestAgent`. |
| `src/main.rs` | The binary. |

### `pylon/guest/linux`

| File | Contents |
|---|---|
| `pv_pylon.c` | The `pv-pylon` module: the PCI driver for `pylon-kick`, the kick handler, the per-CPU park threads, `/dev/pylon` and the `stats` attribute. |
| `pv_pylon_abi.h` | The shared page and BAR0 registers in C, with static assertions on the same offsets `pylon-abi` pins. |
| `Kbuild`, `Makefile` | Out-of-tree module build; `KDIR` selects the kernel headers. |

It follows kernel coding style (tabs; `checkpatch.pl` clean), and is
licensed `GPL-2.0-only OR BSD-3-Clause`, so it can use GPL-only kernel
symbols.

## VMM hook points

| Upstream file | Hook | Calls into `vmm/src/pylon.rs` |
|---|---|---|
| `cloud-hypervisor/src/main.rs` | The `--pylon` argument | `PylonConfig::SYNTAX` |
| `vmm/src/config.rs` | `VmParams.pylon`; parsing during `VmConfig` construction | `parse_and_configure` |
| `vmm/src/memory_manager.rs` | `MemoryManager::new`; `Error::Pylon` | `setup_memory_region` |
| `vmm/src/cpu.rs` | vCPU thread start; after each `KVM_RUN` exit; shutdown | `on_vcpu_thread_start`, `check_and_park_vcpu`, `shutdown_all` |
| `vmm/src/lib.rs` | `pub mod pylon;` | — |
| `vmm/Cargo.toml` | `pylon-abi` dependency | — |

[VMM integration](vmm-integration.md) describes each hook.

## Where to find things

| To change… | Look in |
|---|---|
| A field of the shared page | `pylon-abi/src/lib.rs`, then every reader and writer ([Shared-memory ABI](shared-memory-abi.md#changing-the-abi)) |
| How cores are chosen | `MatrixEngine::evaluate_demand_phase1`, `match_hungry_vms_phase3`, `select_donor_pcpu` |
| How revocations finish | `MatrixEngine::progress_revocations_phase2` |
| A control socket message | `pylond/src/lib.rs` (types) and `handle_client_connection` in `control.rs` |
| A metric | `run_prometheus_server` in `control.rs` |
| What the VMM does on a vCPU exit | `check_and_park_vcpu` in `vmm/src/pylon.rs` |
| When `scx_pylon` attaches | `scx_attach` in `pylond/src/main.rs` |
| The BPF scheduling policy | `scx_pylon.bpf.c` |
