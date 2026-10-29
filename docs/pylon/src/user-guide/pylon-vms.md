# Running a Pylon VM

A Pylon VM is an ordinary Cloud Hypervisor VM started with one more
option, `--pylon`. This page explains what to put in it and what
happens when the VM starts and stops. Every key is listed in
[The --pylon option](../reference/pylon-option.md).

## The option

```text
--pylon shm=<path>[,socket=<path>,gpa=<addr>,initial_active_vcpus=<n>,min_cores=<n>,max_cores=<n>,priority_weight=<n>,vm_id=<id>]
```

Only `shm` is required. A typical VM on a host running `pylond`:

```text
--cpus boot=8 \
--pylon shm=/dev/shm/web1-pylon,socket=/run/pylon/pylond.sock,vm_id=17,initial_active_vcpus=2,min_cores=1,max_cores=8,priority_weight=200
```

## Choosing the numbers

**vCPUs.** Pylon manages `max(boot, max)` vCPUs from `--cpus`, and at
most 64. Every one of them gets a host thread at boot, and the guest
sees all of them online. Pylon then decides which of them actually run.
Give the VM as many vCPUs as it could ever use; the parked ones cost a
thread each and no CPU time.

**`initial_active_vcpus`** is how many vCPUs run from the start: vCPUs
`0` to `n-1`. The rest start parked. It defaults to the `boot` count,
so without it every vCPU starts running. Keep at least `1`, because the
guest boots on vCPU 0.

**`min_cores` and `max_cores`** bound how many host CPUs `pylond` gives
the VM. `max_cores` defaults to the number of vCPUs. `min_cores`
defaults to `1`. When the VM has runnable work, `pylond` raises it to
`min_cores` first, before giving cores to VMs above their minimum.
Today a VM that reports no runnable threads gets no cores, whatever its
`min_cores` ([Status](../status.md#what-doesnt-work-yet)).

**`priority_weight`** (default `100`) multiplies the VM's urgency.
Between two VMs whose oldest runnable thread has waited equally long,
the one with twice the weight wins. It's relative: only the ratio
between VMs matters.

**`vm_id`** names the VM to `pylond`. **Set it, and keep it unique on
the host.** It defaults to `1`. While a VM's VMM runs, `pylond`
rejects any other VM that registers with the same id, and logs it. The
rejected VM boots, but `pylond` never manages it.

**`shm`** is the file backing the VM's shared page. Use a path under
`/dev/shm`, one per VM. The VMM creates the file if it doesn't exist
and zeroes it at start.

**`gpa`** is where the guest finds the page (default `0xFEDF_0000`). It
must be 4 KiB-aligned. Change it only together with the guest side,
which looks there by default.

## What happens at boot

1. While parsing its options, the VMM creates and zeroes the shared
   page, writes the header (magic, ABI version, vCPU count, `vm_id`),
   and creates two `eventfd`s per vCPU.
2. If `socket` is given and exists, the VMM registers the VM with
   `pylond`, passing the shared page and the `eventfd`s. If the socket
   doesn't exist yet, the VM boots without registering; if connecting
   fails, the VMM logs a warning and boots anyway. **It doesn't retry**,
   so start `pylond` before the VMs.
3. The VMM maps the page into guest memory at `gpa`.
4. Each vCPU thread records its host thread ID in its slot. vCPUs below
   `initial_active_vcpus` go straight to `RUNNING`. The others park on
   their wake `eventfd` before entering the guest.

From then on, `pylond` wakes parked vCPUs when the guest reports
demand and a managed CPU is free.

## What happens at shutdown

When the VM shuts down, the VMM wakes every parked vCPU thread so that
it can exit. The shared-page file is **not** removed. Delete it
yourself, or reuse the path for the same VM next time: the VMM zeroes
it at start.

`pylond` holds a pidfd for each VM's VMM. Within about a second of the
VMM exiting, it removes the VM's registration and frees the managed
CPUs the VM held. The VM's `vm_id` is then free for its next start.

## Things to avoid

- **Snapshot, restore and live migration.** Pylon state isn't saved or
  moved, and the VMM doesn't refuse these operations.
- **vCPU hotplug** (`ch-remote resize`). It isn't coordinated with
  Pylon.
- **Two VMs on one `shm` path.** Each would zero the other's page.
