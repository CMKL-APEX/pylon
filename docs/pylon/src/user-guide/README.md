# User guide

This guide is for people who start VMs with Pylon: what the `--pylon`
option does, how to pick its values, and what changes inside the guest.

It assumes a host where `pylond` already runs. To set one up, read the
[Operator guide](../operator-guide/README.md).

| Page | What it covers |
|---|---|
| [Running a Pylon VM](pylon-vms.md) | The `--pylon` option, choosing vCPU counts and policy, and what happens at boot and shutdown. |
| [Inside the guest](guest.md) | What the guest sees, how it reports demand, and `pylon-guest-agent`. |

If you use Glidex, you don't write `--pylon` yourself: Glidex builds it
from the VM's elastic CPU settings. See Glidex's
[virtual machines guide](https://cmkl-apex.github.io/glidex/docs/user-guide/vms.html#elastic-cpu).
