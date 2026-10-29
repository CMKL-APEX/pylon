# Control socket protocol

`pylond` listens on a Unix stream socket (`--socket`, default
`/run/pylon/pylond.sock`). Each connection carries **one request and one
response**: the client sends one JSON object in a single `sendmsg`,
optionally with file descriptors attached as `SCM_RIGHTS`, and `pylond`
replies with one JSON object.

There's no authentication. Anyone who can write to the socket can use
every message ([Running pylond](../operator-guide/pylond.md#the-control-socket)).

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
{"vm_id": 17, "num_vcpus": 8, "min_cores": 1, "max_cores": 8, "priority_weight": 100}
```

Attached descriptors, in order: the shared page's file, then
`num_vcpus` kick `eventfd`s, then `num_vcpus` wake `eventfd`s (1 + 2 ×
`num_vcpus` in all).

| Response | When |
|---|---|
| `{"accepted": true, "message": "registered"}` | The page's header is valid. Any VM already registered with the same `vm_id` is replaced. |
| `{"accepted": false, "message": "expected <n> FDs, got <m>"}` | Wrong number of descriptors. |
| `{"accepted": false, "message": "<error>"}` | The page couldn't be mapped, or its header is invalid. |

## Stats

```json
{"action": "stats"}
{"action": "stats", "vm_id": 17}
```

`action` may also be `"metrics"` or `"status"`; they mean the same. With
`vm_id`, only that VM is listed (an empty list if it isn't registered).

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
      "runnable_threads": 5
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `current_cores` | `u32` | vCPUs with an assigned CPU whose state isn't `PARKED`. |
| `allocated_core_microseconds` | `u64` | Core-microseconds metered to the VM since it registered. |
| `cooperative_yields` | `u64` | Revocations completed by the donor parking. |
| `forced_preemptions` | `u64` | Revocations completed by the deadline. |
| `runnable_threads` | `u32` | The guest's last report. |

## Policy update

```json
{"vm_id": 17, "min_cores": 2, "max_cores": 6, "priority_weight": 250}
```

Every field except `vm_id` is optional. Missing fields keep their value.

| Response | When |
|---|---|
| `{"accepted": true, "message": "policy updated", "min_cores": 2, "max_cores": 6, "priority_weight": 250}` | Applied. The response gives the resulting values. |
| `{"accepted": false, "message": "VM 17 not found"}` | No VM with that id. |
| `{"accepted": false, "message": "..."}` | The result would have `max_cores` = 0, `min_cores` > `max_cores`, or `max_cores` > `num_vcpus`. |

A `priority_weight` of 0 is stored as 1. Updates take effect in the
engine's next sweep and aren't persisted.

## Client helpers

The `pylond` library has Rust clients for these messages, used by its
tests: `send_registration`, `send_policy_update` and `query_vm_stats` in
`pylon/pylond/src/control.rs`.
