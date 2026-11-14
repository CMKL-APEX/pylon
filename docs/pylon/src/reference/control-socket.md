# Control socket protocol

`pylond` listens on a Unix stream socket (`--socket`, default
`/run/pylon/pylond.sock`). Each connection carries **one request and one
response**: the client sends one JSON object in a single `sendmsg`,
optionally with file descriptors attached as `SCM_RIGHTS`, and `pylond`
replies with one JSON object.

Anyone who can write to the socket can register a VM. `pylond` records
the registering peer's UID (`SO_PEERCRED`): stats and policy updates
cover only the VMs that UID registered, unless the caller is root or
runs as `pylond`'s own UID, who see and manage every VM
([Running pylond](../operator-guide/pylond.md#the-control-socket)).

## Telling messages apart

Messages have no type field. `pylond` decides in this order:

1. **File descriptors attached:** a [registration](#registration).
2. **An `action` of `"stats"`, `"metrics"` or `"status"`:** a
   [stats request](#stats).
3. **Otherwise:** a [policy update](#policy-update).

A message meant as a stats request with a misspelled `action` is
therefore treated as a policy update. Because it has no `vm_id`, it's
rejected with `{"accepted": false, "message": "invalid control request:
..."}`.

## Registration

Sent by the VMM at start. You don't send this by hand.

```json
{"vm_id": 17, "num_vcpus": 8, "min_cores": 1, "max_cores": 8, "priority_weight": 100, "kick_signal": 34}
```

`kick_signal` (optional) is the signal that interrupts a vCPU's
`KVM_RUN` in the VMM: `SIGUSR2` (12 on x86_64 and aarch64) from the
Pylon VMM, which sent its `SIGRTMIN` up to v0.0.8-rc3. `pylond` sends
it to a donor vCPU's thread when a revocation's deadline passes. It
must be `SIGUSR1`, `SIGUSR2` or a real-time signal. Without it,
`pylond` never signals the VMM. `pylond` takes the
VMM's PID from the socket's peer credentials, not from the message.

`wake_by_signal` (optional, default `false`) is sent as `true` by a VMM
that parks vCPUs inside KVM (`--pylon park=kvm`, spec 08). The wake
`eventfd` can't reach a thread blocked in `KVM_RUN`, so `pylond` also
sends `kick_signal` when it assigns a core, and again until the vCPU
leaves `PARKED`. The VMM leaves the field out when it's `false`.
`pylond` rejects such a registration when `scx_pylon` isn't attached:
without it, a woken vCPU would run on any CPU.

Attached descriptors, in order: the shared page's file, then
`num_vcpus` kick `eventfd`s, then `num_vcpus` wake `eventfd`s (1 + 2 ×
`num_vcpus` in all).

`pylond` maps the shared page's file and writes the `eventfd`s, so it
checks them first. The file must be a `memfd` on shmem whose size is
sealed (`F_SEAL_SHRINK` and `F_SEAL_GROW`): one that could shrink would
make `pylond` crash with `SIGBUS` when it next reads the page. Each
kick and wake descriptor must be an `eventfd`. A registrant still
shares those `eventfd`s and could make a write to one block, so a
watchdog thread interrupts any write that blocks for 1 ms (with
`SIGUSR1`); `pylond` then stops writing to that VM's descriptors and
drops its registration.

| Response | When |
|---|---|
| `{"accepted": true, "message": "registered"}` | The page's header is valid and the `vm_id` is free. An earlier registration of the same `vm_id` is replaced only if its VMM has exited. |
| `{"accepted": false, "message": "...a running VMM already registered this vm_id"}` | Another VM with this `vm_id` is registered, and its VMM is still running. |
| `{"accepted": false, "message": "expected <n> FDs, got <m>"}` | Wrong number of descriptors. |

`pylond` closes the connection after the response. Both sides log a
rejection as a warning; the VM then runs without `pylond`. A registration whose JSON doesn't parse gets no response.
A registration's `priority_weight` of 0 is stored as 1.
| `{"accepted": false, "message": "<error>"}` | `num_vcpus` isn't in 1–64; `max_cores` is 0, below `min_cores` or above `num_vcpus`; `kick_signal` isn't `SIGUSR1`, `SIGUSR2` or a real-time signal; the file isn't a `memfd` on shmem sealed with `F_SEAL_SHRINK` and `F_SEAL_GROW`, couldn't be mapped, is smaller than 16,384 bytes, or the header of its shared page or host page is invalid; a kick or wake file descriptor isn't an `eventfd`; or a pidfd for the peer couldn't be opened. |

## Stats

```json
{"action": "stats"}
{"action": "stats", "vm_id": 17}
```

`action` may also be `"metrics"` or `"status"`; they mean the same. With
`vm_id`, only that VM is listed (an empty list if it isn't registered).
Only the VMs the caller may manage are listed.

```json
{
  "accepted": true,
  "message": "stats retrieved",
  "vms": [
    {
      "vm_id": 17,
      "num_vcpus": 8,
      "min_cores": 1,
      "max_cores": 8,
      "priority_weight": 100,
      "current_cores": 3,
      "allocated_core_microseconds": 81234567,
      "cooperative_yields": 12,
      "forced_preemptions": 4,
      "runnable_threads": 5,
      "handoffs_without_park": 0,
      "woken_shared_microseconds": 0,
      "woken_unowned_microseconds": 0
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `current_cores` | `u32` | vCPUs with an assigned CPU whose state isn't `PARKED`. |
| `allocated_core_microseconds` | `u64` | Core-microseconds metered to the VM since it registered. |
| `cooperative_yields` | `u64` | Revocations completed by the donor parking before its deadline passed. |
| `forced_preemptions` | `u64` | Revocations whose deadline passed before the donor parked. |
| `runnable_threads` | `u32` | The guest's last report. |
| `handoffs_without_park` | `u64` | Revocations where the core was handed over although the donor never parked. Should stay at 0. |
| `woken_shared_microseconds` | `u64` | Time the VM's parked vCPUs spent woken by KVM while the VM owned a core (spec 08). Always 0 for a VM registered without `wake_by_signal`. |
| `woken_unowned_microseconds` | `u64` | The same, while the VM owned no core. |

## Policy update

```json
{"vm_id": 17, "min_cores": 2, "max_cores": 6, "priority_weight": 250}
```

Every field except `vm_id` is optional. Missing fields keep their value.

| Response | When |
|---|---|
| `{"accepted": true, "message": "policy updated", "min_cores": 2, "max_cores": 6, "priority_weight": 250}` | Applied. The response gives the resulting values. |
| `{"accepted": false, "message": "VM 17 not found, or registered by another user"}` | No VM with that id, or one the caller may not manage. |
| `{"accepted": false, "message": "..."}` | The result would have `max_cores` = 0, `min_cores` > `max_cores`, or `max_cores` > `num_vcpus`. |

A `priority_weight` of 0 is stored as 1. Updates take effect in the
engine's next sweep and aren't persisted.

## Client helpers

The `pylond` library has Rust clients for these messages, used by its
tests: `send_registration`, `send_policy_update` and `query_vm_stats` in
`pylon/pylond/src/control.rs`.
