# Specification 03: `scx_pylon` BPF Dispatcher & Cloud Hypervisor VMM Hooks

## 1. `pylon/scx-pylon`: Zero-Syscall `sched_ext` Dispatcher

### Design Goal
Bypass standard Linux `sched_fair` (EEVDF) wakeup latency and runqueue lock contention when handing off a physical core (`pCPU`) from Donor vCPU thread `TID_A` to Recipient vCPU thread `TID_B`.

### BPF Map Contract (`scx_pylon.bpf.c`)
- Define an `mmap`-able BPF array map shared directly with `pylond` userspace:

```c
struct pcpu_owner_entry {
    __u32 target_tid;
    __u32 flags;
};

struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 256); // Indexed by physical CPU ID
    __uint(map_flags, BPF_F_MMAPABLE);
    __type(key, __u32);
    __type(value, struct pcpu_owner_entry);
} pylon_pcpu_owners SEC(".maps");
```

- **`scx_ops.select_cpu(p, prev_cpu, wake_flags)`:**
  If task `p->pid` matches a `target_tid` in `pylon_pcpu_owners`, return that assigned `pCPU` and insert directly into `SCX_DSQ_LOCAL`.
- **`scx_ops.dispatch(cpu, prev)`:**
  Look up `pylon_pcpu_owners[cpu]`. If `target_tid != 0`, dispatch the matching vCPU task directly to the local CPU queue; if `prev` is a donor vCPU whose `target_tid` was replaced (e.g., deadline expired), preempt `prev` immediately.

---

## 2. Cloud Hypervisor VMM Integration (`vmm/src/pylon.rs`)

All VMM-side logic must be encapsulated inside `vmm/src/pylon.rs` with minimal hook calls from upstream files.

### CLI & Configuration Hook (`vmm/src/config.rs`)
Add an optional `--pylon` configuration parameter to `VmParams` and `VmConfig`:

```text
--pylon shm=/dev/shm/<vm-id>-pylon[,socket=/run/pylon/pylond.sock,gpa=0xfedf0000]
```

### Memory Manager Hook (`vmm/src/memory_manager.rs`)
During guest memory initialization (`MemoryManager::new`):
1. If `pylon` config is present, invoke `crate::pylon::PylonVmmState::new(&config)`.
2. Create or open `/dev/shm/<vm-id>-pylon` (8,192 bytes), initialize `PylonHeader` (`PYLON_MAGIC`, `num_vcpus`), and map the backing file into the Guest Physical Address space at `PYLON_DEFAULT_GPA` (`0xFEDF_0000`) using `GuestRegionMmap` with `MEM_SHARED`.

### vCPU Thread & Fast-Park Hook (`vmm/src/cpu.rs`)
1. **Thread Initialization (`Vcpu::start_threaded`):**
   - At the start of each vCPU thread, obtain the Linux kernel thread ID via `unsafe { libc::gettid() as u32 }`.
   - Call `pylon_state.register_vcpu_tid(vcpu_id, tid)` to store the `TID` into `PylonSharedPage.vcpus[vcpu_id].host_tid`.
   - For all vCPUs where `vcpu_id >= initial_active_vcpus`, start in `VCPU_STATE_PARKED` (blocking on the vCPU's per-thread park `eventfd`/futex until woken by `pylond`).
2. **Fast-Park Exit Handler (`Vcpu::run` loop):**
   - When `self.vcpu.run()` exits with `VcpuExit::Hlt` or a Pylon hypercall while `slot.revoke_requested.load(Acquire) == 1`:
     - Clear `slot.revoke_requested.store(0, Release)`.
     - Set `slot.state.store(VCPU_STATE_PARKED, Release)`.
     - Block on the vCPU's park wait primitive (`pylon_state.park_vcpu(vcpu_id)`) until `pylond` re-assigns a physical core and signals the wake `eventfd`.
     - Upon wakeup, set `slot.state.store(VCPU_STATE_RUNNING, Release)` and immediately re-enter `KVM_RUN`.
