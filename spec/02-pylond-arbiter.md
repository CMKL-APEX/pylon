# Specification 02: Node Core Arbiter Daemon (`pylond`)

## 1. Two-Plane Process Architecture
`pylond` is a standalone Rust binary running once per physical server (or once per NUMA socket). It is strictly divided into two execution planes:

### Plane 1: Control Thread (Pinned to Host Core 0)
- Runs an asynchronous Tokio runtime listening on a Unix domain socket (`/run/pylon/pylond.sock`).
- **VM Registration API:** When a `cloud-hypervisor` process boots with `--pylon shm=/dev/shm/<vm-id>-pylon,socket=/run/pylon/pylond.sock`, it connects to `pylond` and passes:
  - The VM metadata (`vm_id`, `num_vcpus`, `min_cores`, `max_cores`, `priority_weight`).
  - File descriptors via `SCM_RIGHTS`: the `memfd`/shared-memory file descriptor and the per-vCPU KVM `irqfd` (for injecting revocation IPIs) and wake `eventfd`s.
- **Lockless Publication:** Plane 1 `mmap`s the `PylonSharedPage`, constructs a `VmHandle`, and publishes a new immutable `Arc<Vec<VmHandle>>` snapshot to Plane 2 via `arc_swap::ArcSwap`.

### Plane 2: Real-Time Matrix Engine (Pinned to Host Core 1, `nohz_full`)
- Zero heap allocations, zero mutexes, and zero blocking syscalls in the steady-state loop.
- Uses x86_64 `_rdtsc()` for timing and `_tpause(0, tsc + 1000)` (~300-400 ns pause) between polling sweeps to maintain sub-microsecond responsiveness without thermal throttling.

---

## 2. Core Allocation & Revocation Algorithm

Every polling tick (~1 us), the Matrix Engine executes four phases:

### Phase 1: Demand & Zero-Core Wakeup Evaluation
For each registered `VmHandle`:
1. Load `runnable = telemetry.runnable_threads.load(Acquire)`.
2. Load `head_wait = now_tsc.saturating_sub(guest_to_host_tsc(telemetry.head_wait_tsc.load(Relaxed)))` (the guest writes guest TSC; spec 01 *Time bases*).
3. **Zero-Core Check:** If `vm.active_cores == 0`:
   - Check if `now_tsc >= guest_to_host_tsc(telemetry.next_timer_tsc.load(Relaxed))`, or if any free parked vCPU's `park_timer_deadline_tsc` (host TSC, written by the VMM) is non-zero and has passed.
   - Check if the host-side vDPA/virtio RX doorbell counter has advanced past `telemetry.rx_ring_cons_idx.load(Relaxed)`.
   - If either is true, immediately mark the VM as requiring at least 1 physical core with maximum urgency.
4. **Stranded Work Preference:** When selecting which parked `vCPU_i` to wake for a VM, always check `telemetry.stranded_work_bitmap.load(Acquire)` first (`tzcnt` on the bitmap) so vCPUs holding local runqueue work or RCU callbacks are woken before fresh vCPUs. Next, prefer a vCPU whose `park_timer_deadline_tsc` has passed, since its timer interrupt can't fire while it is parked.

### Phase 2: Progress In-Flight Revocations
For each managed tenant physical core (`pCPU`) in `CoreState::Revoking { donor_vm, donor_vcpu, recipient_vm, recipient_vcpu, deadline_tsc }`:
1. Check `donor_slot.state.load(Acquire) == VCPU_STATE_PARKED` OR `now_tsc >= deadline_tsc`.
2. If true:
   - If `now_tsc >= deadline_tsc` and not parked, increment `donor_slot.forced_preempt_count`.
   - Write `recipient_slot.host_tid` into the `scx_pylon` memory-mapped BPF array for `pCPU`.
   - Set `recipient_slot.assigned_pcpu.store(pcpu_id, Release)` and `recipient_slot.state.store(VCPU_STATE_WAKING, Release)`.
   - Trigger `recipient_vm.wake_vcpu(recipient_vcpu)` (writes `1` to the vCPU's wake `eventfd`/futex).
   - Transition `pCPU` to `CoreState::Assigned { vm: recipient_vm, vcpu: recipient_vcpu }`.

### Phase 3: Match Hungry VMs with Idle or Donor Cores
For each hungry VM (ordered by SLO urgency / `head_wait`):
1. **Fast Path (Idle Core Available):** If a tenant `pCPU` is in `CoreState::Idle`, immediately assign `recipient_tid` in the BPF map and wake the recipient vCPU (`~1.5 us` total latency).
2. **Preemption Path (Rebalance from Over-Allocated Donor):** Select a donor VM whose active core count exceeds its fair share or has lower urgency. Lower urgency alone qualifies only while the donor keeps at least the recipient's weighted share of cores after the move, and a donor vCPU that is still `PARKED` (not yet woken after its assignment) is never chosen (spec 05 K7):
   - Prefer preempting a donor vCPU where `in_critical_section.load(Acquire) == 0`.
   - Set `donor_slot.revoke_deadline_tsc.store(now_tsc + eight_us_in_cycles, Relaxed)`.
   - Set `donor_slot.revoke_requested.store(1, Release)`.
   - Trigger `donor_vm.kick_revoke_irqfd(donor_vcpu)` to deliver an immediate IPI to the donor vCPU.
   - Transition `pCPU` to `CoreState::Revoking`.
