# Building and installing

## Requirements

| | |
|---|---|
| Architecture | x86_64. `pylond` and `scx_pylon` read the TSC and use `TPAUSE` (with a `PAUSE` fallback on CPUs without `WAITPKG`). The VMM side builds on every architecture Cloud Hypervisor supports. |
| Hypervisor | KVM. Pylon hasn't been run on MSHV. |
| Rust | The workspace's MSRV, 1.89.0, or newer. |
| Build packages | Cloud Hypervisor's usual ones, plus `clang` (with the BPF target), `llvm`, `libelf-dev`, `zlib1g-dev` and `pkg-config`, for `scx_pylon`. |
| Kernel (optional) | Linux built with `CONFIG_SCHED_CLASS_EXT` (6.12 or later) to use `scx_pylon`. Without it, Pylon still works, using the host's normal scheduler. |

On Debian or Ubuntu:

```bash
sudo apt-get install -y build-essential clang llvm libelf-dev zlib1g-dev pkg-config
```

## Build

```bash
git clone https://github.com/CMKL-APEX/pylon.git && cd pylon
cargo build --release -p cloud-hypervisor -p pylond
```

| Binary | Package | What it is |
|---|---|---|
| `target/release/cloud-hypervisor` | `cloud-hypervisor` | The VMM, with `--pylon`. |
| `target/release/pylond` | `pylond` | The core arbiter, with `scx_pylon` built in. |
| `target/release/scx_pylon` | `scx-pylon` | Optional: inspects and attaches the BPF scheduler on its own. Build with `-p scx-pylon`. |
| `target/release/pylon-guest-agent` | `pylon-guest-agent` | Optional: runs **inside** Linux guests. Build with `-p pylon-guest-agent`. |
| `target/release/pylon-bench` | `pylond` | Optional: the benchmark in [Performance and benchmarks](../developer-guide/benchmarks.md). |

Cross and musl builds of the workspace must leave out the host daemons:
`--exclude scx-pylon --exclude pylond`.

## Install

Pylon has no installer of its own. Copy the binaries where your service
manager expects them, for example:

```bash
sudo install -m 0755 target/release/cloud-hypervisor /usr/local/bin/
sudo install -m 0755 target/release/pylond /usr/local/bin/
```

## A systemd unit for pylond

An example unit. Adjust the CPU list, and the socket group to whatever
group your VMMs run as:

```ini
# /etc/systemd/system/pylond.service
[Unit]
Description=Pylon core arbiter
Before=multi-user.target

[Service]
Type=simple
RuntimeDirectory=pylon
Environment=RUST_LOG=info
# Let the group the VMMs run as (here: kvm) use the socket.
ExecStart=/usr/local/bin/pylond --socket /run/pylon/pylond.sock --cores 2-7 --socket-group kvm --socket-mode 660
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now pylond
```

Start `pylond` before any Pylon VM. A VM registers only once, when it
starts ([Running a Pylon VM](../user-guide/pylon-vms.md#what-happens-at-boot)).

Run `pylond` as root. Attaching `scx_pylon` needs at least `CAP_BPF`
and `CAP_SYS_ADMIN`, and creating the socket under `/run` needs write
access there. It reads each
VM's shared page through the file descriptor the VMM sends, so it
doesn't need access to `/dev/shm` paths itself.
<!-- TODO(verify): run pylond under a capability-limited unit and confirm
the minimum capability set. -->
