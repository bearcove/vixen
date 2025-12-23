# Integration Plan: “vx can build Rust projects” + Parallel-Safe Tests

This document is the execution plan to get `vx build` reliably building real Rust projects again and to make the `crates/vx/tests/*` integration suite stable under **cargo-nextest** with **network enabled** and **parallel execution**.

## Constraints

- Network: **yes**
- Parallel: **yes** (via `cargo nextest`)
- Services are **local-only** (`127.0.0.1`) and communicate via rapace over TCP.

## Current Blockers (Observed)

### 1) Integration tests are not parallel-safe

Even after correctness fixes, the test suite will be flaky under `nextest` unless we address:

- **Fixed ports** (`127.0.0.1:9001-9003`) shared by all tests.
- **Leaked daemons**: `vx build` auto-spawns `vx-daemon` which spawns casd/execd; tests do not currently guarantee shutdown on failure.

### 2) Toolchain acquisition is expensive under test isolation

The harness uses a fresh temp `VX_HOME` per test. If toolchains are stored inside `VX_HOME`, every test may re-download toolchains (slow + flaky).

## P0: Make integration tests parallel-safe + self-cleaning

### 1) Allocate unique ports per test

Update `crates/vx/tests/harness/mod.rs` so each `TestEnv`:
- allocates 3 free loopback ports at runtime (daemon/cas/exec),
- sets:
  - `VX_DAEMON=127.0.0.1:<port>`
  - `VX_CAS=127.0.0.1:<port>`
  - `VX_EXEC=127.0.0.1:<port>`
  - `VX_HOME=<tempdir>`
for each `vx` invocation.

Notes:
- Use `TcpListener::bind("127.0.0.1:0")` to allocate a free port, read `local_addr().port()`, then drop the listener and reuse the port.
- Keep all services loopback-only.

### 2) Ensure daemons are shut down even when a test fails

Add a `Drop` impl for `TestEnv` (or an explicit cleanup method invoked via a guard) that:
- calls `vx kill` using the test’s `VX_DAEMON` endpoint
- ignores errors (best-effort), but prevents port/process leaks across tests.

### 3) Remove cross-test coupling via shared defaults

Ensure no test relies on the default ports or default `VX_HOME`.
All harness calls must consistently set:
- `VX_HOME`, `VX_DAEMON`, `VX_CAS`, `VX_EXEC`.

Success criteria:
- `cargo nextest run -p vx` is stable with high parallelism.

## P1: Reduce network + time cost while keeping isolation

Goal: keep per-test cache isolation while avoiding re-downloading toolchains for every test.

Preferred approach:

1) Add a shared “download cache” directory (content-addressed by URL+hash)
- Introduce env vars (names TBD; pick one set and standardize):
  - `VX_DOWNLOAD_CACHE_DIR` (shared between tests)
  - (optional) `VX_TOOLCHAIN_DOWNLOAD_CACHE_DIR`
  - (optional) `VX_REGISTRY_DOWNLOAD_CACHE_DIR`
- In `vx_toolchain::download_component` (and crates.io downloads), first check the cache dir by expected hash:
  - if present and hash matches, reuse bytes without network
  - otherwise download and populate cache

2) In tests, keep `VX_HOME` per-test, but set `VX_DOWNLOAD_CACHE_DIR` to a shared location
- For `nextest`, configure this via `nextest.toml` so it’s consistent across runs.

Success criteria:
- First test run downloads toolchain once; subsequent tests reuse cache and are fast.

## P1: nextest configuration

Add (or update) a `nextest.toml` to:
- set stable env defaults for integration tests (e.g. `VX_DOWNLOAD_CACHE_DIR`)
- tune timeouts for toolchain download
- allow higher parallelism safely (ports are per test by then)

## Verification Matrix

After P0:
- `cargo test -p vx --test cache_basics -- --nocapture`
- `cargo test -p vx --test multi_crate -- --nocapture`

After P0 + P1:
- `cargo nextest run -p vx`

Manual:
- `target/debug/vx build` in a real project directory.
