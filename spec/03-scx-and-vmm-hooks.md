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
  If task `p->pid` matches a `target_tid` in `pylon_pcpu_owners`, return that assigned `pCPU`. `enqueue` puts it on that pCPU's owner queue.
- **`scx_ops.enqueue(p, enq_flags)`:**
  The owner of a pCPU goes on that pCPU's queue (DSQ id = CPU) with an infinite slice and preempts whatever runs there. That includes a donor vCPU whose `target_tid` was replaced. Other managed vCPU threads go on a shared vCPU queue, and other tasks go on a host queue.
- **`scx_ops.dispatch(cpu, prev)`:**
  Run the pCPU's owner if it is queued. Otherwise take a task from the vCPU queue or the host queue, but only one that the SMT isolation rules below allow.

### SMT Isolation

`scx_pylon` keeps SMT siblings apart for managed vCPU threads, in place of kernel core scheduling (`PR_SCHED_CORE`). Using sched_ext together with core scheduling hangs kernels that lack the upstream fixes for 4c95380701f5 (issue #15).

- A task's *domain* is its VMM process's tgid once it has owned a pCPU, and *host* otherwise.
- Each CPU publishes a claim before it runs work: idle, host, or a domain. The claim is set in `dispatch` and `running`, and cleared when the CPU goes idle (`update_idle`).
- A CPU only runs work whose claim matches all its siblings' claims or sits next to idle siblings. A domain claim beats a host claim. Between two domains, the lower-numbered CPU wins. The loser is kicked with `SCX_KICK_PREEMPT` and idles.
- The siblings come from `pylon_smt_next`, a ring that userspace fills from `thread_siblings_list` before attaching.
- `pylond` doesn't give two VMs sibling threads of one core while `scx_pylon` is attached, so the cross-domain tie-break doesn't keep a VM's vCPU waiting.
- Not isolated: tasks of higher sched classes (RT, deadline, stop), interrupts and softirqs, and tasks with migration disabled. Kernel core scheduling doesn't isolate interrupts either.

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
