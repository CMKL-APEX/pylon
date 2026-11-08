# Developer guide

This guide is for people who want to change Pylon: fix a bug in the
arbiter, extend the shared-memory ABI, wire a new hook into the VMM, or
build the guest side. It assumes you know Rust and have used KVM. It
doesn't assume you know Cloud Hypervisor's internals or `sched_ext`.

To run Pylon, read the [Operator guide](../operator-guide/README.md)
instead. To use it, read the [User guide](../user-guide/README.md).

## Pylon from a contributor's angle

Pylon lives in a fork of Cloud Hypervisor and is built to stay
mergeable with upstream. Almost all of it is in standalone crates
under `pylon/`. The VMM side is two files, `vmm/src/pylon.rs` and
`vmm/src/pylon_kick.rs`, plus a handful of short hooks in upstream
files. Four Rust crates, one BPF program and one guest kernel module
make it up:

- **`pylon-abi`**: the `#![no_std]` layout of the 8 KiB shared page
  that every other part reads and writes.
- **`vmm/src/pylon.rs`**: creates the page, maps it into the guest,
  registers the VM with `pylond`, and parks and wakes vCPU threads.
  **`vmm/src/pylon_kick.rs`** is the `pylon-kick` PCI device, which turns
  `pylond`'s kicks into guest interrupts.
- **`pylond`**: a two-thread daemon. A Tokio control plane on host core 0
  takes registrations and requests. An allocation-free polling engine
  on core 1 decides, every microsecond, which vCPU owns each managed
  CPU.
- **`scx-pylon`**: a `sched_ext` BPF scheduler and its `libbpf-rs`
  loader. It makes the kernel run each CPU's owner, read from a
  memory-mapped BPF map that `pylond` writes.
- **`pylon-guest-agent`**: a userspace guest daemon that reports demand.
  It only writes telemetry.
- **`pv-pylon`** (`pylon/guest/linux/`): a Linux guest kernel module in
  C. It drives `pylon-kick` and parks a revoked vCPU at a safe point.

## Pages

| Page | What it covers |
|---|---|
| [Repository and code organization](organization.md) | Every crate and file, the upstream hook points, and where to find things. |
| [Architecture](architecture.md) | The processes, what each owns, and how a VM's registration and a core handoff flow between them. |
| [Shared-memory ABI](shared-memory-abi.md) | The page's layout, who writes which field, memory ordering, and how to change it. |
| [The arbitration engine](arbitration.md) | `pylond`'s polling loop: demand, urgency, donor selection, revocation and metering. |
| [VMM integration](vmm-integration.md) | `vmm/src/pylon.rs` and its hooks into Cloud Hypervisor. |
| [The scx_pylon dispatcher](scx-pylon.md) | The BPF scheduler, its map, and how `pylond` loads it. |
| [Design decisions and trade-offs](design-decisions.md) | Why Pylon is shaped the way it is, and what each choice costs. |
| [Enhancements](enhancements.md) | The revocation path fix (spec 06), parking inside KVM (spec 08), and other planned work. |
| [Performance and benchmarks](benchmarks.md) | Figures, with their source, date and hardware, and how to reproduce them. |
| [Contributing](contributing.md) | Building, tests, CI, conventions and the pull request flow. |

## Three sets of documentation

| Set | Where | Read it for |
|---|---|---|
| This book | `docs/pylon/src` in the repository, published at <https://cmkl-apex.github.io/pylon/docs/> | What Pylon does, how to run it, and, in this guide, how it's built. |
| Design specs | [`spec/`](https://github.com/CMKL-APEX/pylon/tree/main/spec) | The authority on *why*: contracts between components, field layouts, the implementation plan and status. Code comments cite them as `spec/<file>.md`. |
| API reference (rustdoc) | Built locally with `cargo doc` | Every crate's types and functions. |

The specs:

| Spec | Covers |
|---|---|
| [00-architecture.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/00-architecture.md) | Overall design, upstream isolation rules, the handoff state machine. |
| [01-shared-memory-abi.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/01-shared-memory-abi.md) | The shared page's layout and coherency rules. |
| [02-pylond-arbiter.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/02-pylond-arbiter.md) | `pylond`'s two planes and the arbitration algorithm. |
| [03-scx-and-vmm-hooks.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/03-scx-and-vmm-hooks.md) | `scx_pylon` and the VMM hooks. |
| [04-implementation-plan.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/04-implementation-plan.md) | The original track plan and upstream touch points. |
| [05-implementation-status.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md) | Status tracker, known issues and CI coverage. |
| [06-guest-kick-path.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/06-guest-kick-path.md) | The guest kick and cooperative park design (draft). |
| [07-aarch64-support.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/07-aarch64-support.md) | aarch64 host and guest support (draft). |
| [08-park-in-kvm.md](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md) | Parking vCPUs inside KVM so interrupts wake them (draft). |

Specs 00–03 describe the design as first planned. Where the code
differs, this guide describes the code and says so.

## First day

1. Read [Concepts](../concepts.md) and [Architecture](architecture.md).
2. Build and run the unit tests:
   `cargo test -p pylon-abi -p pylond -p scx-pylon -p pylon-guest-agent`.
3. Run the end-to-end tests, which start an in-process `pylond` engine
   against simulated VMs: `cargo test -p pylond --test e2e_pylon`.
4. Read [`pylon/pylon-abi/src/lib.rs`](https://github.com/CMKL-APEX/pylon/blob/main/pylon/pylon-abi/src/lib.rs)
   end to end. It's short, and everything else depends on it.
5. Read `MatrixEngine::poll_once` in
   [`pylon/pylond/src/engine.rs`](https://github.com/CMKL-APEX/pylon/blob/main/pylon/pylond/src/engine.rs)
   with [The arbitration engine](arbitration.md) open beside it.
6. On a host with KVM, run the VMM tests, including one that boots a
   real KVM VM against the shared page:
   `cargo test -p vmm --features kvm --lib pylon`.
7. Read [Enhancements](enhancements.md) for what is broken and what is
   planned before you pick up work. Open work is tracked as GitHub
   issues on the
   [`v0.0.8` milestone](https://github.com/CMKL-APEX/pylon/milestones).
