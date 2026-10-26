# Pylon: Microsecond Core Elasticity for Glidex Cloud

## 1. Overview & Motivation
**Pylon** (`CMKL-APEX/pylon`) is the microsecond-scale elastic Virtual Machine Monitor (VMM) and host CPU core arbitration subsystem for the **Glidex Cloud** suite. Built as a surgical fork of **Cloud Hypervisor**, Pylon eliminates the "double-scheduling tax" and lock-holder preemption pathologies of traditional multi-tenant virtualization while preserving strict hardware-enforced KVM isolation (`VT-x` / `AMD-V` + IOMMU).

### Core Architectural Invariants
1. **Host Space-Sharing (Zero Time-Slicing of Active vCPUs):**
   The host OS never time-slices competing runnable vCPUs on the same physical CPU (`pCPU`). Every tenant physical core is configured with `isolcpus`, `nohz_full`, and `rcu_nocbs` and runs **at most one active vCPU thread** at any instant.
2. **Guest Time-Sharing (Cooperative Demand & Preemption Deferral):**
   The guest OS (unmodified Linux with `pv-pylon` kmod, or a Rust `ostd`/Asterinas framekernel) schedules internal threads across its currently granted vCPUs. It exposes real-time runqueue demand, timer deadlines, and per-vCPU critical-section counters via a cache-line-aligned shared memory page (`/dev/shm/<vm-id>-pylon`).
3. **Zero-Syscall Microsecond Actuation (~3-6 us Core Handoff):**
   A single node-wide arbiter daemon (`pylond`) busy-polls the shared memory matrix across all colocated VMs using `_rdtsc` and `_tpause`. To move a physical core from a Donor VM to a Recipient VM, `pylond` sets a cooperative revocation flag, fires a KVM `irqfd` interrupt, and updates an `mmap`-able Linux `sched_ext` (`BPF_F_MMAPABLE`) array map (`scx_pylon`) to switch `host_tid` ownership directly on the local CPU dispatch queue (`SCX_DSQ_LOCAL`).

---

## 2. Repository Directory Layout & Upstream Isolation Rules

To keep merges from upstream `cloud-hypervisor/cloud-hypervisor` (`upstream-main`) conflict-free, **all new Pylon crates live under `pylon/`**, and **95% of VMM hooks live in a single new file (`vmm/src/pylon.rs`)**.

```text
CMKL-APEX/pylon/
├── Cargo.toml                     # Root workspace (append "pylon/*" members at bottom)
├── spec/                          # Architectural specifications
│   ├── 00-architecture.md
│   ├── 01-shared-memory-abi.md
│   ├── 02-pylond-arbiter.md
│   └── 03-scx-and-vmm-hooks.md
├── pylon/                         # Standalone Pylon crates (Zero upstream conflict)
│   ├── pylon-abi/                 # #![no_std] 64B cache-line-aligned shared memory ABI
│   ├── pylond/                    # Two-plane Node Core Arbiter daemon
│   ├── scx-pylon/                 # Linux sched_ext (libbpf-rs + C) host dispatcher
│   └── guest/                     # Guest OS integration (Asterinas ostd & Linux kmod)
└── vmm/                           # Upstream Cloud Hypervisor VMM crate
    └── src/
        ├── pylon.rs               # Self-contained Pylon VMM shared-memory & fast-park logic
        ├── config.rs              # Surgical hook: --pylon configuration parameter
        ├── memory_manager.rs      # Surgical hook: map /dev/shm/<vm-id>-pylon into GPA
        └── cpu.rs                 # Surgical hook: export host_tid & fast-park exit loop
```

---

## 3. End-to-End Core Handoff State Machine

```text
Donor vCPU (VM A on pCPU_x)        pylond (Core 1 Engine)          scx_pylon (BPF) & Recipient (VM B)
---------------------------        ----------------------          ----------------------------------
1. Running on pCPU_x               Detects VM B demand via
                                   PylonTelemetry::head_wait_tsc
                                             |
                                   Sets VM A VcpuSlot:
                                   - revoke_requested = 1
                                   - revoke_deadline_tsc = now + 8us
                                   Fires VM A vCPU kick_irqfd -->
         <---------------------------------------------------------
2. Receives IPI / Posted Interrupt
   Checks in_critical_section > 0
   - Finishes spinlock (~200ns)
   - Saves thread to guest runqueue
   - Sets state = VCPU_STATE_PARKED
   - Executes HLT / Hypercall Yield
         |
         +-----------------------> 3. Sees state == PARKED
                                       (or deadline expired)
                                       Writes VM B host_tid to
                                       BPF_F_MMAPABLE pcpu_owner[x] --> 4. VM A vCPU thread blocks;
                                       Wakes VM B vCPU kick/futex       scx_pylon ops.dispatch()
                                                                        switches pCPU_x directly to
                                                                        VM B vCPU thread!
```
