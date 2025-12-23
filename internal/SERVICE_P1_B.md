# Phase 1B: Tighten V1 Spec + Implementation Cleanup

This document captures follow-up work after Phase 1 “service split” landed. The goal is to keep configuration predictable and avoid async runtime hazards, while making it explicit that daemon/execd/casd do **not** need to share a host or filesystem.

## Decisions (Locked)

1) **Simplify configuration back to the original env vars**
- Use only these endpoints (accept `host:port` or `tcp://host:port`):
  - `VX_DAEMON` (default `127.0.0.1:9001`)
  - `VX_CAS` (default `127.0.0.1:9002`)
  - `VX_EXEC` (default `127.0.0.1:9003`)
- Remove daemon-specific endpoint vars (e.g. `VX_DAEMON_CAS`, `VX_DAEMON_EXEC`).
- Avoid split “bind vs endpoint” naming. An endpoint is both the bind addr (server) and connect addr (client).
- Auto-spawn is **only** supported for loopback endpoints; for remote endpoints you must start services out-of-band.

2) **Daemon shutdown must kill spawned children**
- `vx kill` calls `daemon.shutdown()`.
- `daemon.shutdown()` must:
  - kill only services the daemon spawned (casd/execd) via `Child::kill()`
  - then exit the daemon process
- No graceful drain / in-flight tracking in V1.

3) **Make co-location assumptions explicit**
- Daemon/execd/casd may run on different hosts.
- Daemon and execd do not share filesystem paths; execd inputs/outputs are CAS-only.
- If `VX_EXEC` points to a non-loopback endpoint, daemon must know execd’s host triple for toolchain selection (set `VX_EXEC_HOST_TRIPLE`).

4) **Add an explicit RPC “version” call (and use it for health checks)**
- Add a `version()` RPC to each service trait (CAS, Exec, Daemon) returning a small struct:
  - service name (`"vx-casd"`, `"vx-execd"`, `"vx-daemon"`)
  - crate version (`CARGO_PKG_VERSION`)
  - protocol/schema version (a constant we control; bump when breaking wire changes)
- Use this for “is this really our service?” checks:
  - When a port is bound but not our service (protocol mismatch), fail fast with a clear error.
  - When auto-spawning, verify readiness by calling `version()` (instead of raw `TcpStream::connect`).
- Note: Long-term we may want rapace to provide this automatically, but for V1B implement it at the service layer.

5) **No blocking I/O on async runtime paths**
- No `std::fs::*` (and other blocking syscalls) on the tokio runtime in daemons/services.
- Use `tokio::fs` for filesystem I/O and wrap true blocking / CPU-heavy work in `tokio::task::spawn_blocking`.
- Specifically: source-tree ingestion in the daemon must not use `std::fs::read` inside async code.

## Implementation Tasks

### A) Env var cleanup (align with `SERVICE_P1.md` intent)
- Update:
  - `vx` to use only `VX_DAEMON`
  - `vx-daemon` to read `VX_DAEMON`, `VX_CAS`, `VX_EXEC`
  - `vx-casd` to read only `VX_CAS` (and derive its storage root from `VX_HOME`)
  - `vx-execd` to read `VX_EXEC` (bind) and `VX_CAS` (connect)
- Ensure the daemon propagates the same env vars when spawning children.

### B) Shutdown wiring
- Ensure the object that implements the Daemon RPC has access to the `SpawnTracker` used by main.
- Call `SpawnTracker::kill_all()` inside `shutdown()` before exiting.

### C) Remote-safe auto-spawn behavior
- Auto-spawn only for loopback endpoints; for non-loopback endpoints, fail fast with a clear error message.

### D) RPC version() API + health checks
- Add `version()` RPC to:
  - `vx-cas-proto::Cas`
  - `vx-exec-proto::Exec`
  - `vx-daemon-proto::Daemon`
- Implement in each server.
- Replace “connect check” logic in auto-spawn paths to:
  - connect
  - create client + spawn `session.run()`
  - call `version()`
  - only then treat the service as “up”

### E) Remove blocking I/O in daemon and services
- Replace remaining `std::fs` usage in async contexts with `tokio::fs` (or `spawn_blocking` if required).
- Audit for other blocking calls in service request handlers and materialization paths.

## Updated Smoke Test Expectations

1) `cargo build --bin vx-casd --bin vx-execd --bin vx-daemon --bin vx`
2) `target/debug/vx build` with no daemons running:
   - `vx` spawns `vx-daemon`
   - daemon spawns `vx-casd` and `vx-execd`
   - daemon verifies children via `version()` RPC
3) `target/debug/vx kill`:
   - daemon kills spawned children and exits
4) Verify no listeners remain on 9001–9003
5) Verify setting a non-loopback endpoint fails with a clear error message

## Out of Scope (still deferred)
- Remote execution / non-loopback binding
- PID files / locking
- Crash recovery / orphan cleanup
- Graceful shutdown / draining
- Build scripts via execd
