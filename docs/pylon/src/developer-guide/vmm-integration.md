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
    wake_eventfds: Vec<EventFd>,             // blocking, parked threads read them
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

Called from `VmConfig` parsing in `vmm/src/config.rs`. It parses
`--pylon`, then builds and installs `PylonVmmState` (creating the page
and registering with `pylond`). Building the state during option
parsing is a side effect in an unusual place: it happens before the VM
exists, and spec 06 moves registration later for that reason.

The registration message carries `kick_signal`: the VMM's own
`SIGRTMIN()`, the signal its vCPU threads have a no-op handler for.
`pylond` sends it to a donor vCPU whose revocation deadline has passed.
`PylonVmmState::new` also sets every slot's `assigned_pcpu` to
`PCPU_UNASSIGNED`, which `pylond` reads as "no core on its way".

### Memory: `setup_memory_region`

Called from `MemoryManager::new` in `vmm/src/memory_manager.rs`. It
wraps a clone of the `shm` file descriptor in a `GuestRegionMmap` at
`gpa`, inserts it into `GuestMemoryMmap`, and registers it with the
hypervisor as a new memory slot (`MemoryVisibility::Shared`). Errors
surface as `memory_manager::Error::Pylon`.

### vCPU start: `on_vcpu_thread_start`

Called at the top of each vCPU thread in `vmm/src/cpu.rs`, after the
thread barrier and before the first `KVM_RUN`. It stores `gettid()` in
the slot's `host_tid`, sets `RUNNING`, and for
`vcpu_id >= initial_active_vcpus` also sets the slot's `park_on_boot`
tag. It doesn't park those vCPUs: a guest brings its secondary CPUs
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

`now` is the host counter (the TSC, or `CNTVCT_EL0` on aarch64), the clock `pylond` writes deadlines in. To park,
it clears `revoke_requested`, increments `yield_count` (and
`forced_preempt_count` for a forced park), records `last_yield_tsc`,
publishes `PARKED` with `Release`, blocks on the wake `eventfd`, and
sets `RUNNING` again on wakeup (unless shutting down). A boot park
skips the counters and clears `park_on_boot` instead.

Under KVM with an in-kernel LAPIC, a running vCPU exits to userspace
only for MMIO, PIO, signals and similar, so on its own this check runs
at unpredictable times. Spec 06 adds the two exits that make it timely:
the guest's park doorbell (S4) and `pylond`'s signal when a deadline
passes (S2).

Before S1, this hook parked on any exit while `revoke_requested` was
set, and was also called from the `VmExit::Reset` arm, where KVM
reports triple faults. Both are gone
([Enhancements](enhancements.md#problems-in-the-current-revocation-path)).

### Shutdown: `shutdown_all`

Called from `CpuManager::shutdown`. It sets `shutting_down` and writes
every wake `eventfd`, so parked threads return and see the VM is
stopping.

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

## Tests

`cargo test -p vmm --features kvm --lib pylon` runs eleven tests covering
option parsing, page initialization, thread-ID registration, initial
parking, every row of the park decision table, cooperative and forced
parks, and daemon registration over a real socket. A second real-KVM
test runs a guest that never agrees to park, sends its vCPU thread
`SIGRTMIN` with `tgkill` as `pylond` does, and checks that `KVM_RUN`
returns and the VMM force-parks the vCPU. One test boots a minimal real-mode KVM guest that reads and writes
the shared page and parks cooperatively by writing `PARKING` and
executing `HLT`. Without an in-kernel irqchip, `HLT` exits to
userspace and stands in for the park doorbell. It needs `/dev/kvm`.
