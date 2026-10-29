// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

#[cfg(target_arch = "x86_64")]
use core::arch::asm;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{__cpuid_count, _mm_pause, _rdtsc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};
use pylon_abi::{MAX_VCPUS, PCPU_UNASSIGNED, PylonVcpuSlot, VCPU_STATE_PARKED};

use crate::{
    CoreState, Error, MAX_PCPUS, PcpuOwnerTable, VmHandle, VmSnapshot, pin_current_thread_to_core,
};

/// Maximum number of concurrent VMs evaluated per stack-allocated sweep in Plane 2.
pub const MAX_TRACKED_VMS: usize = 64;

/// TSC rate (3.0 GHz = 3,000,000 kHz) assumed when neither a VM's header nor a host measurement
/// gives one.
pub const DEFAULT_TSC_KHZ: u64 = 3_000_000;

/// Default revocation grace period in microseconds (`spec/00-architecture.md` & `spec/02-pylond-arbiter.md`).
pub const REVOKE_GRACE_PERIOD_US: u64 = 8;

/// How long after a revocation's deadline `pylond` keeps waiting for the donor to park before it
/// hands the core over anyway (`spec/06-guest-kick-path.md` §4.5).
pub const FORCE_HANDOFF_US: u64 = 50;

/// Interval at which `pylond` re-sends the kick signal to a donor that hasn't parked. A signal
/// that lands while the vCPU thread is outside `KVM_RUN` doesn't stop its next guest entry.
pub const REVOKE_RESIGNAL_US: u64 = 10;

/// Default `_tpause` delay in TSC cycles (~300-400 ns on a 3 GHz core).
pub const DEFAULT_TPAUSE_CYCLES: u64 = 1000;

/// Measures the host TSC rate in kHz against `CLOCK_MONOTONIC` over `window`. Returns `None`
/// where there is no TSC.
pub fn calibrate_tsc_khz(window: Duration) -> Option<u64> {
    if !cfg!(target_arch = "x86_64") {
        return None;
    }
    let start = Instant::now();
    let start_tsc = read_tsc();
    thread::sleep(window);
    let cycles = read_tsc().saturating_sub(start_tsc);
    let nanos = u64::try_from(start.elapsed().as_nanos()).ok()?;
    if nanos == 0 || cycles == 0 {
        return None;
    }
    // cycles per nanosecond * 1e6 = kHz
    u64::try_from(u128::from(cycles) * 1_000_000 / u128::from(nanos)).ok()
}

/// Detects whether the current x86_64 CPU supports the `WAITPKG` (`tpause`) instruction set
/// (CPUID.07H:0H.ECX[bit 5]).
#[inline]
pub fn detect_waitpkg() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `cpuid` leaf 0 and leaf 7 are always safe to query on x86_64 processors.
        #[allow(unused_unsafe)]
        let (leaf0, leaf7) = unsafe { (__cpuid_count(0, 0), __cpuid_count(7, 0)) };
        if leaf0.eax < 7 {
            return false;
        }
        (leaf7.ecx & (1 << 5)) != 0
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

/// Reads the current x86_64 Time-Stamp Counter (`rdtsc`).
#[inline]
pub fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `_rdtsc` is always safe to execute on x86_64 processors.
        unsafe { _rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn tpause_waitpkg(control: u32, deadline_tsc: u64) {
    let lo = deadline_tsc as u32;
    let hi = (deadline_tsc >> 32) as u32;
    // SAFETY: Caller verified CPUID `WAITPKG` support; `tpause ecx` (`66 0f ae f1`) pauses
    // execution in C0.1/C0.2 until `EDX:EAX` reaches `deadline_tsc` without touching memory.
    unsafe {
        asm!(
            ".byte 0x66, 0x0f, 0xae, 0xf1",
            in("ecx") control,
            in("edx") hi,
            in("eax") lo,
            options(nomem, nostack)
        );
    }
}

/// Pauses the polling core for `cycles` TSC ticks using `_tpause(0, now_tsc + cycles)` when the
/// CPU supports `WAITPKG`, falling back to `_mm_pause()` on older x86_64 processors.
#[inline]
pub fn pause_cycles(now_tsc: u64, cycles: u64, has_waitpkg: bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if has_waitpkg {
            let target = now_tsc.wrapping_add(cycles);
            // SAFETY: `has_waitpkg` is only true when CPUID verified `WAITPKG` support.
            unsafe {
                tpause_waitpkg(0, target);
            }
        } else {
            // SAFETY: `_mm_pause` (`pause`) is always safe to execute on x86_64 processors.
            #[allow(unused_unsafe)]
            unsafe {
                _mm_pause();
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (now_tsc, cycles, has_waitpkg);
        core::hint::spin_loop();
    }
}

/// Per-VM demand evaluation record computed in Phase 1 on the stack (zero heap allocations).
#[derive(Debug, Clone, Copy)]
struct VmDemandEval {
    vm_idx: usize,
    vm_id: u64,
    min_cores: u32,
    assigned_cores: u32,
    incoming_cores: u32,
    desired_cores: u32,
    urgency: u64,
    zero_core_wakeup: bool,
}

impl VmDemandEval {
    const EMPTY: Self = Self {
        vm_idx: 0,
        vm_id: 0,
        min_cores: 0,
        assigned_cores: 0,
        incoming_cores: 0,
        desired_cores: 0,
        urgency: 0,
        zero_core_wakeup: false,
    };

    #[inline]
    fn committed_cores(self) -> u32 {
        self.assigned_cores + self.incoming_cores
    }

    #[inline]
    fn needed_cores(self) -> u32 {
        self.desired_cores.saturating_sub(self.committed_cores())
    }
}

/// Plane 2: Real-Time Matrix Engine (Pinned to Host Core 1).
///
/// Executes the 3-phase microsecond arbitration loop with zero heap allocations, zero mutexes,
/// and zero blocking syscalls in steady state.
pub struct MatrixEngine {
    vm_snapshot: Arc<VmSnapshot>,
    pcpu_owners: Arc<PcpuOwnerTable>,
    core_states: [CoreState; MAX_PCPUS],
    managed_pcpus: [bool; MAX_PCPUS],
    eval_buf: [VmDemandEval; MAX_TRACKED_VMS],
    core_last_metered_tsc: [u64; MAX_PCPUS],
    /// The assigned vCPU's `yield_count` when its core was assigned: a higher count means the
    /// VMM has parked it since, so the core can be reclaimed.
    assigned_yield_count: [u64; MAX_PCPUS],
    /// TSC at which a `Revoking` core's revocation was requested (0 if none).
    revoke_requested_tsc: [u64; MAX_PCPUS],
    /// TSC at which the donor was last signalled after its deadline passed (0 if not yet).
    revoke_signalled_tsc: [u64; MAX_PCPUS],
    /// Host TSC rate, used for VMs whose header gives none.
    host_tsc_khz: u64,
    has_waitpkg: bool,
}

impl MatrixEngine {
    /// Creates a new `MatrixEngine` managing the specified tenant physical cores (`tenant_pcpus`).
    pub fn new(
        vm_snapshot: Arc<VmSnapshot>,
        pcpu_owners: Arc<PcpuOwnerTable>,
        tenant_pcpus: &[usize],
    ) -> Self {
        let mut managed_pcpus = [false; MAX_PCPUS];
        for &pcpu in tenant_pcpus {
            if pcpu < MAX_PCPUS {
                managed_pcpus[pcpu] = true;
            }
        }
        let has_waitpkg = detect_waitpkg();

        Self {
            vm_snapshot,
            pcpu_owners,
            core_states: [CoreState::Idle; MAX_PCPUS],
            managed_pcpus,
            eval_buf: [VmDemandEval::EMPTY; MAX_TRACKED_VMS],
            core_last_metered_tsc: [0; MAX_PCPUS],
            assigned_yield_count: [0; MAX_PCPUS],
            revoke_requested_tsc: [0; MAX_PCPUS],
            revoke_signalled_tsc: [0; MAX_PCPUS],
            host_tsc_khz: DEFAULT_TSC_KHZ,
            has_waitpkg,
        }
    }

    /// Sets the host TSC rate used for VMs whose header gives none (`tsc_khz == 0`), such as a
    /// value from `calibrate_tsc_khz`.
    pub fn set_host_tsc_khz(&mut self, tsc_khz: u64) {
        if tsc_khz > 0 {
            self.host_tsc_khz = tsc_khz;
        }
    }

    /// Returns the current `CoreState` of physical core `pcpu_id`.
    #[inline]
    pub fn core_state(&self, pcpu_id: usize) -> CoreState {
        self.core_states[pcpu_id]
    }

    /// Explicitly sets the `CoreState` of physical core `pcpu_id` (used for setup/testing).
    #[inline]
    pub fn set_core_state(&mut self, pcpu_id: usize, state: CoreState) {
        if pcpu_id < MAX_PCPUS {
            self.core_states[pcpu_id] = state;
            self.core_last_metered_tsc[pcpu_id] = 0;
        }
    }

    /// Returns a reference to the underlying `PcpuOwnerTable`.
    #[inline]
    pub fn pcpu_owners(&self) -> &Arc<PcpuOwnerTable> {
        &self.pcpu_owners
    }

    /// Pins the calling thread to Host Core 1 and runs the real-time polling loop until `stop` is set.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), Error> {
        if let Err(err) = pin_current_thread_to_core(1) {
            warn!("Plane 2 could not pin to Core 1: {err}");
        }
        info!(
            "Plane 2 (Matrix Engine) started (waitpkg={})",
            self.has_waitpkg
        );

        while !stop.load(Ordering::Relaxed) {
            let now_tsc = read_tsc();
            self.poll_once(now_tsc);
            pause_cycles(now_tsc, DEFAULT_TPAUSE_CYCLES, self.has_waitpkg);
        }
        Ok(())
    }

    /// Selects a parked vCPU (`VCPU_STATE_PARKED`) for `vm`, checking `stranded_work_bitmap`
    /// first via `trailing_zeros()` (`tzcnt`) before scanning remaining vCPU slots.
    #[inline]
    pub fn select_parked_vcpu(vm: &VmHandle) -> Option<u8> {
        let page = vm.shared_page();
        let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
        let mut bitmap = page.telemetry.stranded_work_bitmap.load(Ordering::Acquire);

        while bitmap != 0 {
            let vcpu_idx = bitmap.trailing_zeros() as usize;
            let bit = 1u64 << vcpu_idx;
            if vcpu_idx < num_vcpus && Self::is_unassigned_parked(&page.vcpus[vcpu_idx]) {
                page.telemetry
                    .stranded_work_bitmap
                    .fetch_and(!bit, Ordering::AcqRel);
                return Some(vcpu_idx as u8);
            }
            bitmap &= !bit;
        }

        let mut idx = 0usize;
        while idx < num_vcpus {
            if Self::is_unassigned_parked(&page.vcpus[idx]) {
                return Some(idx as u8);
            }
            idx += 1;
        }
        None
    }

    /// A vCPU is free to receive a core when it is parked and no core is on its way to it.
    /// `pylond` doesn't write `state` (`spec/06-guest-kick-path.md` §3), so a vCPU that was just
    /// assigned stays `PARKED` until its thread wakes; `assigned_pcpu` tells the two apart.
    #[inline]
    fn is_unassigned_parked(slot: &PylonVcpuSlot) -> bool {
        slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
            && slot.assigned_pcpu.load(Ordering::Acquire) == PCPU_UNASSIGNED
    }

    /// Converts microseconds to TSC cycles at `tsc_khz`.
    #[inline]
    fn us_to_cycles(usec: u64, tsc_khz: u64) -> u64 {
        tsc_khz.saturating_mul(usec) / 1000
    }

    /// The TSC rate in `vm`'s header, or the host's if the header gives none.
    #[inline]
    fn tsc_khz_of(&self, vm: &VmHandle) -> u64 {
        match vm.shared_page().header.tsc_khz {
            0 => self.host_tsc_khz,
            khz => khz,
        }
    }

    /// Gives `pcpu` to `vcpu` of `vm`: publishes the owner, records the assignment, and wakes
    /// the vCPU thread. The VMM moves the vCPU from `PARKED` to `RUNNING` when it wakes.
    fn assign_pcpu(&mut self, pcpu: usize, vm: &VmHandle, vcpu: u8, now_tsc: u64) {
        let slot = &vm.shared_page().vcpus[vcpu as usize];
        let tid = slot.host_tid.load(Ordering::Acquire);
        self.pcpu_owners.write_owner(pcpu, tid, 0);
        self.assigned_yield_count[pcpu] = slot.yield_count.load(Ordering::Acquire);
        slot.assigned_pcpu.store(pcpu as u8, Ordering::Release);
        let _ = vm.wake_vcpu(vcpu as usize);

        self.core_states[pcpu] = CoreState::Assigned {
            vm_id: vm.vm_id(),
            vcpu,
        };
        self.core_last_metered_tsc[pcpu] = now_tsc;
        self.revoke_requested_tsc[pcpu] = 0;
        self.revoke_signalled_tsc[pcpu] = 0;
    }

    /// Executes one full 3-phase arbitration sweep at timestamp `now_tsc`.
    ///
    /// Guaranteed to perform zero heap allocations, zero mutex locks, and zero blocking syscalls.
    pub fn poll_once(&mut self, now_tsc: u64) {
        let vms_guard = self.vm_snapshot.load();
        let vms: &[VmHandle] = vms_guard.as_slice();
        let vm_count = vms.len().min(MAX_TRACKED_VMS);
        // =====================================================================
        // Phase 0: Reclaim Idled Cores & Meter Core-Microsecond Residency
        // =====================================================================
        self.reclaim_idle_assigned_cores(vms, vm_count, now_tsc);
        self.meter_core_residency(vms, vm_count, now_tsc);

        // =====================================================================
        // Phase 1: Demand & Zero-Core Wakeup Evaluation
        // =====================================================================
        self.evaluate_demand_phase1(vms, vm_count, now_tsc);

        // =====================================================================
        // Phase 2: Progress In-Flight Revocations
        // =====================================================================
        self.progress_revocations_phase2(vms, vm_count, now_tsc);

        // =====================================================================
        // Phase 3: Match Hungry VMs with Idle or Donor Cores
        // =====================================================================
        self.match_hungry_vms_phase3(vms, vm_count, now_tsc);
    }

    fn reclaim_idle_assigned_cores(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            // A VM whose registration was removed (its VMM exited) gives its cores back.
            if self.managed_pcpus[pcpu]
                && let CoreState::Assigned { vm_id, .. } = self.core_states[pcpu]
                && !vms.iter().any(|vm| vm.vm_id() == vm_id)
            {
                self.core_states[pcpu] = CoreState::Idle;
                self.pcpu_owners.write_owner(pcpu, 0, 0);
                self.core_last_metered_tsc[pcpu] = 0;
                pcpu += 1;
                continue;
            }
            if self.managed_pcpus[pcpu]
                && let CoreState::Assigned { vm_id, vcpu } = self.core_states[pcpu]
                && let Some(vm) = Self::find_vm(vms, vm_count, vm_id)
            {
                let slot = &vm.shared_page().vcpus[vcpu as usize];
                // The VMM bumps `yield_count` before publishing `PARKED`, so a parked vCPU
                // whose count moved has given the core back since it was assigned.
                if slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                    && slot.revoke_requested.load(Ordering::Acquire) == 0
                    && slot.yield_count.load(Ordering::Acquire) != self.assigned_yield_count[pcpu]
                {
                    let last_tsc = self.core_last_metered_tsc[pcpu];
                    if last_tsc > 0 && now_tsc > last_tsc {
                        let tsc_khz = self.tsc_khz_of(vm);
                        let usec = ((now_tsc - last_tsc) * 1000) / tsc_khz;
                        if usec > 0 {
                            vm.add_allocated_core_usec(usec);
                        }
                    }
                    self.core_states[pcpu] = CoreState::Idle;
                    self.pcpu_owners.write_owner(pcpu, 0, 0);
                    self.core_last_metered_tsc[pcpu] = 0;
                    slot.assigned_pcpu.store(PCPU_UNASSIGNED, Ordering::Release);
                }
            }
            pcpu += 1;
        }
    }

    fn meter_core_residency(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if self.managed_pcpus[pcpu] {
                let vm_id = match self.core_states[pcpu] {
                    CoreState::Assigned { vm_id, .. } => Some(vm_id),
                    CoreState::Revoking { donor_vm_id, .. } => Some(donor_vm_id),
                    CoreState::Idle => None,
                };
                if let Some(vid) = vm_id {
                    let last_tsc = self.core_last_metered_tsc[pcpu];
                    if last_tsc > 0 && now_tsc > last_tsc {
                        if let Some(vm) = Self::find_vm(vms, vm_count, vid) {
                            let tsc_khz = self.tsc_khz_of(vm);
                            let elapsed_cycles = now_tsc - last_tsc;
                            let usec = (elapsed_cycles * 1000) / tsc_khz;
                            if usec > 0 {
                                vm.add_allocated_core_usec(usec);
                                let remainder_cycles = (usec * tsc_khz) / 1000;
                                self.core_last_metered_tsc[pcpu] = last_tsc + remainder_cycles;
                            }
                        } else {
                            self.core_last_metered_tsc[pcpu] = now_tsc;
                        }
                    } else if last_tsc == 0 {
                        self.core_last_metered_tsc[pcpu] = now_tsc;
                    }
                } else {
                    self.core_last_metered_tsc[pcpu] = 0;
                }
            }
            pcpu += 1;
        }
    }

    fn evaluate_demand_phase1(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut i = 0usize;
        while i < vm_count {
            let vm = &vms[i];
            let vm_id = vm.vm_id();
            let (assigned_cores, incoming_cores) = self.count_vm_cores(vm_id);
            let committed_cores = assigned_cores + incoming_cores;

            let page = vm.shared_page();
            let runnable = page.telemetry.runnable_threads.load(Ordering::Acquire);
            let raw_head_wait = page.telemetry.head_wait_tsc.load(Ordering::Relaxed);
            let head_wait = if raw_head_wait > 0 {
                now_tsc.saturating_sub(raw_head_wait)
            } else {
                0
            };

            let mut zero_core_wakeup = false;
            if committed_cores == 0 {
                let next_timer_tsc = page.telemetry.next_timer_tsc.load(Ordering::Relaxed);
                let timer_expired = next_timer_tsc != 0 && now_tsc >= next_timer_tsc;
                let rx_cons = page.telemetry.rx_ring_cons_idx.load(Ordering::Relaxed);
                let rx_pending = vm.host_rx_doorbell() > rx_cons;
                if timer_expired || rx_pending {
                    zero_core_wakeup = true;
                }
            }

            let desired_cores = if zero_core_wakeup {
                runnable.max(1).clamp(vm.min_cores().max(1), vm.max_cores())
            } else if runnable > 0 {
                runnable.clamp(vm.min_cores(), vm.max_cores())
            } else {
                0
            };

            let urgency = if zero_core_wakeup {
                u64::MAX
            } else if desired_cores > committed_cores {
                let base_wait = head_wait.max(1);
                let weighted = base_wait.saturating_mul(u64::from(vm.priority_weight()));
                if committed_cores < vm.min_cores() {
                    weighted.saturating_add(1u64 << 60)
                } else {
                    weighted
                }
            } else {
                head_wait.saturating_mul(u64::from(vm.priority_weight()))
            };

            self.eval_buf[i] = VmDemandEval {
                vm_idx: i,
                vm_id,
                min_cores: vm.min_cores(),
                assigned_cores,
                incoming_cores,
                desired_cores,
                urgency,
                zero_core_wakeup,
            };
            i += 1;
        }
    }

    fn progress_revocations_phase2(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if !self.managed_pcpus[pcpu] {
                pcpu += 1;
                continue;
            }

            if let CoreState::Revoking {
                donor_vm_id,
                donor_vcpu,
                recipient_vm_id,
                recipient_vcpu,
                deadline_tsc,
            } = self.core_states[pcpu]
            {
                let donor_vm = Self::find_vm(vms, vm_count, donor_vm_id);
                let recipient_vm = Self::find_vm(vms, vm_count, recipient_vm_id);

                if let (Some(donor), Some(recipient)) = (donor_vm, recipient_vm) {
                    let donor_slot = &donor.shared_page().vcpus[donor_vcpu as usize];
                    let tsc_khz = self.tsc_khz_of(donor);
                    let signalled_tsc = self.revoke_signalled_tsc[pcpu];
                    // Only the VMM writes `PARKED`, once the donor thread has left `KVM_RUN`
                    // (`spec/06-guest-kick-path.md` §3).
                    let parked = donor_slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED;

                    if !parked && now_tsc >= deadline_tsc {
                        // Interrupt the donor's `KVM_RUN` so the VMM force-parks it, and repeat
                        // in case a signal lands while the thread is outside `KVM_RUN`.
                        if signalled_tsc == 0 {
                            donor.record_forced_preempt();
                        }
                        if signalled_tsc == 0
                            || now_tsc
                                >= signalled_tsc
                                    .saturating_add(Self::us_to_cycles(REVOKE_RESIGNAL_US, tsc_khz))
                        {
                            let _ = donor.signal_vcpu(donor_vcpu as usize);
                            self.revoke_signalled_tsc[pcpu] = now_tsc;
                        }
                    }

                    let hand_over = if parked {
                        if signalled_tsc == 0 {
                            donor.record_cooperative_yield();
                        }
                        let requested_tsc = self.revoke_requested_tsc[pcpu];
                        if requested_tsc != 0 && now_tsc > requested_tsc {
                            donor.record_park_latency_us(
                                (now_tsc - requested_tsc).saturating_mul(1000) / tsc_khz,
                            );
                        }
                        true
                    } else if now_tsc
                        >= deadline_tsc
                            .saturating_add(Self::us_to_cycles(FORCE_HANDOFF_US, tsc_khz))
                    {
                        // The donor still hasn't parked; give the core away regardless and let
                        // `scx_pylon` preempt the donor thread. It parks at its next exit.
                        donor.record_handoff_without_park();
                        true
                    } else {
                        false
                    };

                    if hand_over {
                        donor_slot
                            .assigned_pcpu
                            .store(PCPU_UNASSIGNED, Ordering::Release);
                        self.assign_pcpu(pcpu, recipient, recipient_vcpu, now_tsc);

                        if let Some(eval_idx) = self.find_eval_idx(vm_count, recipient_vm_id) {
                            self.eval_buf[eval_idx].incoming_cores =
                                self.eval_buf[eval_idx].incoming_cores.saturating_sub(1);
                            self.eval_buf[eval_idx].assigned_cores += 1;
                        }
                    }
                } else {
                    // One side is gone: drop the revocation and release the recipient's
                    // reservation so it can be matched again.
                    if let Some(recipient) = recipient_vm {
                        recipient.shared_page().vcpus[recipient_vcpu as usize]
                            .assigned_pcpu
                            .store(PCPU_UNASSIGNED, Ordering::Release);
                    }
                    self.core_states[pcpu] = CoreState::Idle;
                    self.pcpu_owners.write_owner(pcpu, 0, 0);
                    self.core_last_metered_tsc[pcpu] = 0;
                    self.revoke_requested_tsc[pcpu] = 0;
                    self.revoke_signalled_tsc[pcpu] = 0;
                }
            }
            pcpu += 1;
        }
    }

    fn match_hungry_vms_phase3(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        // Insertion-sort `eval_buf[..vm_count]` in descending order of urgency (in-place, 0 allocs).
        let mut i = 1usize;
        while i < vm_count {
            let key = self.eval_buf[i];
            let mut j = i;
            while j > 0 && self.eval_buf[j - 1].urgency < key.urgency {
                self.eval_buf[j] = self.eval_buf[j - 1];
                j -= 1;
            }
            self.eval_buf[j] = key;
            i += 1;
        }

        let mut order_idx = 0usize;
        while order_idx < vm_count {
            while self.eval_buf[order_idx].needed_cores() > 0 {
                let recipient_eval = self.eval_buf[order_idx];
                let recipient_vm = &vms[recipient_eval.vm_idx];
                let Some(recipient_vcpu) = Self::select_parked_vcpu(recipient_vm) else {
                    break;
                };

                // 1. Fast Path: Check for an Idle physical core.
                if let Some(idle_pcpu) = self.find_idle_pcpu() {
                    self.assign_pcpu(idle_pcpu, recipient_vm, recipient_vcpu, now_tsc);
                    self.eval_buf[order_idx].assigned_cores += 1;
                    continue;
                }

                // 2. Preemption Path: Select an over-allocated or lower-urgency donor core.
                let Some((donor_pcpu, donor_eval_idx, donor_vcpu)) =
                    self.select_donor_pcpu(vms, vm_count, recipient_eval)
                else {
                    break;
                };

                let donor_eval = self.eval_buf[donor_eval_idx];
                let donor_vm = &vms[donor_eval.vm_idx];
                let donor_page = donor_vm.shared_page();
                let deadline_tsc = now_tsc.saturating_add(Self::us_to_cycles(
                    REVOKE_GRACE_PERIOD_US,
                    self.tsc_khz_of(donor_vm),
                ));

                let donor_slot = &donor_page.vcpus[donor_vcpu as usize];
                donor_slot
                    .revoke_deadline_tsc
                    .store(deadline_tsc, Ordering::Relaxed);
                donor_slot.revoke_requested.store(1, Ordering::Release);
                let _ = donor_vm.kick_revoke_irqfd(donor_vcpu as usize);

                self.core_states[donor_pcpu] = CoreState::Revoking {
                    donor_vm_id: donor_eval.vm_id,
                    donor_vcpu,
                    recipient_vm_id: recipient_eval.vm_id,
                    recipient_vcpu,
                    deadline_tsc,
                };
                self.revoke_requested_tsc[donor_pcpu] = now_tsc;
                self.revoke_signalled_tsc[donor_pcpu] = 0;
                // Reserve the recipient vCPU so later matches don't pick it for another core.
                recipient_vm.shared_page().vcpus[recipient_vcpu as usize]
                    .assigned_pcpu
                    .store(donor_pcpu as u8, Ordering::Release);
                self.eval_buf[donor_eval_idx].assigned_cores = self.eval_buf[donor_eval_idx]
                    .assigned_cores
                    .saturating_sub(1);
                self.eval_buf[order_idx].incoming_cores += 1;
            }
            order_idx += 1;
        }
    }

    #[inline]
    fn count_vm_cores(&self, vm_id: u64) -> (u32, u32) {
        let mut assigned = 0u32;
        let mut incoming = 0u32;
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if self.managed_pcpus[pcpu] {
                match self.core_states[pcpu] {
                    CoreState::Idle => {}
                    CoreState::Assigned {
                        vm_id: owner_id, ..
                    } => {
                        if owner_id == vm_id {
                            assigned += 1;
                        }
                    }
                    CoreState::Revoking {
                        recipient_vm_id, ..
                    } => {
                        if recipient_vm_id == vm_id {
                            incoming += 1;
                        }
                    }
                }
            }
            pcpu += 1;
        }
        (assigned, incoming)
    }

    #[inline]
    fn find_idle_pcpu(&self) -> Option<usize> {
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if self.managed_pcpus[pcpu] && self.core_states[pcpu] == CoreState::Idle {
                return Some(pcpu);
            }
            pcpu += 1;
        }
        None
    }

    /// Selects the best donor `(pcpu, donor_eval_idx, donor_vcpu)` to preempt for `recipient`,
    /// preferring donor vCPUs with `in_critical_section == 0`.
    fn select_donor_pcpu(
        &self,
        vms: &[VmHandle],
        vm_count: usize,
        recipient: VmDemandEval,
    ) -> Option<(usize, usize, u8)> {
        let mut best: Option<(usize, usize, u8, bool, bool, u64)> = None;

        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if !self.managed_pcpus[pcpu] {
                pcpu += 1;
                continue;
            }
            if let CoreState::Assigned {
                vm_id: donor_vm_id,
                vcpu: donor_vcpu,
            } = self.core_states[pcpu]
                && donor_vm_id != recipient.vm_id
                && let Some(donor_eval_idx) = self.find_eval_idx(vm_count, donor_vm_id)
            {
                let donor_eval = self.eval_buf[donor_eval_idx];
                let over_desired = donor_eval.assigned_cores > donor_eval.desired_cores;
                let over_fair_share =
                    donor_eval.assigned_cores > recipient.committed_cores().saturating_add(1);
                let lower_urgency = donor_eval.urgency < recipient.urgency;
                let above_min = donor_eval.assigned_cores > donor_eval.min_cores;

                let eligible = (above_min && (over_desired || over_fair_share || lower_urgency))
                    || (recipient.zero_core_wakeup && lower_urgency);

                if eligible {
                    let donor_vm = &vms[donor_eval.vm_idx];
                    let in_cs = donor_vm.shared_page().vcpus[donor_vcpu as usize]
                        .in_critical_section
                        .load(Ordering::Acquire)
                        != 0;
                    let not_in_cs = !in_cs;

                    let replace = match best {
                        None => true,
                        Some((_, _, _, best_not_in_cs, best_over_desired, best_urgency)) => {
                            if not_in_cs != best_not_in_cs {
                                not_in_cs
                            } else if over_desired != best_over_desired {
                                over_desired
                            } else {
                                donor_eval.urgency < best_urgency
                            }
                        }
                    };

                    if replace {
                        best = Some((
                            pcpu,
                            donor_eval_idx,
                            donor_vcpu,
                            not_in_cs,
                            over_desired,
                            donor_eval.urgency,
                        ));
                    }
                }
            }
            pcpu += 1;
        }

        best.map(|(pcpu, eval_idx, vcpu, _, _, _)| (pcpu, eval_idx, vcpu))
    }

    #[inline]
    fn find_vm(vms: &[VmHandle], vm_count: usize, vm_id: u64) -> Option<&VmHandle> {
        let mut i = 0usize;
        while i < vm_count {
            if vms[i].vm_id() == vm_id {
                return Some(&vms[i]);
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn find_eval_idx(&self, vm_count: usize, vm_id: u64) -> Option<usize> {
        let mut i = 0usize;
        while i < vm_count {
            if self.eval_buf[i].vm_id == vm_id {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}
