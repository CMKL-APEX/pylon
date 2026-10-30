# Quick start

This page builds Pylon from source, starts the `pylond` arbiter and
boots a VM with eight vCPUs, two of them running.

You need an x86_64 or aarch64 Linux host with KVM, and a user that can
read and write `/dev/kvm`. On aarch64, read the
[aarch64 requirements](operator-guide/installation.md#aarch64) first.

## 1. Build

Install Cloud Hypervisor's usual build dependencies, plus `clang`,
`libelf` and `zlib` for the `scx_pylon` BPF object:

```bash
sudo apt-get install -y build-essential clang llvm libelf-dev zlib1g-dev pkg-config
git clone https://github.com/CMKL-APEX/pylon.git && cd pylon
cargo build --release -p cloud-hypervisor -p pylond
```

This produces `target/release/cloud-hypervisor` and
`target/release/pylond`.

## 2. Start pylond

`pylond` arbitrates the host CPUs you give it with `--cores`, and uses
host cores 0 and 1 for itself. Keep those three sets apart: don't list
0 or 1 in `--cores`. Core 1 is where the polling engine runs; move it
with `--engine-cpu` (on a host with big and little cores, pick a big
one) and keep it out of `--cores` too.

```bash
sudo ./target/release/pylond --socket /run/pylon/pylond.sock --cores 2-7 \
    --socket-group "$(id -gn)" --socket-mode 660
```

`--socket-group` and `--socket-mode` let your own group, and so your
non-root VMM, use the socket. Without them, the socket keeps root's
umask (`0755`), and only root can connect.

On a kernel with `sched_ext`, `pylond` also attaches the `scx_pylon`
scheduler. **That scheduler then schedules every task on the host, not
only VMs,** until `pylond` exits. Add `--no-scx` to keep the host's
normal scheduler (see [Kernel scheduling with
scx_pylon](operator-guide/sched-ext.md)).

## 3. Boot a Pylon VM

Add `--pylon` to an ordinary Cloud Hypervisor command line:

```bash
./target/release/cloud-hypervisor \
    --kernel vmlinux \
    --disk path=focal.raw \
    --cmdline "console=ttyS0 root=/dev/vda1 rw" \
    --cpus boot=8 \
    --memory size=2G \
    --serial tty --console off \
    --pylon shm=/dev/shm/vm1-pylon,socket=/run/pylon/pylond.sock,vm_id=1,initial_active_vcpus=2,min_cores=1,max_cores=8
```

The VM boots with eight vCPUs. vCPUs 0 and 1 run; vCPUs 2–7 start
parked. The VM registers with `pylond`, which can then wake parked vCPUs
when there's demand.

**Give every VM its own `vm_id`.** It defaults to `1`, and `pylond`
rejects a VM whose id is already registered by a running VMM.

## 4. Look at it

Ask `pylond` for its view of the VM over its socket:

```bash
echo '{"action":"stats"}' | sudo socat - UNIX-CONNECT:/run/pylon/pylond.sock
```

The reply lists every registered VM with its current cores, policy and
counters. To export them as Prometheus metrics, start `pylond` with
`--metrics-port 9100` and fetch `http://127.0.0.1:9100/metrics`. See
[Policy and monitoring](operator-guide/monitoring.md).

## Next steps

- What `--pylon` options mean: [Running a Pylon VM](user-guide/pylon-vms.md)
  and [the option reference](reference/pylon-option.md).
- What the guest needs to report demand: [Inside the guest](user-guide/guest.md).
- What doesn't work yet: [Status and roadmap](status.md).
