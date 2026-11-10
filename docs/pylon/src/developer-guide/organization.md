# Repository and code organization

## Upstream isolation

`CMKL-APEX/pylon` tracks upstream Cloud Hypervisor, and upstream changes
are merged in regularly. To keep those merges cheap, Pylon follows the
rule in
[`spec/00-architecture.md` §2](https://github.com/CMKL-APEX/pylon/blob/main/spec/00-architecture.md#2-repository-directory-layout--upstream-isolation-rules):

- New crates live under `pylon/`, where upstream never writes.
- VMM-side logic lives in new files: `vmm/src/pylon.rs`, and
  `vmm/src/pylon_kick.rs` for the `pylon-kick` device.
- Upstream files get only short hook calls into those files.

When you add VMM behavior, put it in `vmm/src/pylon.rs` (or a sibling
new file) and add the smallest possible call in the upstream file.

## Crates

| Crate | Path | Lines | Kind | What it is |
|---|---|---|---|---|
| `pylon-abi` | [`pylon/pylon-abi`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylon-abi) | ~520 | `#![no_std]` library | The shared page's types and constants, the `pylon-kick` registers, the counter, and compile-time layout assertions. |
| `pylond` | [`pylon/pylond`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylond) | ~7,100 | Library + binaries `pylond`, `pylon-bench` | The arbiter. |
| `scx-pylon` | [`pylon/scx-pylon`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/scx-pylon) | ~2,600 | Library + binary `scx_pylon` | The BPF scheduler, its build script and loader. |
| `pylon-guest-agent` | [`pylon/pylon-guest-agent`](https://github.com/CMKL-APEX/pylon/tree/main/pylon/pylon-guest-agent) | ~770 | Library + binary | The userspace guest daemon. |

`pylon/guest/linux/` isn't a crate: it is the `pv-pylon` Linux kernel
module, in C, built with Kbuild (`make -C pylon/guest/linux`). See [the
guest module](#pylonguestlinux) below.

Line counts include tests and were taken on 2026-11-08.

## Files

### `pylon-abi`

| File | Contents |
|---|---|
| `src/lib.rs` | `PylonHeader`, `PylonTelemetry`, `PylonVcpuSlot`, `PylonSharedPage`, the `VCPU_STATE_*` and other constants, the `pylon-kick` PCI ID and BAR0 register offsets (`PYLON_KICK_*`), and `const` assertions on every size and key offset. |
| `src/clock.rs` | `read_counter` (the TSC on x86_64, `CNTVCT_EL0` on aarch64; spec 07 §3) and `architectural_counter_khz` (`CNTFRQ_EL0` on aarch64; none on x86_64, where `pylond` measures the rate). |

### `pylond`

| File | Contents |
|---|---|
| `src/lib.rs` | Shared types: `VmHandle` (one registered VM: its mapped page, `eventfd`s, policy and counters), `VmSnapshot` (`ArcSwap<Vec<VmHandle>>`), the pCPU owner table, the VM slot table behind `pylon_vm_vcpus`, `ParkLatencyHistogram`, `CoreState`, the JSON request and response types, and CPU pinning. |
| `src/control.rs` | Plane 1: `ControlPlane`, the Unix socket server, `SCM_RIGHTS` file descriptor passing, registration, policy and stats handling, the Prometheus HTTP server, and client helpers (`send_registration` and the like). |
| `src/engine.rs` | Plane 2: `MatrixEngine`, the polling loop, the counter, rate and timed-wait helpers (TSC and `TPAUSE` on x86_64, `CNTVCT_EL0` and `WFET` on aarch64). |
| `src/main.rs` | The `pylond` binary: options, `scx_pylon` attach policy, thread start-up. |
| `src/bin/pylon_bench.rs` | The Pylon vs. ACPI hotplug model benchmark. |
| `tests/e2e_pylon.rs` | End-to-end tests: a real control plane and engine, with VMs simulated by threads. |

### `scx-pylon`

| File | Contents |
|---|---|
| `src/bpf/scx_pylon.bpf.c` | The `sched_ext` scheduler: the `pylon_pcpu_owners`, `pylon_smt_next`, `pylon_vcpu_dom`, `pylon_vm_vcpus`, `pylon_task_ctx` and `pylon_guard` maps, the owner, vCPU, per-VM and host queues, SMT isolation, and the core scheduling guard. |
| `src/bpf/vmlinux.h` | Kernel type definitions for the BPF build (generated; excluded from linters). |
| `build.rs` | Compiles the BPF object with `clang -target bpf` into `OUT_DIR`. |
| `src/lib.rs` | `ScxPylonUserspace` (open, load, attach), `PcpuOwnerMap` (the userspace view of the owner map), `VcpuDomainMap`, and the SMT ring helpers. |
| `src/coresched.rs` | The guard against kernel core scheduling while `scx_pylon` is attached ([#15](https://github.com/CMKL-APEX/pylon/issues/15)). |
| `src/main.rs` | The standalone `scx_pylon` tool. |
| `tests/live_attach.rs` | Attaches to a real `sched_ext` kernel. Marked `#[ignore]` because it needs root: `sudo -E cargo test -p scx-pylon -- --ignored`. |

### `pylon-guest-agent`

| File | Contents |
|---|---|
| `src/lib.rs` | `PylonMemoryBackend` (`/dev/pylon` from `pv-pylon`, `/dev/mem`, or a file), `TelemetryReader` and its `/proc` implementation, `PylonGuestAgent`. |
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

| Upstream file | Hook | Calls into `vmm/src/pylon.rs` or `pylon_kick.rs` |
|---|---|---|
| `cloud-hypervisor/src/main.rs` | The `--pylon` argument | `PylonConfig::SYNTAX` |
| `vmm/src/config.rs` | `VmParams.pylon`; parsing during `VmConfig` construction | `parse_and_configure` |
| `vmm/src/memory_manager.rs` | `MemoryManager::new`; `Error::Pylon` | `setup_memory_region` |
| `vmm/src/cpu.rs` | `CpuManager::new`; vCPU thread start; after each `KVM_RUN` exit; shutdown | `check_hypervisor`, `on_vcpu_thread_start`, `read_guest_tsc`, `read_guest_tsc_deadline`, `check_and_park_vcpu`, `kvm_park_op`, `shutdown_all` |
| `vmm/src/device_manager.rs` | Creating the `pylon-kick` device when `--pylon` is set | `global_state`, `PylonKickDevice` |
| `vmm/src/seccomp_filters.rs` | Syscalls the vCPU threads need for Pylon, including park inside KVM | — |
| `vmm/src/lib.rs` | `pub mod pylon;`, `pub mod pylon_kick;` | — |
| `vmm/Cargo.toml` | `pylon-abi` dependency | — |
| `arch/src/x86_64/layout.rs`, `arch/src/aarch64/layout.rs` | `PYLON_SHARED_START` and `PYLON_SHARED_SIZE`, which `vmm/src/pylon.rs` checks against `pylon-abi` | — |
| `hypervisor/src/vm.rs`, `hypervisor/src/kvm/mod.rs` | `Vm::set_halt_poll_ns`, used to turn halt polling off for park inside KVM | — |

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
| How a vCPU parks inside KVM (`park=kvm`) | `check_and_park_vcpu` and `kvm_park_op` in `vmm/src/pylon.rs` |
| The `pylon-kick` device's registers and MSI-X | `vmm/src/pylon_kick.rs` |
| When `scx_pylon` attaches | `scx_attach` in `pylond/src/main.rs` |
| The BPF scheduling policy | `scx_pylon.bpf.c` |
