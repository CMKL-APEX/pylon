# VMM integration

The VMM side of Pylon is
[`vmm/src/pylon.rs`](https://github.com/CMKL-APEX/pylon/blob/main/vmm/src/pylon.rs)
plus short hooks in upstream files
([Repository and code organization](organization.md#vmm-hook-points)).
The design is in
[`spec/03-scx-and-vmm-hooks.md` §2](https://github.com/CMKL-APEX/pylon/blob/main/spec/03-scx-and-vmm-hooks.md#2-cloud-hypervisor-vmm-integration-vmmsrcpylonrs).

## State

```rust
pub struct PylonVmmState {
    config: PylonConfig,
    boot_vcpus: u32,
    max_vcpus: u32,              // slots in use: max(boot, max)
    initial_active_vcpus: u32,
    shm_file: File,
    shared_page: NonNull<PylonSharedPage>,   // mmap of shm_file, MAP_SHARED
    kick_eventfds: Vec<EventFd>,             // sent to pylond; pylon-kick's MSI-X notifiers
    wake_eventfds: Vec<EventFd>,             // sent to pylond; blocking, or non-blocking with park in KVM
    parked_in_kvm: Vec<AtomicBool>,          // per vCPU: parked inside KVM
    park_bounds: Vec<ParkBound>,             // per vCPU: timer and deadline that bound a park in KVM
    shutting_down: AtomicBool,
}
```

There is one `PylonVmmState` per VMM process, held in a process-global
`Mutex<Option<Arc<PylonVmmState>>>` (`GLOBAL_PYLON_STATE`). The hooks
reach it through free functions (`pylon::on_vcpu_thread_start(id)` and
so on) that do nothing when `--pylon` wasn't given. A global keeps the
hooks one line each, with no new parameters threaded through upstream
constructors. The cost is that a VMM process can hold only one Pylon VM.
Cloud Hypervisor runs one VM per process, so that holds today.

`PylonVmmState` is `Send` and `Sync` by `unsafe impl`. That's sound
because, after `new` finishes, the page is reached only through atomic
fields and `EventFd`s are thread-safe. Any non-atomic access added later
breaks that argument.

## Hooks

### Configuration: `parse_and_configure`

Called at the end of `VmConfig::parse` in `vmm/src/config.rs`, after
validation. It parses
`--pylon`, then builds and installs `PylonVmmState`, creating the page.
Building the state during option parsing is a side effect in an unusual
place: it happens before the VM exists, and before the VMM installs its
logger, so nothing logged there shows. Registration with `pylond`
therefore waits for `setup_memory_region`.

The registration message carries `kick_signal`: the VMM's own
`SIGRTMIN()`, the signal its vCPU threads have a no-op handler for.
`pylond` sends it to a donor vCPU whose revocation deadline has passed.
With park in KVM, the message also carries `wake_by_signal: true`
([Control socket protocol](../reference/control-socket.md#registration)).
The VMM registers only if the socket exists, once per process, and
waits up to 1 second for the reply. It logs where its vCPUs park when
`pylond` accepts it, and a warning with `pylond`'s reason when it
doesn't; the VM then runs unmanaged. `PylonVmmState::new` also sets every slot's `assigned_pcpu` to
`PCPU_UNASSIGNED`, in the shared page and in the
[host page](shared-memory-abi.md#the-host-page), which `pylond` reads
as "no core on its way".

### Memory: `setup_memory_region`

Called from `MemoryManager::new` in `vmm/src/memory_manager.rs`. It
wraps a clone of the shared memory `memfd` in a guest region at `gpa`
and registers it with the hypervisor as a new memory slot
(`MemoryVisibility::Shared`). It doesn't add the region to the guest's
memory map, like a device BAR
([Shared-memory ABI](shared-memory-abi.md#the-page-in-guest-memory));
`MemoryManager` keeps the region alive. It then registers the VM with
`pylond` (`PylonVmmState::register`). With park in KVM it also turns
halt polling off for the VM (`KVM_CAP_HALT_POLL` set to 0) and only logs
a warning if that fails. Errors surface as
`memory_manager::Error::Pylon`.

### Backend check: `check_hypervisor`

Called from `CpuManager::new`. It fails VM creation with `--pylon` on
MSHV on aarch64, which can't read the guest's counter and timer.

### Kick device

`DeviceManager` adds the `pylon-kick` PCI device
(`vmm/src/pylon_kick.rs`) when `--pylon` is given. Its MSI-X notifiers
are the kick `eventfd`s, and its BAR0 holds the park doorbell
([Enhancements](enhancements.md#kick-delivery-the-pylon-kick-pci-device)).

### vCPU start: `on_vcpu_thread_start`

Called at the top of each vCPU thread in `vmm/src/cpu.rs`, after the
thread barrier and before the first `KVM_RUN`. It stores `gettid()` in
`host_tid`, in the host page and then in the guest's copy, measures the guest counter offset into
`guest_tsc_offset` ([Time bases](shared-memory-abi.md#time-bases)), sets
`RUNNING`, and for `vcpu_id >= initial_active_vcpus` also sets the
slot's `park_on_boot` tag. With park in KVM it also creates the vCPU's
park bound timer. It doesn't park those vCPUs: a guest brings its secondary CPUs
online at boot (`PSCI CPU_ON` on aarch64, INIT/SIPI on x86_64), and a
vCPU whose thread never enters `KVM_RUN` doesn't answer, so the guest
waits for it, gives up, and boots without it. Once the guest has
booted (a non-zero `guest_heartbeat`), `pylond` sets `revoke_requested`
on each tagged vCPU and signals its thread, and `check_and_park_vcpu`
parks it (below).

### After each exit: `check_and_park_vcpu`

Called in the vCPU run loop after the vCPU lock is dropped, on every
exit to userspace. It applies the decision table from spec 06 §4.4
(`park_decision`):

| `state` | `revoke_requested` | Deadline | Action |
|---|---|---|---|
| any, `park_on_boot` set | 1 | any | Boot park: park at once, without a yield or forced-park count, and clear the tag |
| `PARKING` | any | any | Cooperative park |
| other | 1 | `now >= revoke_deadline_tsc` | Forced park |
| other | 1 | not passed | Keep running: the guest hasn't agreed and may hold a lock |
| other | 0 | — | Keep running |

`state` is the guest's, from the shared page: the guest asks to park by
writing `PARKING` there. `revoke_requested`, the deadline and
`park_on_boot` come from the
[host page](shared-memory-abi.md#the-host-page), so a guest can't clear
or postpone a revocation. What the VMM publishes (`state`,
`yield_count`, clearing `revoke_requested` and `park_on_boot`) goes to
the host page first and then to the guest's copy.

A vCPU already parked inside KVM skips this table; see
[Parking inside KVM](#parking-inside-kvm).

`now` is the host counter (the TSC, or `CNTVCT_EL0` on aarch64), the clock `pylond` writes deadlines in. To park,
it clears `revoke_requested`, increments `yield_count` (and
`forced_preempt_count` for a forced park), records `last_yield_tsc`,
records the guest's armed timer as `park_timer_deadline_tsc`, publishes
`PARKED` with `Release`, blocks on the wake `eventfd`, and on wakeup
clears `park_timer_deadline_tsc` and sets `RUNNING` again (unless
shutting down). A boot park skips the counters and clears
`park_on_boot` instead.

Under KVM with an in-kernel LAPIC, a running vCPU exits to userspace
only for MMIO, PIO, signals and similar, so on its own this check runs
at unpredictable times. Spec 06 adds the two exits that make it timely:
the guest's park doorbell (S4) and `pylond`'s signal when a deadline
passes (S2).

Before S1, this hook parked on any exit while `revoke_requested` was
set, and was also called from the `VmExit::Reset` arm, where KVM
reports triple faults. Both are gone
([Enhancements](enhancements.md#problems-in-the-original-revocation-path)).

### Shutdown: `shutdown_all`

Called from `CpuManager::shutdown`. It sets `shutting_down`, writes
every wake `eventfd`, so parked threads return and see the VM is
stopping, and stops the park bound timers.

## Parking

```rust
pub fn park_vcpu(&self, vcpu_id: u32) {
    while !self.shutting_down.load(Ordering::Acquire) {
        match wake_evt.read() {           // blocking read
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}
```

A parked thread holds no VMM locks, so pausing, snapshotting or
shutting down the VM doesn't deadlock on it. But the VMM's pause
protocol (`SIGRTMIN`, `vcpu_paused`) doesn't know about it either.
Pausing a VM with parked vCPUs hasn't been tested.
<!-- TODO(verify): pause/resume with parked vCPUs. -->

## Parking inside KVM

With `--pylon park=kvm`
([The --pylon option](../reference/pylon-option.md)), the default on
x86_64 KVM hosts, a
parked vCPU sleeps in `KVM_RUN` instead of in `park_vcpu`. The design
and its measurements are in
[spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md).
The run loop passes `check_and_park_vcpu` a callback, `kvm_park_op`,
that runs one `KvmParkOp` on the locked vCPU:

| Op | x86_64 | aarch64 |
|---|---|---|
| `InterruptsEnabled` | `RFLAGS.IF` is set | Always true: `WFI` wakes on a masked interrupt |
| `Halt` | `KVM_MP_STATE_HALTED` | `KVM_MP_STATE_SUSPENDED` |
| `Run` | `KVM_MP_STATE_RUNNABLE` | `KVM_MP_STATE_RUNNABLE` |

Built without the `kvm` feature, the callback always fails.

**Park.** The decision table is the same. Before publishing anything,
the VMM checks `InterruptsEnabled` and sets `Halt`. If either fails, the
vCPU keeps running as if it hadn't been asked, and `pylond` signals it
again (a halted x86 vCPU with interrupts off wouldn't wake for an IPI).
Then it updates the slot as above, publishes `PARKED`, marks the vCPU
parked in KVM, and returns. The run loop's next `KVM_RUN` sleeps until
an interrupt or a signal.

**Wake by interrupt.** KVM runs the guest again without an exit. The
VMM doesn't see it, and `state` stays `PARKED`. `scx_pylon` decides
where the thread runs ([scx_pylon](scx-pylon.md)).

**Wake for a core.** The wake `eventfd` can't reach a thread in
`KVM_RUN`, so `pylond` also signals the thread. After each exit of a
vCPU parked in KVM, the VMM:

1. turns `PARKING` back into `PARKED`: a guest park request that came
   after a forced park is already done (spec 08 §3.7);
2. reads the non-blocking wake `eventfd`. If it was written, it stops
   the park bound, sets `Run`, clears `park_timer_deadline_tsc` and
   publishes `RUNNING`;
3. otherwise, if the park bound has passed, it sets `Run` and leaves
   `state` at `PARKED`. The vCPU then runs without a core until it halts
   again.

**Bound.** A cooperative or forced park (not a boot park) arms a
per-vCPU POSIX timer that sends `SIGRTMIN` to the vCPU thread after
`PARK_IN_KVM_BOUND` (4 ms) and every 4 ms after that. It covers a vCPU
that nothing would wake: no timer armed, or work queued without an IPI
(spec 08 §3.6).

**Halt polling** is off for the VM (`PARK_IN_KVM_HALT_POLL_NS` = 0),
so a parked vCPU costs no host CPU. This also affects the guest's own
`HLT`.

## Seccomp

Pylon adds these rules to `vmm/src/seccomp_filters.rs`:

| Thread | Allowed | Why |
|---|---|---|
| vCPU | `KVM_GET_MSRS` (x86_64) | Read the guest TSC and TSC deadline |
| vCPU | `KVM_GET_ONE_REG` (aarch64) | Read the guest counter and virtual timer |
| vCPU | `KVM_SET_MP_STATE`, and `KVM_GET_REGS` on x86_64 | Park inside KVM |
| vCPU and VMM | `timer_create`, `timer_settime`, `timer_delete` | Park bound timers |

## Tests

`cargo test -p vmm --features kvm --lib pylon` runs the tests in
`vmm/src/pylon.rs` and `vmm/src/pylon_kick.rs`. They cover option
parsing, page initialization, thread-ID registration, boot parks, every
row of the park decision table, cooperative and forced parks, the guest
counter offset and timer conversion, parking inside KVM (with a fake
`KvmParkOp` callback), and daemon registration over a real socket. A second real-KVM
test runs a guest that never agrees to park, sends its vCPU thread
`SIGRTMIN` with `tgkill` as `pylond` does, and checks that `KVM_RUN`
returns and the VMM force-parks the vCPU. One test boots a minimal real-mode KVM guest that reads and writes
the shared page and parks cooperatively by writing `PARKING` and
executing `HLT`. Without an in-kernel irqchip, `HLT` exits to
userspace and stands in for the park doorbell. It needs `/dev/kvm`.
