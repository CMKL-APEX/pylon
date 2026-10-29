# Metrics

With `--metrics-port`, `pylond` serves Prometheus text-format metrics at
`http://<metrics-addr>:<port>/metrics` (and at `/`). Every metric has one
label, `vm_id`, and one series per registered VM.

| Metric | Type | Meaning |
|---|---|---|
| `pylon_vm_allocated_cores` | gauge | vCPUs that currently have a managed CPU and aren't parked. |
| `pylon_vm_core_microseconds_total` | counter | Core-microseconds of managed CPU assigned to the VM. |
| `pylon_vm_cooperative_yields_total` | counter | Revocations completed because the donor parked within the grace period. |
| `pylon_vm_forced_preemptions_total` | counter | Revocations completed because the grace period ran out. |
| `pylon_vm_runnable_threads` | gauge | Runnable threads the guest last reported. |
| `pylon_vm_min_cores` | gauge | Current `min_cores`. |
| `pylon_vm_max_cores` | gauge | Current `max_cores`. |

Counters reset when `pylond` restarts or the VM registers again.

Example (abridged):

```text
# HELP pylon_vm_allocated_cores Current physical cores assigned to VM
# TYPE pylon_vm_allocated_cores gauge
pylon_vm_allocated_cores{vm_id="17"} 3
pylon_vm_core_microseconds_total{vm_id="17"} 81234567
...
```

The same values are available over the control socket's
[stats request](control-socket.md#stats).
