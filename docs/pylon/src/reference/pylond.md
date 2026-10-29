# pylond

```text
pylond [--socket <path>] [--socket-group <group>] [--socket-mode <mode>]
       [--cores <list>] [--enable-scx | --no-scx]
       [--core-sched-guard <auto|on|off>] [--core-sched-deny <eperm|enodev|off>]
       [--pcpu-map-file <path>] [--metrics-port <port>] [--metrics-addr <addr>]
```

| Option | Default | Meaning |
|---|---|---|
| `--socket <path>` | `/run/pylon/pylond.sock` | Control socket. Its parent directory is created if needed, and an existing file at the path is removed first. |
| `--socket-group <group>` | `pylond`'s group | Group that owns the socket: a name or a numeric GID. |
| `--socket-mode <mode>` | `pylond`'s umask | The socket's permission bits, in octal, such as `660` or `0o660`. VMMs need write permission to register. |
| `--cores <list>` | `2-7` | Managed host CPUs: a comma-separated list of CPUs and ranges, such as `2-7` or `4-15,20-31`. Must not include 0 or 1, which `pylond` uses itself. |
| `--enable-scx` | off | Attach `scx_pylon`, or exit with an error if it can't be attached. Conflicts with `--no-scx`. |
| `--no-scx` | off | Never attach `scx_pylon`. |
| `--core-sched-guard <mode>` | `auto` | Keep `scx_pylon` away from kernel core scheduling: `auto` on kernels that lack the fix for issue #15, `on` on every kernel that can core-schedule, `off` never ([Core scheduling](../operator-guide/sched-ext.md#core-scheduling)). |
| `--core-sched-deny <answer>` | `eperm` | With the guard, what a process asking for a core scheduling cookie gets while `scx_pylon` is attached: `eperm`, `enodev`, or `off` to let it through and disable `scx_pylon`. `eperm` and `enodev` need the bpf LSM. |
| `--pcpu-map-file <path>` | none | Keep the pCPU owner table in this existing file instead of in memory. Turns off automatic `scx_pylon` attachment. |
| `--metrics-port <port>` | none | Serve Prometheus metrics over HTTP on this port. |
| `--metrics-addr <addr>` | `127.0.0.1` | Address for the metrics server. |
| `--version`, `--help` | | |

Without `--enable-scx`, `--no-scx` or `--pcpu-map-file`, `pylond`
attaches `scx_pylon` when `/sys/kernel/sched_ext` exists. If that
fails, it logs a warning and continues.

## Startup

At start, `pylond` measures the host's TSC rate against the system clock
for 50 ms and logs `Host TSC rate: <n> kHz`. It uses that rate to turn
TSC cycles into time for every VM whose shared-page header gives none,
which the VMM's doesn't. If it can't measure it, it assumes 3 GHz and
says so.

## Environment

| Variable | Meaning |
|---|---|
| `RUST_LOG` | Log level for `env_logger` (`error`, `warn`, `info`, `debug`, `trace`). Logs go to standard error. |

## Threads

| Thread | CPU | Role |
|---|---|---|
| main (Tokio, single-threaded) | 0 | Control plane and metrics server |
| `pylond-engine` | 1 | Arbitration engine |

## Exit

`pylond` exits on `SIGTERM` or `SIGINT` (default signal handling), or
when the control plane fails, for example if it can't bind the socket.
`scx_pylon`, if attached, is detached by the kernel when the process
exits.
