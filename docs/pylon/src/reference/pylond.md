# pylond

```text
pylond [--socket <path>] [--cores <list>] [--enable-scx | --no-scx]
       [--pcpu-map-file <path>] [--metrics-port <port>] [--metrics-addr <addr>]
```

| Option | Default | Meaning |
|---|---|---|
| `--socket <path>` | `/run/pylon/pylond.sock` | Control socket. Its parent directory is created if needed, and an existing file at the path is removed first. |
| `--cores <list>` | `2-7` | Managed host CPUs: a comma-separated list of CPUs and ranges, such as `2-7` or `4-15,20-31`. Must not include 0 or 1, which `pylond` uses itself. |
| `--enable-scx` | off | Attach `scx_pylon`, or exit with an error if it can't be attached. Conflicts with `--no-scx`. |
| `--no-scx` | off | Never attach `scx_pylon`. |
| `--pcpu-map-file <path>` | none | Keep the pCPU owner table in this existing file instead of in memory. Turns off automatic `scx_pylon` attachment. |
| `--metrics-port <port>` | none | Serve Prometheus metrics over HTTP on this port. |
| `--metrics-addr <addr>` | `127.0.0.1` | Address for the metrics server. |
| `--version`, `--help` | | |

Without `--enable-scx`, `--no-scx` or `--pcpu-map-file`, `pylond`
attaches `scx_pylon` when `/sys/kernel/sched_ext` exists. If that
fails, it logs a warning and continues.

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
