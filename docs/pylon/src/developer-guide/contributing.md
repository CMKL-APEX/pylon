# Contributing

The project-wide rules are in
[`CONTRIBUTING.md`](https://github.com/CMKL-APEX/pylon/blob/main/CONTRIBUTING.md)
and [`AGENTS.md`](https://github.com/CMKL-APEX/pylon/blob/main/AGENTS.md).
This page adds what's specific to Pylon.

## Where changes go

All work happens in `CMKL-APEX/pylon`. **Don't push branches, open pull
requests or file issues against upstream
`cloud-hypervisor/cloud-hypervisor`** unless the project owner asks for
it. Pylon isn't meant for upstream, and upstream changes reach Pylon by
merging `upstream/main`.

## Building

```bash
cargo build -p pylon-abi -p pylond -p scx-pylon -p pylon-guest-agent
cargo build -p cloud-hypervisor --features kvm
```

`scx-pylon` (and `pylond`, which depends on it) need `clang` with the
BPF target, `libelf` and `zlib`. If a VMM build fails in a way that
looks feature-related, retry with `--features kvm`.

## Tests

| Command | Covers | Needs |
|---|---|---|
| `cargo test -p pylon-abi` | Layout | — |
| `cargo test -p pylond` | Engine, control plane, `scx_attach` policy | — |
| `cargo test -p pylond --test e2e_pylon` | End to end with simulated VMs: scale-out, revocation, deadlines, zero-core wakeup, policy, stats, metrics | — |
| `cargo test -p scx-pylon` | BPF object and `PcpuOwnerMap` | `clang` |
| `sudo -E cargo test -p scx-pylon -- --ignored` | Live `sched_ext` attach | Root, a `sched_ext` kernel |
| `cargo test -p pylon-guest-agent` | `/proc` parsing, telemetry | — |
| `cargo test -p vmm --features kvm --lib pylon` | VMM side, including a real KVM guest | `/dev/kvm` |
| `cargo test -p vmm --features kvm --lib config` and `cargo test -p cloud-hypervisor --features kvm --bin cloud-hypervisor` | `--pylon` parsing in context | — |

Add a unit test with every bug fix, and with any non-trivial logic in
the engine. The engine takes time as an argument (`poll_once(now_tsc)`),
so tests don't need real time.

## Style

- `cargo +nightly fmt --all` (formatting needs nightly rustfmt
  options).
- `cargo clippy --all-targets -- -D warnings` for the crates you touch.
- Prefer safe Rust. Every `unsafe` block gets a `// SAFETY:` comment
  that states the invariants it relies on.
- Errors use `thiserror`. Messages start with a capital letter. Don't
  repeat a `#[source]` error's text in the outer message.
- The engine's loop must stay free of heap allocation, locks and
  blocking calls. Reviewers check this for every change to
  `engine.rs`.

## Changing a design

1. Change the spec in `spec/` first. For a new design, add a numbered
   spec (`spec/07-….md`). Keep existing spec paths, because code and
   other specs cite them.
2. Update the status tracker,
   [`spec/05-implementation-status.md`](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md),
   in the same pull request as the code that changes the status.
3. Update this book: the user or operator pages for anything that
   changes what users see, and the developer guide for anything that
   changes how it works. [Status and roadmap](../status.md) must match
   the tracker.

## Commits

Commit messages follow `CONTRIBUTING.md`: a component prefix, a subject
and body within 72 columns, and a `Signed-off-by` trailer. Pylon's
prefixes include `pylon`, `pylond`, `scx-pylon`, `pylon-abi`, `spec`,
`docs` and `ci`; the full list is in
[`scripts/gitlint/rules/TitleStartsWithComponent.py`](https://github.com/CMKL-APEX/pylon/blob/main/scripts/gitlint/rules/TitleStartsWithComponent.py).

If an LLM contributed meaningfully to a change, say so in the pull
request description (`CONTRIBUTING.md`, "Contributions assisted by
LLMs").

## CI

| Workflow | Runs |
|---|---|
| `pylon-ci.yaml` | Pylon's crates: tests, clippy, the VMM's Pylon tests, release builds and `pylon-bench --quick`. |
| `ci.yaml` | Upstream's workspace checks: formatting, clippy for every hypervisor and feature combination, builds across targets, unit tests. Cross and musl jobs exclude `scx-pylon` and `pylond`. |
| `pylon-docs.yaml` | Builds this book on pull requests and publishes it on `main`. |

Jobs that need upstream's self-hosted runners are disabled in this
repository. The current coverage is in the status tracker's
[CI coverage](https://github.com/CMKL-APEX/pylon/blob/main/spec/05-implementation-status.md#ci-coverage)
section.

## Documentation

Build and preview this book with:

```bash
mdbook serve docs/pylon     # http://localhost:3000
```

Rules for what goes where are in
`docs/pylon/README.md` in the repository.
In short: user and operator pages describe shipped behavior only,
developer pages may link into specs and code with full GitHub URLs,
and every benchmark figure names its date, hardware and whether it was
measured or modelled.
