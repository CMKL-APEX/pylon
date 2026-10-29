# The --pylon option

```text
cloud-hypervisor ... --pylon shm=<path>[,socket=<path>,gpa=<addr>,initial_active_vcpus=<n>,min_cores=<n>,max_cores=<n>,priority_weight=<n>,vm_id=<id>]
```

Comma-separated `key=value` pairs, parsed like Cloud Hypervisor's other
device options. An unknown key is an error.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `shm` | path | **required** | File backing the 8 KiB shared page, for example `/dev/shm/vm17-pylon`. Created if missing (with its parent directories), resized to 8,192 bytes and zeroed at start. Not removed at exit. One file per VM. |
| `socket` | path | none | `pylond`'s control socket, such as `/run/pylon/pylond.sock`. Without it, the VM never registers, and parked vCPUs are never woken. |
| `gpa` | integer, `0x` hex or decimal | `0xfedf0000` | Guest-physical address of the page. Must be a multiple of 4,096. |
| `initial_active_vcpus` | integer | `--cpus boot` | vCPUs `0 … n-1` run from the start; the others start parked. Capped at the number of slots. |
| `min_cores` | integer | `1` | Cores `pylond` tries to give the VM before any VM above its minimum, whenever it reports demand. |
| `max_cores` | integer | number of slots | Most cores `pylond` gives the VM. |
| `priority_weight` | integer | `100` | Multiplies the VM's urgency. Only ratios between VMs matter. |
| `vm_id` | integer | `1` | The VM's id in the page's header and in `pylond`. Must be unique among VMs registered with one `pylond`. |

The number of vCPU slots is `max(--cpus boot, --cpus max)`, at most 64.
More vCPUs than that is an error.

## Errors

| Message | Cause |
|---|---|
| `Error parsing --pylon: shm path missing` | No `shm`, or it's empty. |
| `Error parsing --pylon: invalid GPA <value>` | `gpa` isn't a number, or isn't 4 KiB-aligned. |
| `Error parsing --pylon options` | An unknown key, or a value that isn't a number. |
| `Invalid Pylon vCPU count <n> (must be between 1 and 64)` | Too many vCPUs. |
| `Failed to open Pylon shared memory file` (and others about the file) | The `shm` path can't be created, opened, resized or mapped. |

Registration failures aren't errors: the VM boots and the VMM logs a
warning ([Troubleshooting](../operator-guide/troubleshooting.md)).

## Example

```text
--cpus boot=8 \
--pylon shm=/dev/shm/vm17-pylon,socket=/run/pylon/pylond.sock,vm_id=17,initial_active_vcpus=2,min_cores=1,max_cores=6,priority_weight=150
```
