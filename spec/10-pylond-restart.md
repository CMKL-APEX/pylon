# Specification 10: Surviving a `pylond` Restart

> **Status:** `DRAFT`. Not started. It answers spec 06 Q5 (#60): how a
> VMM finds out that `pylond` is gone, runs its VM without it, and
> registers again when `pylond` comes back.

## 1. Motivation

A VMM registers with `pylond` once, at boot (`PylonVmmState::register`
in `vmm/src/pylon.rs`), and `pylond` closes the connection after its
reply. Nothing tells the VMM when `pylond` stops, and nothing makes it
register again. So when `pylond` exits, for an upgrade, a crash or a
restart by systemd:

- **`park=vmm` VMs stall.** A parked vCPU's thread blocks on its wake
  `EventFd`, which only `pylond` writes. Its vCPUs stay parked for
  good, and the guest gets the soft lockups and RCU stalls of spec 08
  §1.
- **`park=kvm` VMs run on, unmanaged.** A parked vCPU wakes on
  interrupts and, after the 4 ms bound (spec 08 §3.6), runs as a woken
  vCPU. `scx_pylon` leaves with `pylond`, so the kernel's default
  scheduler runs every vCPU thread anywhere. But the VM's vCPUs stay
  `PARKED`, and a revocation in flight stays set.
- **Neither comes back.** A new `pylond` doesn't know the VM. The VM
  stays unmanaged until its VMM restarts.
- **A VMM that starts before `pylond`** finds no socket, logs that the
  VM runs unmanaged, and never registers.

## 2. Goals & Non-Goals

**Goals**
- The VMM learns within a few milliseconds that `pylond` is gone.
- Without `pylond`, every vCPU of the VM runs, under the host's
  scheduler, as if the VM had no `--pylon`.
- When a `pylond` is listening again, the VMM registers again, and the
  VM is managed as after boot.
- A VMM that started before `pylond` registers once it appears.
- A VMM and a `pylond` of `v0.0.8` keep working with the new ones, as
  far as §5 allows.

**Non-Goals**
- **Carrying `pylond`'s state across a restart** (metering, hold times,
  urgency). A new `pylond` starts from what the VMs report.
- **Restarting without disturbing the VMs.** For the time `pylond` is
  away, its VMs run unmanaged: they may take more or fewer cores than
  their policy allows.
- **Policy changes made over the control socket** are lost with
  `pylond`. A management layer that set them sets them again after a
  restart; the VMM re-registers with its command-line policy.
- **Snapshot and migration** (#61).

## 3. Design

### 3.1 A registration is a session

The registration connection stays open for as long as the VM is
registered:

- **The VMM** sends its registration as today and asks for a session
  (`session: true`). After `pylond`'s reply, which now ends with a
  newline, it keeps the socket.
- **`pylond`** answers `session: true` when it accepts, keeps its end
  of the socket in the VM's handle, and closes it when it removes the
  VM.
- **Either side closing** ends the session: `pylond` exiting, for any
  reason, closes every session at once, as the kernel closes its
  sockets.

Nothing else travels over a session for now. `pylond` keeps detecting a
VMM's exit with its pidfd; a closed session is a second sign.

### 3.2 The VMM without `pylond`

A VMM thread, `pylon-session`, waits on the session socket. When it
reads end of file (or an error), the VM becomes **unmanaged**:

1. **The VMM sets `unmanaged`**, a flag that its park decision reads
   first. While it is set, the VMM parks no vCPU: a guest `PARKING` is
   turned back into `RUNNING` at the next exit, and a revocation is
   ignored.
2. **It clears `revoke_requested`** in both slots of every vCPU, so
   `pv-pylon` stops trying to park.
3. **It wakes every parked vCPU** as `pylond` would:
   - `park=vmm`: writes the vCPU's wake `EventFd`;
   - `park=kvm`: signals the vCPU thread with its `kick_signal`; the
     VMM then makes the vCPU runnable, as for a core assignment
     (spec 08 §3.3), and stops its bound timer.
4. **Each woken vCPU writes `RUNNING`** with `assigned_pcpu` unassigned,
   and runs where the host scheduler puts it.
5. **It logs once** that `pylond` is gone and the VM runs unmanaged.

A VMM that finds no socket at boot starts unmanaged too. Its vCPUs
then all run: with no `pylond`, the boot parks (`park_on_boot`) never
happen.

### 3.3 Registering again

While unmanaged, the `pylon-session` thread watches the socket's path:

- With `inotify` on the socket's directory, for the socket's name
  appearing (`IN_CREATE`, `IN_MOVED_TO`: `pylond` binds a temporary name
  and renames it into place).
- And every 5 seconds, in case the watch misses it (a directory that
  was replaced, or no `inotify`).

When the socket is there, the VMM prepares the host page as at boot
and registers again (§3.4). If `pylond` refuses or doesn't answer, the
VMM logs it and waits for the next sign: a refused registration isn't
retried until the socket appears again, so a VMM doesn't poll a
`pylond` that keeps refusing it.

### 3.4 What the new `pylond` sees

`pylond` already knows how to take on a VM whose vCPUs all run: it is
the boot of a VM with `initial_active_vcpus` vCPUs running and
`park_on_boot` set on the rest (spec 06 §4.4). So before it registers
again, with every vCPU running and none parked, the VMM:

1. Resets each vCPU's host slot, as at boot: `RUNNING`,
   `assigned_pcpu` unassigned, no revocation, `park_on_boot` set for
   vCPUs beyond `initial_active_vcpus`. `yield_count` and
   `forced_preempt_count` keep counting.
2. Clears `unmanaged` only once `pylond` has accepted the VM.

The guest's heartbeat is already non-zero, so `pylond` asks the
`park_on_boot` vCPUs to park at once, and they park without the grace
period, as at boot. The VM ends with `initial_active_vcpus` running,
and `pylond` gives it cores from there.

The same shared memory, kick and wake `EventFd`s are sent again. The
wake `EventFd`s are emptied first, so a count written by the old
`pylond` doesn't end a new park.

### 3.5 Several VMs at once

A restart makes every VM register again within a few milliseconds.

- **The connection limit.** `pylond` serves at most 64 connections at
  once (`MAX_CONNECTIONS`), each for at most 5 seconds. A session lasts
  as long as its VM, so `pylond` returns its connection's permit once
  the VM is accepted, and holds the socket outside the limit. The limit
  on sessions is the number of VM slots.
- **A burst of registrations** beyond the limit is turned away, as
  today. A turned-away VMM sees its connection closed before a reply,
  and tries again after 100 ms to 1 s, at random, up to the 5-second
  watch.

## 4. Component Changes

### 4.1 VMM (`vmm/src/pylon.rs`, `vmm/src/cpu.rs`)

- The session: keep the socket, read the reply up to its newline
  instead of to end of file.
- The `pylon-session` thread (§3.2, §3.3), started at boot even when the
  socket is missing.
- The `unmanaged` flag in the park decision, and the wake of every
  parked vCPU.
- The host page reset before a registration (§3.4).
- **A `pylond` that doesn't open a session** (no `session: true` in its
  reply, as from `v0.0.8`) is used as today: the VMM keeps no socket,
  and doesn't notice a restart.

### 4.2 `pylond` (`pylon/pylond/src/control.rs`)

- Accept `session: true`; keep the socket in the VM's handle; return
  the connection permit once the VM is accepted.
- Remove a VM whose session closes, as one whose VMM exited.
- A VMM that doesn't ask for a session (`v0.0.8`) gets today's reply,
  and the connection is closed.

### 4.3 ABI and messages

- The registration gains `session: bool`, and the reply `session: bool`
  and a closing newline. Both are optional in the JSON, so each side
  reads the other's older messages.
- The shared and host pages don't change.

### 4.4 `scx_pylon`

No change. It leaves with `pylond` and is attached again by the new
one, which lists the vCPU threads again as the VMs register.

## 5. Compatibility

| VMM | `pylond` | Result |
|---|---|---|
| new | new | Restarts handled (§3). |
| new | `v0.0.8` | The old `pylond` ignores `session` and closes after its reply, which has no `session`: the VMM works as today. Whether the old `pylond` accepts an unknown field is to be checked (§6 V4). |
| `v0.0.8` | new | No `session`: the new `pylond` closes after its reply, as today. |

## 6. Points to Verify

- **V1. Wake on loss.** A `park=vmm` VM with parked vCPUs, under load,
  whose `pylond` is killed: every vCPU runs within a few milliseconds,
  and the guest logs no stall.
- **V2. Re-registration.** The same VM, with `pylond` started again: it
  is registered, its `park_on_boot` vCPUs park, and it settles at its
  share. Two VMs at once.
- **V3. The engine and running vCPUs without a core.** `pylond` takes a
  VM whose vCPUs run with `assigned_pcpu` unassigned only at boot
  today. Check that the engine counts them as it does there (spec 05
  K7), with every vCPU running, not just `initial_active_vcpus`.
- **V4. Unknown fields.** Whether `v0.0.8`'s `pylond` and VMM accept a
  message with a field they don't know.
- **V5. A `pylond` that comes back fast**, before the VMM's
  `pylon-session` thread has woken the parked vCPUs: the VMM registers
  only after it has finished §3.2.

## 7. Implementation Steps

| Step | Content | Exit criteria |
|---|---|---|
| **R1** | Sessions on both sides; `pylond` removes a VM whose session closes; the permit is returned. | Unit tests over a socket pair: a session outlives the connection timeout, and closing it removes the VM. |
| **R2** | The VMM's `unmanaged` state and the wake of every parked vCPU, both park modes. | V1 on the bare-metal host. |
| **R3** | Registering again: the watch, the host page reset, the retry. | V2, V3 and V5. |
| **R4** | Docs: the operator guide (`pylond` restarts), the status page and the release notes. | — |

## 8. Alternatives Considered

- **`pylond` saves its state and takes the VMs back on restart.** It
  would need the shared memory and `EventFd`s, which only the VMM can
  hand over, so the VMM has to reconnect anyway; and the saved state
  would be stale by the time it is read.
- **Pass the sessions across the restart** (systemd's file descriptor
  store). It covers planned restarts under systemd, not crashes, and
  ties `pylond` to systemd.
- **The VMM polls `pylond`** (a heartbeat over the control socket). It
  finds the loss only as fast as it polls, costs a connection per VM
  per poll, and still needs §3.2 and §3.3.
- **`pylond` writes a heartbeat in the host page.** The VMM would have
  to watch it, and a stopped heartbeat can mean a stalled `pylond`, not
  a gone one. A closed socket can only mean it is gone.
