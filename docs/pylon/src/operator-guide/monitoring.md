# Policy and monitoring

`pylond` answers policy changes and stats requests on its control
socket, and can serve Prometheus metrics over HTTP. The message formats
are in [Control socket protocol](../reference/control-socket.md) and
the metrics in [Metrics](../reference/metrics.md).

The examples use `socat`. Any client that writes one JSON object to the
socket and reads one back works.

## Changing a VM's policy

Send the VM's id and the fields to change. Fields you leave out keep
their value.

```bash
echo '{"vm_id":17,"max_cores":4,"priority_weight":300}' \
  | socat - UNIX-CONNECT:/run/pylon/pylond.sock
```

```json
{"accepted":true,"message":"policy updated","min_cores":1,"max_cores":4,"priority_weight":300}
```

The new values apply from the engine's next sweep, within microseconds.
Lowering `max_cores` below what a VM holds doesn't take cores away
immediately. The VM gives them back as its vCPUs park, or when another VM
needs them.

`pylond` rejects an update (`"accepted": false`) for a VM it doesn't
know, and one that would leave `max_cores` at `0`, `min_cores` above
`max_cores`, or `max_cores` above the VM's vCPU count. A
`priority_weight` of `0` is raised to `1`.

Policy changes aren't saved. A VM that restarts registers again with
the values from its `--pylon` option.

## Reading stats

```bash
echo '{"action":"stats"}' | socat - UNIX-CONNECT:/run/pylon/pylond.sock
echo '{"action":"stats","vm_id":17}' | socat - UNIX-CONNECT:/run/pylon/pylond.sock
```

```json
{
  "accepted": true,
  "message": "stats retrieved",
  "vms": [
    {
      "vm_id": 17, "num_vcpus": 8,
      "min_cores": 1, "max_cores": 4, "priority_weight": 300,
      "current_cores": 3,
      "allocated_core_microseconds": 81234567,
      "cooperative_yields": 12, "forced_preemptions": 4,
      "runnable_threads": 5,
      "handoffs_without_park": 0
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `current_cores` | vCPUs that have a managed CPU and aren't parked. |
| `allocated_core_microseconds` | Total time managed CPUs have been assigned to this VM, in core-microseconds, since it registered. |
| `cooperative_yields` | Revocations where the donor vCPU parked within the grace period. |
| `forced_preemptions` | Revocations where the grace period ran out. `pylond` then signals the donor's vCPU thread so the VMM parks it. |
| `handoffs_without_park` | Revocations where the donor still hadn't parked 50 µs after it was first signalled, so the core moved anyway. Should stay at 0. |
| `runnable_threads` | What the guest last reported. `0` if nothing in the guest reports it. |

Counters start at zero when the VM registers and are lost when `pylond`
restarts.

## Prometheus metrics

Start `pylond` with `--metrics-port` to serve metrics over HTTP:

```bash
pylond --cores 2-7 --metrics-port 9100   # listens on 127.0.0.1:9100
curl -s http://127.0.0.1:9100/metrics
```

It listens on `127.0.0.1` unless you set `--metrics-addr`. The endpoint
has no authentication. If you bind it to another address, restrict who
can reach it with a firewall.

A minimal scrape configuration:

```yaml
scrape_configs:
  - job_name: pylond
    static_configs:
      - targets: ["127.0.0.1:9100"]
```

Useful queries:

| Question | PromQL |
|---|---|
| Cores each VM uses now | `pylon_vm_allocated_cores` |
| Average cores per VM over 5 minutes | `rate(pylon_vm_core_microseconds_total[5m]) / 1e6` |
| p99 revocation-to-park latency per VM | `histogram_quantile(0.99, sum by (vm_id, le) (rate(pylon_vm_park_latency_microseconds_bucket[5m])))` |
| Cores handed over without a park | `increase(pylon_vm_handoffs_without_park_total[1h])` |
| Share of revocations that were forced | `rate(pylon_vm_forced_preemptions_total[5m]) / (rate(pylon_vm_forced_preemptions_total[5m]) + rate(pylon_vm_cooperative_yields_total[5m]))` |

While cooperative revocation is incomplete
([Status](../status.md#what-doesnt-work-yet)), expect the forced share
to be high under contention.
