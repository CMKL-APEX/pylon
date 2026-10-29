# Pylon documentation

The Pylon book covers Pylon, the core-elasticity subsystem in this Cloud
Hypervisor fork: what it does, how to run it, and how it is designed and
built. It's built with mdBook and published together with a landing page.

| Part | Source | Published at |
|---|---|---|
| **Book** | [`docs/pylon/src/`](src/SUMMARY.md) (config in [`book.toml`](book.toml)) | <https://cmkl-apex.github.io/pylon/docs/> |
| **Landing page** | [`docs/pylon/site/`](site/) | <https://cmkl-apex.github.io/pylon/> |
| **Design specs** | [`spec/`](../../spec/) | Not published; read them in the repository |

Upstream Cloud Hypervisor's own documentation stays in `docs/*.md` and
isn't part of this book.

## What goes where

- **User and operator guides** describe what Pylon does today and how to
  use it: the `--pylon` option, `pylond`, `scx_pylon`, monitoring. They
  describe shipped behavior only. Where something doesn't work yet, they
  say so and link to [Status and roadmap](src/status.md).
- **The developer guide** explains the architecture, the shared-memory
  ABI, the arbitration engine, design decisions, planned enhancements
  and benchmark figures. It's the one part of the book that points into
  the specs and source code. It always does so with full GitHub URLs
  (`https://github.com/CMKL-APEX/pylon/blob/main/...`), never relative
  links, so the links still work on the published site.
- **Specs** in `spec/` stay the authority on *why*. They keep their paths
  because code and other specs cite them as `spec/<file>.md`. When a
  spec's design ships, write the user-facing part here and link to the
  spec from the developer guide.
- **Benchmark figures** always name their source, date and hardware, and
  say whether they were measured or modelled.

## Building

```sh
cargo install mdbook --version 0.5.4 --locked
mdbook build docs/pylon     # -> target/docs/pylon
mdbook serve docs/pylon     # live preview on http://localhost:3000
```

CI builds the book on every pull request that touches it
([`.github/workflows/pylon-docs.yaml`](../../.github/workflows/pylon-docs.yaml))
and keeps the site as a workflow artifact. On every push to `main`, it
publishes the site to the `gh-pages` branch: the landing page from
[`site/`](site/) at the root and the book under `docs/`.
GitHub Pages must be set to serve the `gh-pages` branch (repository
**Settings → Pages → Build and deployment → Deploy from a branch**).
