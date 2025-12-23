# vixen

A Rust build engine with correct caching. The CLI is `vx`.

## What it does today

Builds single-crate Rust projects with no dependencies:

```bash
vx build           # debug build
vx build --release # release build
vx clean           # remove .vx/ directory
```

Outputs go to `.vx/build/` (not `target/`).

## Caching

vertex uses content-addressed storage and picante (incremental queries) to cache builds correctly:

- Second build with unchanged inputs = instant (zero rustc invocations)
- Change source, edition, or profile = rebuild
- Different checkout path = still a cache hit (`--remap-path-prefix`)
- Cache persists across sessions

Global cache lives at `~/.vx/` (or `$VX_HOME`).

## Limitations (v0)

vertex explicitly rejects (with clear errors):

- Workspaces
- Dependencies
- Features
- Build scripts (`build.rs`)
- Proc macros
- Tests / benches / examples
- Multiple binary targets

## Debug logging

```bash
RUST_LOG=vx_daemon=debug vx build
```
