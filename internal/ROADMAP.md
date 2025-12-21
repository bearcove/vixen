# vx Roadmap

This document tracks implementation status. See [DESIGN.md](DESIGN.md) for the normative specification.

---

## v0 Implementation Checklist

### Core Infrastructure

- [x] Project structure with crate layout
- [x] Cargo.toml parsing via facet-toml
- [x] Basic picante database with inputs
- [x] Blake3 hashing for content
- [x] CAS blob storage (put/get)
- [ ] CAS manifest storage
- [ ] Cache key → manifest index

### Picante Inputs

- [x] `SourceFile` (path + content hash)
- [x] `CargoToml` (hash + name + edition + bin_path)
- [x] `BuildConfig` (profile + target + workspace)
- [ ] `RustcToolchain` (rustc path + `rustc -vV` output)

### Picante Queries

- [x] `cache_key_compile_bin` — explicit blake3 cache key
- [x] `plan_compile_bin` — rustc invocation builder
- [x] `node_id_compile_bin` — human-readable node ID

### Build Flow

- [x] CLI sends BuildRequest to daemon
- [x] Daemon parses Cargo.toml
- [x] Daemon discovers source files
- [x] Daemon computes cache key
- [x] Daemon checks cache hit/miss
- [x] On miss: invoke rustc
- [x] Store output in CAS
- [x] Materialize output to `.vx/build/`
- [ ] Write NodeManifest on completion
- [ ] Publish cache key → manifest mapping

### Cache Correctness

**Note:** Current caching uses in-memory picante memoization + ad-hoc file checks. v0 completion requires CAS-backed manifest/index for proper cross-session persistence.

- [x] No-op build = zero rustc invocations (within session)
- [x] Source change = cache miss
- [x] Profile change = cache miss
- [x] Edition change = cache miss
- [ ] Toolchain change = cache miss (needs RustcToolchain input)
- [ ] Different checkout path = still cache hit (--remap-path-prefix)
- [ ] Cross-session cache persistence (needs CAS manifest/index)

### Service Separation

- [x] `vx-daemon-proto` crate with Daemon trait
- [x] `vx-cas-proto` crate (types exist)
- [ ] `vx-exec-proto` crate
- [ ] vx-casd as separate binary
- [ ] vx-execd as separate binary
- [ ] vx-execd talks to CAS directly
- [ ] rapace transport between services

### CLI Commands

- [x] `vx build [--release]`
- [x] `vx kill` — stop daemon (no-op for v0, daemon runs in-process)
- [x] `vx clean` — stop daemon + wipe `.vx/`
- [ ] `vx explain` — show last build details

### Error Handling

- [x] Fail on workspaces
- [x] Fail on dependencies
- [ ] Fail on features
- [ ] Fail on build.rs
- [ ] Fail on proc-macros
- [ ] Fail on tests/benches/examples
- [ ] Fail on multiple targets

### Testing

Current state: 3 unit tests in vx-manifest. Nothing else.

- [ ] Unit tests for CAS operations (put/get blob, manifest storage)
- [ ] Unit tests for cache key computation (determinism, correct invalidation)
- [ ] Integration tests for full build flow:
  - [ ] Fresh build produces correct output
  - [ ] Second build is cache hit (zero rustc invocations)
  - [ ] Source change triggers rebuild
  - [ ] Profile change triggers rebuild
  - [ ] Edition change triggers rebuild
- [ ] Test fixtures: minimal single-crate projects
- [ ] CI: run tests on push

---

## Known Issues

- facet-toml doesn't handle `Option<Vec<T>>` correctly (https://github.com/facet-rs/facet/issues/1341)
- `[[bin]]` array parsing needs the above fix

---

## Future (post-v0)

These are explicitly out of scope for v0:

- Workspaces
- Dependencies (crates.io, git, path)
- Features
- Build scripts
- Proc macros
- Tests / benches / examples
- Multiple targets per crate
- Incremental compilation
- Remote execution
- CI integration

### Toolchain Management (post-v0)

v0 uses system rustc (honor `RUSTC` env var or find in PATH).

Future: vx owns the toolchain completely:
- Read `rust-toolchain.toml` or default to a pinned version
- Download toolchain from rustup if not present
- Store toolchain in CAS (it's just bytes)
- Toolchain hash becomes part of cache key
- No dependency on system rustc
