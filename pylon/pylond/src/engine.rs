// Copyright © 2026 CMKL-APEX
//
// SPDX-License-Identifier: Apache-2.0
//

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use core::arch::asm;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{__cpuid_count, _mm_pause};
#[cfg(not(target_arch = "x86_64"))]
use core::hint::spin_loop;
use std::array::from_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use log::{info, warn};
use pylon_abi::clock::{architectural_counter_khz, read_counter};
use pylon_abi::{MAX_VCPUS, PCPU_UNASSIGNED, PylonHostVcpuSlot, VCPU_STATE_PARKED};

use crate::{
    CoreState, Error, MAX_PCPUS, MAX_VM_SLOTS, PcpuOwnerTable, VM_SLOT_NONE, VmHandle, VmSnapshot,
    VmVcpusTable, pin_current_thread_to_core,
};

/// Maximum number of concurrent VMs evaluated per stack-allocated sweep in Plane 2.
pub const MAX_TRACKED_VMS: usize = 64;

/// x86_64 TSC rate (3.0 GHz = 3,000,000 kHz) assumed when neither a VM's header nor a host
/// measurement gives one. Use [`default_counter_khz`], which prefers the architectural rate.
pub const DEFAULT_TSC_KHZ: u64 = 3_000_000;

/// Counter rate in kHz assumed when neither a VM's header nor a host measurement gives one: the
/// architectural rate where there is one (`CNTFRQ_EL0` on aarch64), else [`DEFAULT_TSC_KHZ`].
pub fn default_counter_khz() -> u64 {
    architectural_counter_khz().unwrap_or(DEFAULT_TSC_KHZ)
}

/// Default revocation grace period in microseconds (`spec/00-architecture.md` & `spec/02-pylond-arbiter.md`).
pub const REVOKE_GRACE_PERIOD_US: u64 = 8;

/// How long after a revocation's deadline `pylond` keeps waiting for the donor to park before it
/// hands the core over anyway (`spec/06-guest-kick-path.md` §4.5).
pub const FORCE_HANDOFF_US: u64 = 50;

/// Interval at which `pylond` first re-sends the kick signal to a donor that hasn't parked. A
/// signal that lands while the vCPU thread is outside `KVM_RUN` doesn't stop its next guest
/// entry. The interval doubles with each signal, up to `REVOKE_RESIGNAL_MAX_US`.
pub const REVOKE_RESIGNAL_US: u64 = 10;

/// Longest interval between kick signals to a donor that hasn't parked. A donor the VMM can't
/// park yet, because the guest has interrupts off (`spec/08-park-in-kvm.md` §3.4), needs time
/// in the guest to turn them back on, and each signal takes that from it: on a nested host, a
/// guest entry costs about as long as `REVOKE_RESIGNAL_US`.
pub const REVOKE_RESIGNAL_MAX_US: u64 = 1000;

/// How long a parked vCPU that KVM woke may stay runnable, sharing its VM's cores, before it
/// counts as demand for a core of its own (`spec/08-park-in-kvm.md` §4.2).
pub const WOKEN_DEMAND_US: u64 = 4_000;

/// Longest time a core given to a woken vCPU stays wanted while the guest agent hasn't reported
/// since (`spec/08-park-in-kvm.md` §4.2). The agent reports every 50 ms by default; a guest
/// without one gets the same bound.
pub const WOKEN_GRANT_HOLD_US: u64 = 50_000;

/// How often a VM woken from zero cores may take a core from a donor at its `min_cores`. The
/// wakeup's urgency comes from timers and telemetry the guest writes, so without a limit a
/// guest that parks and wakes again and again would keep taking cores below other VMs' floor.
pub const FLOOR_PREEMPT_INTERVAL_US: u64 = 10_000;

/// Delay between polling sweeps in nanoseconds (~1000 cycles on a 3 GHz core). The engine turns it
/// into counter ticks with the host rate, so it doesn't depend on the counter's frequency.
pub const SWEEP_INTERVAL_NS: u64 = 333;

/// Default CPU the polling engine pins itself to.
pub const DEFAULT_ENGINE_CPU: usize = 1;

/// `AT_HWCAP2` bit for `WFET`/`WFIT` (`FEAT_WFxT`), from the kernel's `asm/hwcap.h`.
#[cfg(target_arch = "aarch64")]
const HWCAP2_WFXT: libc::c_ulong = 1 << 31;

/// Longest SMT sibling ring followed when placing vCPUs.
const MAX_SMT: usize = 8;

/// Finds the host counter rate in kHz: the architectural rate where there is one (`CNTFRQ_EL0` on
/// aarch64), else measured against `CLOCK_MONOTONIC` over `window` (the TSC on x86_64). Returns
/// `None` where there is no counter.
pub fn calibrate_tsc_khz(window: Duration) -> Option<u64> {
    if let Some(khz) = architectural_counter_khz() {
        return Some(khz);
    }
    let start = Instant::now();
    let start_tsc = read_tsc();
    if start_tsc == 0 {
        return None;
    }
    thread::sleep(window);
    let cycles = read_tsc().saturating_sub(start_tsc);
    let nanos = u64::try_from(start.elapsed().as_nanos()).ok()?;
    if nanos == 0 || cycles == 0 {
        return None;
    }
    // cycles per nanosecond * 1e6 = kHz
    u64::try_from(u128::from(cycles) * 1_000_000 / u128::from(nanos)).ok()
}

/// Converts nanoseconds to counter ticks at `counter_khz`, at least 1.
fn ns_to_cycles(nanos: u64, counter_khz: u64) -> u64 {
    u64::try_from(u128::from(nanos) * u128::from(counter_khz) / 1_000_000)
        .unwrap_or(u64::MAX)
        .max(1)
}

/// Detects whether this CPU can wait until a counter value: `WAITPKG` (`tpause`, CPUID.07H:0H.ECX
/// bit 5) on x86_64, `FEAT_WFxT` (`WFET`, `HWCAP2_WFXT`) on aarch64.
#[inline]
pub fn detect_timed_wait() -> bool {
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
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `getauxval` only reads the process's auxiliary vector.
        (unsafe { libc::getauxval(libc::AT_HWCAP2) } & HWCAP2_WFXT) != 0
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// Reads the host counter: the TSC on x86_64, `CNTVCT_EL0` on aarch64 (`pylon_abi::clock`).
#[inline]
pub fn read_tsc() -> u64 {
    read_counter()
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

/// Waits until `CNTVCT_EL0` reaches `deadline` (or an event wakes the core earlier).
///
/// # Safety
///
/// The CPU must support `FEAT_WFxT`.
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn wfet(deadline: u64) {
    // SAFETY: The caller verified `FEAT_WFxT`. `wfet x9` (`0xd5031009`, which is `0xd5031000 | Rt`;
    // `.inst` because the assembler needs the `wfxt` feature for the mnemonic) is a hint that waits
    // in a low-power state and touches no memory, so it can return at any time. `x9` is named so
    // the encoding's register is known.
    unsafe {
        asm!(
            ".inst 0xd5031009",
            in("x9") deadline,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Pauses the polling core for `cycles` counter ticks: until `now_tsc + cycles` with `tpause` or
/// `WFET` when `has_timed_wait`, else with a spin-loop hint. It may return early.
#[inline]
pub fn pause_cycles(now_tsc: u64, cycles: u64, has_timed_wait: bool) {
    #[cfg(target_arch = "x86_64")]
    {
        if has_timed_wait {
            let target = now_tsc.wrapping_add(cycles);
            // SAFETY: `has_timed_wait` is only true when CPUID verified `WAITPKG` support.
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
    #[cfg(target_arch = "aarch64")]
    {
        if has_timed_wait {
            // SAFETY: `has_timed_wait` is only true when `HWCAP2_WFXT` reported `FEAT_WFxT`.
            unsafe {
                wfet(now_tsc.wrapping_add(cycles));
            }
        } else {
            spin_loop();
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = (now_tsc, cycles, has_timed_wait);
        spin_loop();
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

/// What the engine read from one VM's `woken` mask in a sweep.
#[derive(Debug, Clone, Copy, Default)]
struct WokenEval {
    /// How many vCPUs still `PARKED` with no core assigned have been woken for
    /// `WOKEN_DEMAND_US` or longer.
    long: u32,
    /// The longest current wake, in counter ticks (0 if none).
    longest_cycles: u64,
}

/// The engine's record of one VM slot's woken vCPUs.
#[derive(Debug, Clone, Copy)]
struct WokenTrack {
    /// The VM that held the slot when the record was made (0 if none).
    vm_id: u64,
    /// The mask seen in the last sweep.
    mask: u64,
    /// When each vCPU of `mask` was first seen woken.
    since_tsc: [u64; MAX_VCPUS],
    /// Woken vCPU time not yet added to the VM's counters, in counter ticks: while the VM owned
    /// a core, and while it owned none.
    pending_cycles: [u64; 2],
}

impl WokenTrack {
    const EMPTY: Self = Self {
        vm_id: 0,
        mask: 0,
        since_tsc: [0; MAX_VCPUS],
        pending_cycles: [0; 2],
    };
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

/// Plane 2: Real-Time Matrix Engine (pinned to one host CPU, `DEFAULT_ENGINE_CPU` unless set).
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
    /// TSC at which the donor was first signalled (0 if not yet). `FORCE_HANDOFF_US` counts from
    /// here, so a donor always has that long to park after a signal, even if the engine ran late.
    revoke_first_signal_tsc: [u64; MAX_PCPUS],
    /// The donor's `yield_count` when its revocation was requested. The donor has parked for the
    /// revocation only once it is `PARKED` with a higher count: a vCPU that was just assigned is
    /// still `PARKED` until its thread wakes.
    revoke_yield_count: [u64; MAX_PCPUS],
    /// TSC at which an assigned vCPU of a VM that parks inside KVM was last signalled to wake
    /// (0 if not, or once it left `PARKED`).
    wake_signalled_tsc: [u64; MAX_PCPUS],
    /// When the vCPU a core is assigned or being handed to was picked while woken (0 if it
    /// wasn't), and the guest's `guest_heartbeat` then. The core stays wanted until the agent
    /// reports again or `WOKEN_GRANT_HOLD_US` passes (`spec/08-park-in-kvm.md` §4.2). Read only
    /// while the core is `Assigned` or `Revoking`.
    woken_grant_tsc: [u64; MAX_PCPUS],
    woken_grant_heartbeat: [u64; MAX_PCPUS],
    /// When each vCPU (by VM index in the snapshot) that kept running after a handoff without park
    /// was last signalled (0 if not yet).
    orphan_signalled_tsc: [[u64; MAX_VCPUS]; MAX_TRACKED_VMS],
    /// Host TSC rate, used for every VM. The rate in a VM's header is never read: the guest can
    /// write the shared page, and every `*_tsc` field counts host ticks anyway.
    host_tsc_khz: u64,
    has_timed_wait: bool,
    engine_cpu: usize,
    /// SMT sibling ring: the next CPU on the same core, or the CPU itself.
    smt_next: [u16; MAX_PCPUS],
    /// Per-VM parked vCPU bitmasks shared with `scx_pylon` (`spec/08-park-in-kvm.md` §4.3).
    vm_vcpus: Arc<VmVcpusTable>,
    /// VM slots whose `parked` mask the engine last left non-zero.
    parked_slots: u64,
    /// Woken vCPUs by VM slot (`spec/08-park-in-kvm.md` §4.2).
    woken: Box<[WokenTrack; MAX_VM_SLOTS]>,
    /// When the previous sweep ran (0 before the first).
    last_poll_tsc: u64,
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
        let has_timed_wait = detect_timed_wait();

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
            revoke_first_signal_tsc: [0; MAX_PCPUS],
            revoke_yield_count: [0; MAX_PCPUS],
            wake_signalled_tsc: [0; MAX_PCPUS],
            woken_grant_tsc: [0; MAX_PCPUS],
            woken_grant_heartbeat: [0; MAX_PCPUS],
            orphan_signalled_tsc: [[0; MAX_VCPUS]; MAX_TRACKED_VMS],
            vm_vcpus: Arc::new(VmVcpusTable::new_in_memory()),
            parked_slots: 0,
            woken: Box::new([WokenTrack::EMPTY; MAX_VM_SLOTS]),
            last_poll_tsc: 0,
            host_tsc_khz: default_counter_khz(),
            has_timed_wait,
            engine_cpu: DEFAULT_ENGINE_CPU,
            smt_next: from_fn(|pcpu| pcpu as u16),
        }
    }

    /// Sets the host TSC rate used for every VM, such as a value from `calibrate_tsc_khz`.
    pub fn set_host_tsc_khz(&mut self, tsc_khz: u64) {
        if tsc_khz > 0 {
            self.host_tsc_khz = tsc_khz;
        }
    }

    /// Sets the SMT sibling ring (`scx_pylon::host_smt_ring`), so that VMs aren't given sibling
    /// threads of one core. Entries out of range are treated as CPUs without siblings.
    pub fn set_smt_ring(&mut self, ring: &[u32; MAX_PCPUS]) {
        for (pcpu, &next) in ring.iter().enumerate() {
            self.smt_next[pcpu] = if (next as usize) < MAX_PCPUS {
                next as u16
            } else {
                pcpu as u16
            };
        }
    }

    /// Returns the current `CoreState` of physical core `pcpu_id`.
    #[inline]
    pub fn core_state(&self, pcpu_id: usize) -> CoreState {
        self.core_states[pcpu_id]
    }

    /// Explicitly sets the `CoreState` of physical core `pcpu_id` (used for setup/testing).
    #[inline]
    /// Publishes parked vCPUs in `table`, the `scx_pylon` map, instead of heap memory.
    #[must_use]
    pub fn with_vm_vcpus(mut self, table: Arc<VmVcpusTable>) -> Self {
        self.vm_vcpus = table;
        self
    }

    pub fn set_core_state(&mut self, pcpu_id: usize, state: CoreState) {
        if pcpu_id < MAX_PCPUS {
            self.core_states[pcpu_id] = state;
            self.core_last_metered_tsc[pcpu_id] = 0;
            self.woken_grant_tsc[pcpu_id] = 0;
        }
    }

    /// Returns a reference to the underlying `PcpuOwnerTable`.
    #[inline]
    pub fn pcpu_owners(&self) -> &Arc<PcpuOwnerTable> {
        &self.pcpu_owners
    }

    /// Sets the host CPU the polling loop pins itself to in [`Self::run`].
    pub fn set_engine_cpu(&mut self, cpu: usize) {
        self.engine_cpu = cpu;
    }

    /// Pins the calling thread to the engine CPU and runs the real-time polling loop until `stop`
    /// is set.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), Error> {
        if let Err(err) = pin_current_thread_to_core(self.engine_cpu) {
            warn!("Plane 2 could not pin to CPU {}: {err}", self.engine_cpu);
        }
        let sweep_cycles = ns_to_cycles(SWEEP_INTERVAL_NS, self.host_tsc_khz);
        info!(
            "Plane 2 (Matrix Engine) started (cpu={}, timed_wait={}, sweep={sweep_cycles} ticks)",
            self.engine_cpu, self.has_timed_wait
        );

        while !stop.load(Ordering::Relaxed) {
            let now_tsc = read_tsc();
            self.poll_once(now_tsc);
            pause_cycles(now_tsc, sweep_cycles, self.has_timed_wait);
        }
        Ok(())
    }

    /// Selects a parked vCPU (`VCPU_STATE_PARKED`) for `vm`, checking `stranded_work_bitmap`
    /// first via `trailing_zeros()` (`tzcnt`), then a vCPU whose timer is due at `now_tsc`
    /// (`park_timer_deadline_tsc`), before scanning remaining vCPU slots.
    #[inline]
    pub fn select_parked_vcpu(vm: &VmHandle, now_tsc: u64) -> Option<u8> {
        if let Some(vcpu) = Self::take_stranded_vcpu(vm) {
            return Some(vcpu);
        }

        let host_page = vm.host_page();
        let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
        let mut first_parked = None;
        let mut idx = 0usize;
        while idx < num_vcpus {
            if Self::is_unassigned_parked(&host_page.vcpus[idx]) {
                if Self::park_timer_due(&host_page.vcpus[idx], now_tsc) {
                    return Some(idx as u8);
                }
                first_parked = first_parked.or(Some(idx as u8));
            }
            idx += 1;
        }
        first_parked
    }

    /// Takes the lowest free parked vCPU of `vm` with guest tasks queued behind it
    /// (`stranded_work_bitmap`) and clears its bit. The guest counts those tasks as
    /// runnable, and they can't run until that vCPU gets a core.
    #[inline]
    fn take_stranded_vcpu(vm: &VmHandle) -> Option<u8> {
        let page = vm.shared_page();
        let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
        let mut bitmap = page.telemetry.stranded_work_bitmap.load(Ordering::Acquire);

        while bitmap != 0 {
            let vcpu_idx = bitmap.trailing_zeros() as usize;
            let bit = 1u64 << vcpu_idx;
            if vcpu_idx < num_vcpus && Self::is_unassigned_parked(&vm.host_page().vcpus[vcpu_idx]) {
                page.telemetry
                    .stranded_work_bitmap
                    .fetch_and(!bit, Ordering::AcqRel);
                return Some(vcpu_idx as u8);
            }
            bitmap &= !bit;
        }
        None
    }

    /// Whether the guest timer that was armed when `slot`'s vCPU parked is due. A parked
    /// vCPU's timer interrupt can't fire until the vCPU gets a core again.
    #[inline]
    fn park_timer_due(slot: &PylonHostVcpuSlot, now_tsc: u64) -> bool {
        let deadline = slot.park_timer_deadline_tsc.load(Ordering::Relaxed);
        deadline != 0 && now_tsc >= deadline
    }

    /// Whether any of `vm`'s unassigned parked vCPUs has a timer due at `now_tsc`.
    #[inline]
    fn any_park_timer_due(vm: &VmHandle, now_tsc: u64) -> bool {
        let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
        vm.host_page().vcpus[..num_vcpus]
            .iter()
            .any(|host| Self::is_unassigned_parked(host) && Self::park_timer_due(host, now_tsc))
    }

    /// A vCPU is free to receive a core when it is parked and no core is on its way to it.
    /// `pylond` doesn't write `state` (`spec/06-guest-kick-path.md` §3), so a vCPU that was just
    /// assigned stays `PARKED` until its thread wakes; `assigned_pcpu` tells the two apart.
    #[inline]
    fn is_unassigned_parked(slot: &PylonHostVcpuSlot) -> bool {
        slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
            && slot.assigned_pcpu.load(Ordering::Acquire) == PCPU_UNASSIGNED
    }

    /// Whether to re-send a kick signal first sent (or due) at `first_tsc` and last sent at
    /// `last_tsc`. The interval is the time since `first_tsc`, at least `REVOKE_RESIGNAL_US` and
    /// at most `REVOKE_RESIGNAL_MAX_US`, so it doubles with each signal.
    #[inline]
    fn resignal_due(first_tsc: u64, last_tsc: u64, now_tsc: u64, tsc_khz: u64) -> bool {
        let interval = last_tsc.saturating_sub(first_tsc).clamp(
            Self::us_to_cycles(REVOKE_RESIGNAL_US, tsc_khz),
            Self::us_to_cycles(REVOKE_RESIGNAL_MAX_US, tsc_khz),
        );
        now_tsc >= last_tsc.saturating_add(interval)
    }

    /// Converts microseconds to TSC cycles at `tsc_khz`.
    #[inline]
    fn us_to_cycles(usec: u64, tsc_khz: u64) -> u64 {
        tsc_khz.saturating_mul(usec) / 1000
    }

    /// Gives `pcpu` to `vcpu` of `vm`: publishes the owner, records the assignment, and wakes
    /// the vCPU thread. The VMM moves the vCPU from `PARKED` to `RUNNING` when it wakes.
    fn assign_pcpu(&mut self, pcpu: usize, vm: &VmHandle, vcpu: u8, now_tsc: u64) {
        let host = &vm.host_page().vcpus[vcpu as usize];
        let tid = host.host_tid.load(Ordering::Acquire);
        // `scx_pylon` finds the CPUs a VM owns by the slot in the flags (spec 08 §4.3).
        let vm_slot = vm.vm_slot();
        let flags = if vm_slot == VM_SLOT_NONE {
            0
        } else {
            (u32::from(vm_slot) + 1) << scx_pylon::PCPU_OWNER_SLOT_SHIFT
        };
        self.pcpu_owners.write_owner(pcpu, tid, flags);
        self.assigned_yield_count[pcpu] = host.yield_count.load(Ordering::Acquire);
        vm.set_assigned_pcpu(vcpu as usize, pcpu as u8);
        let _ = vm.wake_vcpu(vcpu as usize);
        // A vCPU parked inside KVM only sees the wake `eventfd` after an exit
        // (`spec/08-park-in-kvm.md` §3.3).
        self.wake_signalled_tsc[pcpu] = 0;
        if vm.wake_by_signal() && vm.signal_vcpu(vcpu as usize).unwrap_or(false) {
            self.wake_signalled_tsc[pcpu] = now_tsc;
        }

        self.core_states[pcpu] = CoreState::Assigned {
            vm_id: vm.vm_id(),
            vcpu,
        };
        self.core_last_metered_tsc[pcpu] = now_tsc;
        self.revoke_requested_tsc[pcpu] = 0;
        self.revoke_signalled_tsc[pcpu] = 0;
        self.revoke_first_signal_tsc[pcpu] = 0;
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
        self.signal_orphaned_donors(vms, vm_count, now_tsc);
        self.signal_unwoken_recipients(vms, vm_count, now_tsc);

        // =====================================================================
        // Phase 3: Match Hungry VMs with Idle or Donor Cores
        // =====================================================================
        self.match_hungry_vms_phase3(vms, vm_count, now_tsc);

        // Last, so the cores assigned in this sweep are already out of the masks.
        self.publish_parked_vcpus(vms, vm_count);
        self.last_poll_tsc = now_tsc;
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
                let slot = &vm.host_page().vcpus[vcpu as usize];
                // The VMM bumps `yield_count` before publishing `PARKED`, so a parked vCPU
                // whose count moved has given the core back since it was assigned.
                if slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                    && slot.revoke_requested.load(Ordering::Acquire) == 0
                    && slot.yield_count.load(Ordering::Acquire) != self.assigned_yield_count[pcpu]
                {
                    let last_tsc = self.core_last_metered_tsc[pcpu];
                    if last_tsc > 0 && now_tsc > last_tsc {
                        let tsc_khz = self.host_tsc_khz;
                        let usec = ((now_tsc - last_tsc) * 1000) / tsc_khz;
                        if usec > 0 {
                            vm.add_allocated_core_usec(usec);
                        }
                    }
                    self.core_states[pcpu] = CoreState::Idle;
                    self.pcpu_owners.write_owner(pcpu, 0, 0);
                    self.core_last_metered_tsc[pcpu] = 0;
                    vm.set_assigned_pcpu(vcpu as usize, PCPU_UNASSIGNED);
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
                            let tsc_khz = self.host_tsc_khz;
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
            // The guest writes these on its own TSC; see `PylonSharedPage::guest_to_host_tsc`.
            let raw_head_wait =
                page.guest_to_host_tsc(page.telemetry.head_wait_tsc.load(Ordering::Relaxed));
            let head_wait = if raw_head_wait > 0 {
                now_tsc.saturating_sub(raw_head_wait)
            } else {
                0
            };

            let woken = self.track_woken_vcpus(vm, committed_cores > 0, now_tsc);
            let woken_grants = self.count_woken_grants(vm, now_tsc);
            // A long wake waits like the head of the guest's run queue.
            let head_wait = head_wait.max(woken.longest_cycles);

            let mut zero_core_wakeup = false;
            if committed_cores == 0 {
                let next_timer_tsc =
                    page.guest_to_host_tsc(page.telemetry.next_timer_tsc.load(Ordering::Relaxed));
                // KVM fires the timers of a vCPU parked inside it, which then runs without a
                // core (`spec/08-park-in-kvm.md` §3.2). A due timer needs no core, and the
                // deadline the VMM recorded at the park stays due until the vCPU gets one: the
                // guest re-arms its timer without the VMM seeing it.
                let timer_expired = !vm.wake_by_signal()
                    && ((next_timer_tsc != 0 && now_tsc >= next_timer_tsc)
                        || Self::any_park_timer_due(vm, now_tsc));
                let rx_cons = page.telemetry.rx_ring_cons_idx.load(Ordering::Relaxed);
                let rx_pending = vm.host_rx_doorbell() > rx_cons;
                // With no core, a woken vCPU runs on CPUs other VMs may own. A short wake (a
                // tick or an IPI, then a halt) costs them less than a forced preemption would,
                // so only a long one wakes the VM (spec 08 §4.2).
                if timer_expired || rx_pending || woken.long > 0 {
                    zero_core_wakeup = true;
                }
            }

            let desired_cores = if zero_core_wakeup {
                runnable.max(1).clamp(vm.min_cores().max(1), vm.max_cores())
            } else if runnable > 0 {
                runnable.clamp(vm.min_cores(), vm.max_cores())
            } else {
                // `min_cores` is a floor under demand, not a reservation: an idle VM wants no
                // core, so it can scale to zero.
                0
            };
            // Each long wake asks for one more core: `runnable_threads` may not show its work.
            let desired_cores = if woken.long > 0 {
                desired_cores.max((committed_cores + woken.long).min(vm.max_cores()))
            } else {
                desired_cores
            };
            // A core given to a woken vCPU stays wanted until the guest agent reports again,
            // or for `WOKEN_GRANT_HOLD_US`. Once it has the core, the vCPU no longer counts as
            // woken, and `runnable_threads` doesn't show its work before that report: without
            // this, the VM would want no core and give it back in the next sweep, before the
            // vCPU ran on it.
            let desired_cores = desired_cores.max(woken_grants);

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
                    let donor_slot = &donor.host_page().vcpus[donor_vcpu as usize];
                    let tsc_khz = self.host_tsc_khz;
                    let signalled_tsc = self.revoke_signalled_tsc[pcpu];
                    let first_signal_tsc = self.revoke_first_signal_tsc[pcpu];
                    // Only the VMM writes `PARKED`, once the donor thread has left `KVM_RUN`
                    // (`spec/06-guest-kick-path.md` §3), and it bumps `yield_count` first. A
                    // `PARKED` donor with the old count hasn't run since it was assigned.
                    let parked = donor_slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                        && donor_slot.yield_count.load(Ordering::Acquire)
                            != self.revoke_yield_count[pcpu];

                    if !parked && now_tsc >= deadline_tsc {
                        // Interrupt the donor's `KVM_RUN` so the VMM force-parks it, and repeat
                        // in case a signal lands while the thread is outside `KVM_RUN`.
                        if signalled_tsc == 0 {
                            donor.record_forced_preempt();
                            self.revoke_first_signal_tsc[pcpu] = now_tsc;
                        }
                        if signalled_tsc == 0
                            || Self::resignal_due(first_signal_tsc, signalled_tsc, now_tsc, tsc_khz)
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
                    } else if first_signal_tsc != 0
                        && now_tsc
                            >= first_signal_tsc
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
                        donor.set_assigned_pcpu(donor_vcpu as usize, PCPU_UNASSIGNED);
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
                        recipient.set_assigned_pcpu(recipient_vcpu as usize, PCPU_UNASSIGNED);
                    }
                    self.core_states[pcpu] = CoreState::Idle;
                    self.pcpu_owners.write_owner(pcpu, 0, 0);
                    self.core_last_metered_tsc[pcpu] = 0;
                    self.revoke_requested_tsc[pcpu] = 0;
                    self.revoke_signalled_tsc[pcpu] = 0;
                    self.revoke_first_signal_tsc[pcpu] = 0;
                }
            }
            pcpu += 1;
        }
    }

    /// Keeps signalling donors whose core was handed over before they parked (a handoff
    /// without park): such a vCPU still has `revoke_requested` set and its deadline passed, so
    /// the VMM parks it at its next exit, but a guest that doesn't exit to userspace on its own
    /// would otherwise keep running without a core. The interval grows from the deadline on,
    /// as in `resignal_due`: the core already belongs to the recipient.
    fn signal_orphaned_donors(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut i = 0usize;
        while i < vm_count {
            let vm = &vms[i];
            let tsc_khz = self.host_tsc_khz;
            let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
            let mut vcpu = 0usize;
            while vcpu < num_vcpus {
                let slot = &vm.host_page().vcpus[vcpu];
                let deadline = slot.revoke_deadline_tsc.load(Ordering::Relaxed);
                // A donor still being revoked keeps its `assigned_pcpu` until the handover, and
                // boot parks (`park_on_boot`) are the control plane's to signal.
                let orphaned = slot.revoke_requested.load(Ordering::Acquire) == 1
                    && slot.assigned_pcpu.load(Ordering::Acquire) == PCPU_UNASSIGNED
                    && slot.park_on_boot.load(Ordering::Acquire) == 0
                    && slot.state.load(Ordering::Acquire) != VCPU_STATE_PARKED
                    && deadline != 0
                    && now_tsc >= deadline;
                let last = &mut self.orphan_signalled_tsc[i][vcpu];
                if !orphaned {
                    *last = 0;
                } else if *last == 0 || Self::resignal_due(deadline, *last, now_tsc, tsc_khz) {
                    let _ = vm.signal_vcpu(vcpu);
                    *last = now_tsc;
                }
                vcpu += 1;
            }
            i += 1;
        }
    }

    /// Keeps signalling assigned vCPUs of VMs that park inside KVM while they are still
    /// `PARKED` since the assignment. A signal that lands just before the vCPU thread re-enters
    /// `KVM_RUN` interrupts nothing, and the thread would sleep in KVM with a core assigned.
    fn signal_unwoken_recipients(&mut self, vms: &[VmHandle], vm_count: usize, now_tsc: u64) {
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            let mut unwoken = false;
            if self.managed_pcpus[pcpu]
                && let CoreState::Assigned { vm_id, vcpu } = self.core_states[pcpu]
                && let Some(vm) = Self::find_vm(vms, vm_count, vm_id)
                && vm.wake_by_signal()
            {
                let slot = &vm.host_page().vcpus[vcpu as usize];
                unwoken = slot.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                    && slot.yield_count.load(Ordering::Acquire) == self.assigned_yield_count[pcpu];
                let last = self.wake_signalled_tsc[pcpu];
                if unwoken
                    && now_tsc
                        >= last.saturating_add(Self::us_to_cycles(
                            REVOKE_RESIGNAL_US,
                            self.host_tsc_khz,
                        ))
                    && vm.signal_vcpu(vcpu as usize).unwrap_or(false)
                {
                    self.wake_signalled_tsc[pcpu] = now_tsc;
                }
            }
            if !unwoken {
                self.wake_signalled_tsc[pcpu] = 0;
            }
            pcpu += 1;
        }
    }

    /// Reads `vm`'s `woken` mask from `scx_pylon`, keeping vCPUs that are still `PARKED` with no
    /// core assigned, records when each was first seen, and adds the woken time since the last
    /// sweep to the VM's counters, as shared time if the VM `owned` a core
    /// (`spec/08-park-in-kvm.md` §4.2).
    fn track_woken_vcpus(&mut self, vm: &VmHandle, owned: bool, now_tsc: u64) -> WokenEval {
        let slot = vm.vm_slot();
        let Some(entry) = self.vm_vcpus.entry(slot) else {
            return WokenEval::default();
        };
        if !vm.wake_by_signal() {
            return WokenEval::default();
        }
        let page = vm.host_page();
        let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
        let mut mask = entry.woken.load(Ordering::Acquire);
        if num_vcpus < MAX_VCPUS {
            mask &= (1u64 << num_vcpus) - 1;
        }
        let mut bits = mask;
        while bits != 0 {
            let vcpu = bits.trailing_zeros() as usize;
            if !Self::is_unassigned_parked(&page.vcpus[vcpu]) {
                mask &= !(1u64 << vcpu);
            }
            bits &= bits - 1;
        }

        let tsc_khz = self.host_tsc_khz;
        let demand_cycles = Self::us_to_cycles(WOKEN_DEMAND_US, tsc_khz);
        let elapsed = if self.last_poll_tsc != 0 {
            now_tsc.saturating_sub(self.last_poll_tsc)
        } else {
            0
        };
        let track = &mut self.woken[slot as usize];
        if track.vm_id != vm.vm_id() {
            *track = WokenTrack::EMPTY;
            track.vm_id = vm.vm_id();
        }

        // Time since the last sweep, for the vCPUs that were woken then and still are.
        let pending = &mut track.pending_cycles[usize::from(!owned)];
        *pending = pending
            .saturating_add(elapsed.saturating_mul(u64::from((track.mask & mask).count_ones())));
        let usec = pending.saturating_mul(1000) / tsc_khz;
        if usec > 0 {
            vm.add_woken_usec(owned, usec);
            *pending -= usec * tsc_khz / 1000;
        }

        let mut eval = WokenEval::default();
        let mut bits = mask;
        while bits != 0 {
            let vcpu = bits.trailing_zeros() as usize;
            if track.mask & (1u64 << vcpu) == 0 {
                track.since_tsc[vcpu] = now_tsc;
            }
            let woken_for = now_tsc.saturating_sub(track.since_tsc[vcpu]);
            if woken_for >= demand_cycles {
                eval.long += 1;
            }
            eval.longest_cycles = eval.longest_cycles.max(woken_for);
            bits &= bits - 1;
        }
        track.mask = mask;
        eval
    }

    /// The vCPU of `vm` that has been woken longest and is still `PARKED` with no core assigned.
    /// It is already running, so a core for it needs no wake-up.
    fn longest_woken_vcpu(&self, vm: &VmHandle) -> Option<u8> {
        let slot = vm.vm_slot() as usize;
        let track = self.woken.get(slot)?;
        if track.vm_id != vm.vm_id() {
            return None;
        }
        let page = vm.host_page();
        let mut best: Option<(u64, u8)> = None;
        let mut bits = track.mask;
        while bits != 0 {
            let vcpu = bits.trailing_zeros() as usize;
            let since = track.since_tsc[vcpu];
            if Self::is_unassigned_parked(&page.vcpus[vcpu])
                && best.is_none_or(|(best_since, _)| since < best_since)
            {
                best = Some((since, vcpu as u8));
            }
            bits &= bits - 1;
        }
        best.map(|(_, vcpu)| vcpu)
    }

    /// Tells `scx_pylon` which vCPUs of VMs that park inside KVM are `PARKED` with no core
    /// assigned, so it keeps them on their own VM's cores when KVM wakes them
    /// (`spec/08-park-in-kvm.md` §4.3). Masks of VMs that left are cleared.
    fn publish_parked_vcpus(&mut self, vms: &[VmHandle], vm_count: usize) {
        let mut seen = 0u64;
        let mut i = 0usize;
        while i < vm_count {
            let vm = &vms[i];
            let slot = vm.vm_slot();
            if let Some(entry) = self.vm_vcpus.entry(slot) {
                let mut mask = 0u64;
                if vm.wake_by_signal() {
                    let num_vcpus = (vm.num_vcpus() as usize).min(MAX_VCPUS);
                    for (vcpu, v) in vm.host_page().vcpus[..num_vcpus].iter().enumerate() {
                        if v.state.load(Ordering::Acquire) == VCPU_STATE_PARKED
                            && v.assigned_pcpu.load(Ordering::Acquire) == PCPU_UNASSIGNED
                        {
                            mask |= 1 << vcpu;
                        }
                    }
                }
                if entry.parked.load(Ordering::Relaxed) != mask {
                    entry.parked.store(mask, Ordering::Release);
                }
                if mask != 0 {
                    seen |= 1 << slot;
                }
            }
            i += 1;
        }
        let mut stale = self.parked_slots & !seen;
        while stale != 0 {
            let slot = stale.trailing_zeros() as u16;
            if let Some(entry) = self.vm_vcpus.entry(slot) {
                entry.parked.store(0, Ordering::Release);
            }
            stale &= stale - 1;
        }
        self.parked_slots = seen;
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
                // Stranded work first: those tasks wait until their vCPU runs, while a woken
                // vCPU already runs, on cores its VM owns or on others.
                let stranded = Self::take_stranded_vcpu(recipient_vm);
                let woken_vcpu = if stranded.is_none() {
                    self.longest_woken_vcpu(recipient_vm)
                } else {
                    None
                };
                let woken_pick = woken_vcpu.is_some();
                let Some(recipient_vcpu) = stranded
                    .or(woken_vcpu)
                    .or_else(|| Self::select_parked_vcpu(recipient_vm, now_tsc))
                else {
                    break;
                };

                // 1. Fast Path: Check for an Idle physical core.
                if let Some(idle_pcpu) = self.find_idle_pcpu(recipient_eval.vm_id) {
                    self.assign_pcpu(idle_pcpu, recipient_vm, recipient_vcpu, now_tsc);
                    self.record_woken_grant(idle_pcpu, recipient_vm, woken_pick, now_tsc);
                    self.eval_buf[order_idx].assigned_cores += 1;
                    continue;
                }

                // 2. Preemption Path: Select an over-allocated or lower-urgency donor core.
                let below_floor_allowed = recipient_eval.zero_core_wakeup
                    && Self::floor_preempt_ready(recipient_vm, now_tsc, self.host_tsc_khz);
                let Some((donor_pcpu, donor_eval_idx, donor_vcpu, at_floor)) =
                    self.select_donor_pcpu(vms, vm_count, recipient_eval, below_floor_allowed)
                else {
                    break;
                };
                if at_floor {
                    recipient_vm.set_floor_preempt_tsc(now_tsc);
                }

                let donor_eval = self.eval_buf[donor_eval_idx];
                let donor_vm = &vms[donor_eval.vm_idx];
                let deadline_tsc = now_tsc.saturating_add(Self::us_to_cycles(
                    REVOKE_GRACE_PERIOD_US,
                    self.host_tsc_khz,
                ));

                self.revoke_yield_count[donor_pcpu] = donor_vm.host_page().vcpus
                    [donor_vcpu as usize]
                    .yield_count
                    .load(Ordering::Acquire);
                donor_vm.request_park(donor_vcpu as usize, deadline_tsc);
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
                self.revoke_first_signal_tsc[donor_pcpu] = 0;
                self.record_woken_grant(donor_pcpu, recipient_vm, woken_pick, now_tsc);
                // Reserve the recipient vCPU so later matches don't pick it for another core.
                recipient_vm.set_assigned_pcpu(recipient_vcpu as usize, donor_pcpu as u8);
                self.eval_buf[donor_eval_idx].assigned_cores = self.eval_buf[donor_eval_idx]
                    .assigned_cores
                    .saturating_sub(1);
                self.eval_buf[order_idx].incoming_cores += 1;
            }
            order_idx += 1;
        }
    }

    /// Records whether `pcpu` goes to a vCPU of `vm` that was woken when it was picked.
    fn record_woken_grant(&mut self, pcpu: usize, vm: &VmHandle, woken: bool, now_tsc: u64) {
        self.woken_grant_tsc[pcpu] = if woken { now_tsc.max(1) } else { 0 };
        self.woken_grant_heartbeat[pcpu] = vm
            .shared_page()
            .telemetry
            .guest_heartbeat
            .load(Ordering::Acquire);
    }

    /// How many cores `vm` has or is getting for a vCPU that was woken when it was picked, while
    /// the guest agent hasn't reported since and `WOKEN_GRANT_HOLD_US` hasn't passed.
    fn count_woken_grants(&self, vm: &VmHandle, now_tsc: u64) -> u32 {
        let vm_id = vm.vm_id();
        let heartbeat = vm
            .shared_page()
            .telemetry
            .guest_heartbeat
            .load(Ordering::Acquire);
        let hold = Self::us_to_cycles(WOKEN_GRANT_HOLD_US, self.host_tsc_khz);
        let mut count = 0u32;
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            let granted = self.woken_grant_tsc[pcpu];
            if self.managed_pcpus[pcpu]
                && granted != 0
                && self.woken_grant_heartbeat[pcpu] == heartbeat
                && now_tsc.saturating_sub(granted) < hold
            {
                let owner = match self.core_states[pcpu] {
                    CoreState::Idle => None,
                    CoreState::Assigned { vm_id, .. } => Some(vm_id),
                    CoreState::Revoking {
                        recipient_vm_id, ..
                    } => Some(recipient_vm_id),
                };
                if owner == Some(vm_id) {
                    count += 1;
                }
            }
            pcpu += 1;
        }
        count
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

    /// Returns how `vm_id` relates to the SMT siblings of `pcpu`: `None` if a sibling is
    /// assigned to, or being handed to or from, another VM (`scx_pylon` won't run the two side
    /// by side), else `Some(true)` if a sibling is already `vm_id`'s.
    #[inline]
    fn sibling_fit(&self, pcpu: usize, vm_id: u64) -> Option<bool> {
        let mut shares_core = false;
        let mut sib = pcpu;
        let mut i = 0usize;
        while i < MAX_SMT {
            sib = self.smt_next[sib] as usize;
            if sib == pcpu {
                break;
            }
            match self.core_states[sib] {
                CoreState::Idle => {}
                CoreState::Assigned { vm_id: owner, .. } => {
                    if owner != vm_id {
                        return None;
                    }
                    shares_core = true;
                }
                CoreState::Revoking {
                    donor_vm_id,
                    recipient_vm_id,
                    ..
                } => {
                    if donor_vm_id != vm_id || recipient_vm_id != vm_id {
                        return None;
                    }
                    shares_core = true;
                }
            }
            i += 1;
        }
        Some(shares_core)
    }

    /// Finds an idle managed core that `vm_id` may use, preferring one whose SMT sibling the VM
    /// already holds so that whole cores stay free for other VMs.
    #[inline]
    fn find_idle_pcpu(&self, vm_id: u64) -> Option<usize> {
        let mut found = None;
        let mut pcpu = 0usize;
        while pcpu < MAX_PCPUS {
            if self.managed_pcpus[pcpu] && self.core_states[pcpu] == CoreState::Idle {
                match self.sibling_fit(pcpu, vm_id) {
                    Some(true) => return Some(pcpu),
                    Some(false) if found.is_none() => found = Some(pcpu),
                    _ => {}
                }
            }
            pcpu += 1;
        }
        found
    }

    /// Whether `vm`, woken from zero cores, may take a core from a donor at its `min_cores`:
    /// at most once per `FLOOR_PREEMPT_INTERVAL_US`.
    #[inline]
    fn floor_preempt_ready(vm: &VmHandle, now_tsc: u64, tsc_khz: u64) -> bool {
        let last = vm.floor_preempt_tsc();
        last == 0
            || now_tsc
                >= last.saturating_add(Self::us_to_cycles(FLOOR_PREEMPT_INTERVAL_US, tsc_khz))
    }

    /// Selects the best donor `(pcpu, donor_eval_idx, donor_vcpu, at_floor)` to preempt for
    /// `recipient`, preferring donor vCPUs with `in_critical_section == 0`. A donor at its
    /// `min_cores` (`at_floor`) is eligible only if `below_floor_allowed`.
    fn select_donor_pcpu(
        &self,
        vms: &[VmHandle],
        vm_count: usize,
        recipient: VmDemandEval,
        below_floor_allowed: bool,
    ) -> Option<(usize, usize, u8, bool)> {
        let mut best: Option<(usize, usize, u8, bool, bool, u64, bool)> = None;

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
                && self.sibling_fit(pcpu, recipient.vm_id).is_some()
                && let Some(donor_eval_idx) = self.find_eval_idx(vm_count, donor_vm_id)
            {
                let donor_eval = self.eval_buf[donor_eval_idx];
                let over_desired = donor_eval.assigned_cores > donor_eval.desired_cores;
                let over_fair_share =
                    donor_eval.assigned_cores > recipient.committed_cores().saturating_add(1);
                // Urgency alone may move a core only while the donor keeps at least as many
                // cores per unit of weight as the recipient will have. Without that bound, the
                // more urgent VM takes the donor below its fair share, `over_fair_share` takes
                // the core back, and the two trade it on every sweep (spec 05 K7).
                let donor_weight = u64::from(vms[donor_eval.vm_idx].priority_weight().max(1));
                let recipient_weight = u64::from(vms[recipient.vm_idx].priority_weight().max(1));
                let within_share = u64::from(recipient.committed_cores() + 1) * donor_weight
                    <= u64::from(donor_eval.assigned_cores) * recipient_weight;
                let lower_urgency = donor_eval.urgency < recipient.urgency && within_share;
                let above_min = donor_eval.assigned_cores > donor_eval.min_cores;

                let eligible = (above_min && (over_desired || over_fair_share || lower_urgency))
                    || (recipient.zero_core_wakeup && below_floor_allowed && lower_urgency);
                // A `PARKED` vCPU on an assigned core either hasn't woken since the assignment
                // or has given the core back, which phase 0 reclaims. Revoking it would race
                // its wakeup.
                let donor_vm = &vms[donor_eval.vm_idx];
                let donor_running = donor_vm.host_page().vcpus[donor_vcpu as usize]
                    .state
                    .load(Ordering::Acquire)
                    != VCPU_STATE_PARKED;

                if eligible && donor_running {
                    let in_cs = donor_vm.shared_page().vcpus[donor_vcpu as usize]
                        .in_critical_section
                        .load(Ordering::Acquire)
                        != 0;
                    let not_in_cs = !in_cs;

                    let replace = match best {
                        None => true,
                        Some((_, _, _, best_not_in_cs, best_over_desired, best_urgency, _)) => {
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
                            !above_min,
                        ));
                    }
                }
            }
            pcpu += 1;
        }

        best.map(|(pcpu, eval_idx, vcpu, _, _, _, at_floor)| (pcpu, eval_idx, vcpu, at_floor))
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

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;

    use super::*;

    fn engine_with_cores(pcpus: &[usize]) -> MatrixEngine {
        MatrixEngine::new(
            Arc::new(ArcSwap::from_pointee(Vec::new())),
            Arc::new(PcpuOwnerTable::new_in_memory()),
            pcpus,
        )
    }

    fn assigned(vm_id: u64) -> CoreState {
        CoreState::Assigned { vm_id, vcpu: 0 }
    }

    #[test]
    fn resignal_interval_doubles_up_to_its_cap() {
        // At 3 GHz: 10 us is 30,000 cycles, 1 ms 3,000,000.
        const KHZ: u64 = 3_000_000;
        let due = |first, last, now| MatrixEngine::resignal_due(first, last, now, KHZ);
        let mut last = 1_000;
        let mut sent = vec![last];
        for now in (1_000..20_000_000).step_by(1_000) {
            if due(1_000, last, now) {
                last = now;
                sent.push(now);
            }
        }
        let intervals: Vec<u64> = sent.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            intervals[..8],
            [
                30_000, 30_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_920_000
            ]
        );
        assert!(intervals[8..].iter().all(|&i| i == 3_000_000));
    }

    #[test]
    fn idle_cores_skip_siblings_of_other_vms() {
        let mut engine = engine_with_cores(&[0, 1, 2, 3]);
        // Two cores: CPUs 0 and 2, and CPUs 1 and 3.
        let mut ring: [u32; MAX_PCPUS] = from_fn(|pcpu| pcpu as u32);
        ring[..4].copy_from_slice(&[2, 3, 0, 1]);
        engine.set_smt_ring(&ring);

        engine.set_core_state(0, assigned(1));
        // VM 1 gets the other thread of its core; VM 2 gets a core of its own.
        assert_eq!(engine.find_idle_pcpu(1), Some(2));
        assert_eq!(engine.find_idle_pcpu(2), Some(1));

        engine.set_core_state(1, assigned(2));
        // CPU 2 sits next to VM 1 and CPU 3 next to VM 2.
        assert_eq!(engine.find_idle_pcpu(3), None);
        assert_eq!(engine.find_idle_pcpu(2), Some(3));

        // A core being handed between VMs counts as both.
        engine.set_core_state(
            1,
            CoreState::Revoking {
                donor_vm_id: 2,
                donor_vcpu: 0,
                recipient_vm_id: 3,
                recipient_vcpu: 0,
                deadline_tsc: 0,
            },
        );
        assert_eq!(engine.find_idle_pcpu(2), None);
        assert_eq!(engine.find_idle_pcpu(3), None);
        assert_eq!(engine.find_idle_pcpu(1), Some(2));
    }

    #[test]
    fn without_a_ring_every_core_stands_alone() {
        let mut engine = engine_with_cores(&[0, 1]);
        engine.set_core_state(0, assigned(1));
        assert_eq!(engine.find_idle_pcpu(2), Some(1));
    }
}
