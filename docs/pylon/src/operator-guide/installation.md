# Building and installing

## Requirements

| | |
|---|---|
| Architecture | x86_64 or aarch64. `pylond` and the VMM count time in host counter ticks: the TSC on x86_64, the virtual counter `CNTVCT_EL0` on aarch64. Between sweeps `pylond` waits with `TPAUSE` (a `PAUSE` fallback on CPUs without `WAITPKG`) on x86_64, and with `WFET` on aarch64 CPUs that have `FEAT_WFxT` (a spin hint otherwise). See [aarch64](#aarch64). The VMM side builds on every architecture Cloud Hypervisor supports. |
| Hypervisor | KVM. Pylon hasn't been run on MSHV. |
| Rust | The workspace's MSRV, 1.89.0, or newer. |
| Build packages | Cloud Hypervisor's usual ones, plus `clang` (with the BPF target), `llvm`, `libelf-dev`, `zlib1g-dev` and `pkg-config`, for `scx_pylon`. |
| Kernel | Linux built with `CONFIG_SCHED_CLASS_EXT` (6.12 or later) to use `scx_pylon`. The default `--pylon park=kvm` needs it: without `scx_pylon`, `pylond` refuses such VMs. On other kernels, Pylon works with `park=vmm`, using the host's normal scheduler. |

On Debian or Ubuntu:

```bash
sudo apt-get install -y build-essential clang llvm libelf-dev zlib1g-dev pkg-config
```

## aarch64

Pylon runs on aarch64 KVM hosts with aarch64 Linux guests. What it needs
beyond the table above:

- **KVM only.** `--pylon` is rejected at start-up on MSHV on aarch64,
  which can read neither the guest's counter nor its timer.
- **`FEAT_S2FWB`, which every Armv8.4 or later core implements.**
  The guest agent maps the shared page through `/dev/mem`, which on arm64
  is device memory; stage 2 must force write-back for it to be coherent
  with `pylond`. This is a host requirement, not a runtime check: the
  kernel hides the ID register field, so Pylon can't test for it.
- **`FEAT_WFxT` (Armv8.7) is optional.** Without it `pylond` spins with a
  hint between sweeps instead of waiting, so the engine CPU stays fully
  busy at lower power savings; check `timed_wait=true` in its start-up
  log line.
- **The shared page is at guest address `0xFEDF_0000`**, 64 KiB aligned,
  in the 32-bit reserved hole, outside the FDT's memory node.
- **Heterogeneous CPUs.** On a host with big and little cores, the
  engine's core type made no measurable difference on the reference
  host (Cortex-X925 and Cortex-A725): guest throughput and wake latency
  were the same with `pylond --engine-cpu` on either. Keep the engine
  CPU out of `--cores`.
- **Idle states.** A parked vCPU's core idles, and leaving a deep idle
  state is most of a wake's cost. On the reference host, the p99 of a
  forced park's wake was 531 to 732 µs without a limit and 11 to 12 µs
  with the cores held out of deep idle (a PM QoS request of 0 on
  `/dev/cpu_dma_latency`). Holding a request costs idle power on the
  managed cores; whether to do it is the operator's choice, and
  `pylond` doesn't make one.

A booted aarch64 guest runs end to end, with `scx_pylon` and parking
inside KVM. The aarch64 numbers are in
[Benchmarks](../developer-guide/benchmarks.md).

## Download a release

Each [release](https://github.com/CMKL-APEX/pylon/releases) has these
files:

| File | What it is |
|---|---|
| `cloud-hypervisor`, `ch-remote` | The VMM and its client for x86_64, linked to glibc. |
| `cloud-hypervisor-static`, `ch-remote-static` | The same, static (musl), for x86_64. |
| `cloud-hypervisor-static-aarch64`, `ch-remote-static-aarch64` | The same, static, for aarch64. |
| `pylond`, `scx_pylon` | The host daemon and the BPF scheduler tool for x86_64. |
| `pylond-aarch64`, `scx_pylon-aarch64` | The same for aarch64. |
| `pylon-x86_64`, `pylond-x86_64`, `SHA256SUMS` | The VMM and `pylond` for x86_64, built on Ubuntu 24.04, with their SHA-256 checksums, for installers that pin them. |
| `pylon-guest-agent-static`, `pylon-guest-agent-static-aarch64` | The guest agent, static, to copy into Linux guests. |
| `pv-pylon-<version>.tar.gz` | The `pv-pylon` guest module's source. Build it against the guest's kernel headers. |
| `pylon-<version>.tar.xz` | The source, with its Rust dependencies vendored. |

`pylond` and `scx_pylon` are built on Ubuntu 24.04. They need glibc 2.39
or later, `libelf`, `zlib` and `zstd` (on Ubuntu, `libelf1t64`,
`zlib1g` and `libzstd1`). On other hosts, build from source.

Every file has a build provenance attestation, which shows that the
release workflow built it from the tagged source. Check a file before
installing it:

```bash
gh attestation verify pylond --repo CMKL-APEX/pylon
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

An example unit. Adjust the CPU list, and the socket group to a group
that only the users your VMMs run as belong to. Anyone who can use the
socket can register VMs ([The control socket](pylond.md#the-control-socket)):

```ini
# /etc/systemd/system/pylond.service
[Unit]
Description=Pylon core arbiter
Before=multi-user.target

[Service]
Type=simple
RuntimeDirectory=pylon
Environment=RUST_LOG=info
# Let only the group the VMMs run as (here: pylon-vmm) use the socket.
ExecStart=/usr/local/bin/pylond --socket /run/pylon/pylond.sock --cores 2-7 --socket-group pylon-vmm --socket-mode 660
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
VM's shared page through the `memfd` the VMM sends, so it needs no
access to files of its own for it.
<!-- TODO(verify): run pylond under a capability-limited unit and confirm
the minimum capability set. -->
