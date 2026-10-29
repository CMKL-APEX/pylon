# Inside the guest

Pylon needs no changes to the guest to boot it. Without a guest
component, though, `pylond` has no demand signal. Its vCPUs below
`initial_active_vcpus` run, and the parked ones stay parked. This page
explains what the guest sees and how it reports demand.

## What the guest sees

- **All vCPUs are online.** A parked vCPU is still online in the guest.
  It just isn't scheduled on the host, the way a vCPU of an overcommitted
  host sometimes isn't. The guest's scheduler may put work on it; that
  work waits until `pylond` wakes the vCPU.
- **The shared page**, 8 KiB at guest-physical address `0xFEDF_0000`
  (or the `gpa` you set). It starts with the header: magic
  `0x5059_4C4F_4E56_4D31` (`"PYLONVM1"`), ABI version `1`, the number of
  vCPU slots and the VM's id. See [Shared-memory
  ABI](../developer-guide/shared-memory-abi.md) for the layout.

## Reporting demand: pylon-guest-agent

`pylon-guest-agent` is a small userspace daemon for Linux guests. It
maps the shared page through `/dev/mem` and, every interval (50 ms by
default):

- writes the number of runnable threads, from the fourth field of
  `/proc/loadavg` (or `procs_running` in `/proc/stat`), to
  `runnable_threads`;
- increments `guest_heartbeat`;
- when there are more runnable threads than running vCPUs, writes the
  current TSC to `head_wait_tsc`, and clears it when one thread or
  fewer is runnable.

Build it on the host and copy it into the guest:

```bash
cargo build --release -p pylon-guest-agent
# in the guest, as root:
./pylon-guest-agent                       # run, sampling every 50 ms
./pylon-guest-agent --interval-us 10000   # sample every 10 ms
./pylon-guest-agent --status              # print the page and exit
```

It needs root, and a guest kernel that allows `/dev/mem` mappings of the
page's address. All its options are in [Other
tools](../reference/tools.md#pylon-guest-agent).

> **Warning.** The agent also tries to "yield" vCPUs that `pylond` asks
> to give up a core, by marking them parked in the page. It can't
> actually stop those vCPUs, so it misreports their state to `pylond`.
> This is a known problem ([Status](../status.md#what-doesnt-work-yet)).
> The fix makes the agent telemetry-only.

## Limits of a userspace agent

The agent samples at a fixed interval, so demand reaches `pylond`
milliseconds late. Pylon's microsecond handoff needs the signal sooner
than that. A userspace process also can't see kernel critical sections,
can't react to a revocation on the vCPU being revoked, and can't run at
all when every vCPU is parked. The planned `pv-pylon` guest kernel
module covers these ([Enhancements](../developer-guide/enhancements.md)).
