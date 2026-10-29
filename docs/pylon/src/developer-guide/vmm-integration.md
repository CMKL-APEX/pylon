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
    kick_eventfds: Vec<EventFd>,             // non-blocking, sent to pylond
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

### Memory: `setup_memory_region`

Called from `MemoryManager::new` in `vmm/src/memory_manager.rs`. It
wraps a clone of the `shm` file descriptor in a `GuestRegionMmap` at
`gpa`, inserts it into `GuestMemoryMmap`, and registers it with the
hypervisor as a new memory slot (`MemoryVisibility::Shared`). Errors
surface as `memory_manager::Error::Pylon`.

### vCPU start: `on_vcpu_thread_start`

Called at the top of each vCPU thread in `vmm/src/cpu.rs`, after the
thread barrier and before the first `KVM_RUN`. It stores `gettid()` in
the slot's `host_tid`. For `vcpu_id >= initial_active_vcpus`, it sets
`PARKED` and blocks on the wake `eventfd`. For the others, it sets
`RUNNING`.

### After each exit: `check_and_park_vcpu`

Called in the vCPU run loop after the vCPU lock is dropped, on every
exit to userspace. If the slot's `revoke_requested` is `1`, it clears
the flag, sets `PARKED`, increments `yield_count`, blocks on the wake
`eventfd`, and sets `RUNNING` again on wakeup (unless shutting down).

It's also called from the `VmExit::Reset` arm. If it parks there, the
loop continues instead of resetting the VM.

Both hooks have problems
([Enhancements](enhancements.md#problems-in-the-current-revocation-path)):

- Under KVM with an in-kernel LAPIC, a running vCPU exits to userspace
  only for MMIO, PIO and similar, so the check runs late and unpredictably.
- It doesn't consult `in_critical_section`.
- The `Reset` arm was meant for HLT exits, which KVM handles in the
  kernel. KVM also reports a triple fault as `Reset`, so a triple fault
  during a revocation is swallowed.

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

`cargo test -p vmm --features kvm --lib pylon` runs seven tests covering
option parsing, page initialization, thread-ID registration, initial
parking, park and wake, shutdown, and daemon registration over a real
socket. One test boots a minimal real-mode KVM guest that reads and
writes the shared page, so it needs `/dev/kvm`.
