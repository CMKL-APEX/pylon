# The --pylon option

```text
cloud-hypervisor ... --pylon [socket=<path>,gpa=<addr>,initial_active_vcpus=<n>,min_cores=<n>,max_cores=<n>,priority_weight=<n>,vm_id=<id>,park=kvm|vmm]
```

Comma-separated `key=value` pairs, parsed like Cloud Hypervisor's other
device options. An unknown key is an error. Every key is optional;
`--pylon ""` turns Pylon on with the defaults.

The VMM keeps the shared page and the host page after it in a `memfd`
of 16,384 bytes whose size is sealed. It has no path, so no other user
can open, replace or truncate it. Only the shared page is mapped into
the guest. Earlier versions took the file's path as `shm=<path>`; that
key is now an error.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `socket` | path | none | `pylond`'s control socket, such as `/run/pylon/pylond.sock`. Without it, the VM never registers, and parked vCPUs are never woken. The VMM registers once, at start, and doesn't retry: if the socket doesn't exist yet, it logs that and runs unregistered. |
| `gpa` | integer, `0x` hex or decimal | `0xfedf0000` | Guest-physical address of the page. Must be a multiple of 4,096. |
| `initial_active_vcpus` | integer | `--cpus boot` | vCPUs `0 … n-1` always run; the others run during boot and are parked once the guest reports it is up (first `guest_heartbeat`). Capped at the number of slots. |
| `min_cores` | integer | `1` | Cores `pylond` tries to give the VM before any VM above its minimum, whenever it reports demand. It isn't a reservation: an idle VM holds no cores. |
| `max_cores` | integer | number of slots | Most cores `pylond` gives the VM. `pylond` rejects the registration if `max_cores` is 0, `min_cores` is above it, or it's above the number of slots. |
| `priority_weight` | integer | `100` | Multiplies the VM's urgency. Only ratios between VMs matter. `pylond` stores 0 as 1. |
| `vm_id` | integer | `1` | The VM's id in the page's header and in `pylond`. Must be unique among VMs registered with one `pylond`. |
| `park` | `kvm` or `vmm` | `kvm` on x86_64 when `/dev/kvm` exists, else `vmm` | Where a vCPU without a core waits. `kvm` halts it inside `KVM_RUN`, so an interrupt wakes it; the VMM also turns KVM's halt polling off for the VM and tells `pylond` to wake vCPUs with a signal. It needs `pylond` with `scx_pylon`. `vmm` blocks the vCPU thread in the VMM until `pylond` assigns a core; use it with a guest that runs `pv-pylon` on a host without `scx_pylon`. See [VMM integration](../developer-guide/vmm-integration.md#parking-inside-kvm) and [spec 08](https://github.com/CMKL-APEX/pylon/blob/main/spec/08-park-in-kvm.md). |

The number of vCPU slots is `max(--cpus boot, --cpus max)`, at most 64.
More vCPUs than that is an error.

## Errors

| Message | Cause |
|---|---|
| `Error parsing --pylon: shm was removed; ...` | The command line still has `shm=<path>`. Drop it. |
| `Error parsing --pylon: invalid GPA <value>` | `gpa` isn't a number, or isn't 4 KiB-aligned. |
| `Error parsing --pylon options` | An unknown key, a value that isn't a number, or a `park` other than `kvm` or `vmm`. |
| `--pylon park=kvm needs KVM; use park=vmm` | `park=kvm` on MSHV. |
| `Invalid Pylon vCPU count <n> (must be between 1 and 64)` | Too many vCPUs. |
| `--pylon is not supported on MSHV on aarch64` | The MSHV backend on aarch64 can't read the guest's counter and timer. |
| `Failed to create Pylon shared memory file` (and others about the file) | The `memfd` can't be created, resized, sealed or mapped. |

Registration failures aren't errors: the VM boots and the VMM logs a
warning ([Troubleshooting](../operator-guide/troubleshooting.md)).

## Example

```text
--cpus boot=8 \
--pylon socket=/run/pylon/pylond.sock,vm_id=17,initial_active_vcpus=2,min_cores=1,max_cores=6,priority_weight=150
```
