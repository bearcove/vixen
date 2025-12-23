# Phase 1B: Tighten V1 Spec + Implementation Cleanup

This document captures agreed follow-up work after Phase 1 “local-only service split” landed. The goal is to make the implementation match the original intent of `internal/SERVICE_P1.md`: simple, local-only IPC with predictable configuration and no async runtime hazards.

## Decisions (Locked)

1) **Simplify configuration back to the original env vars**
- Use only these endpoints (simple `host:port` strings):
  - `VX_DAEMON` (default `127.0.0.1:9001`)
  - `VX_CAS` (default `127.0.0.1:9002`)
  - `VX_EXEC` (default `127.0.0.1:9003`)
- Remove daemon-specific endpoint vars (e.g. `VX_DAEMON_CAS`, `VX_DAEMON_EXEC`).
- Avoid split “bind vs endpoint” naming. For V1, an endpoint is both the bind addr (server) and connect addr (client), and is always loopback.

2) **Daemon shutdown must kill spawned children**
- `vx kill` calls `daemon.shutdown()`.
- `daemon.shutdown()` must:
  - kill only services the daemon spawned (casd/execd) via `Child::kill()`
  - then exit the daemon process
- No graceful drain / in-flight tracking in V1.

3) **Enforce loopback-only**
- All services must reject non-loopback binds in V1.
- Accepted: `127.0.0.1:*` only (not `0.0.0.0`, not other interfaces).

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

### C) Loopback enforcement
- On startup (before bind/connect), parse the endpoint and enforce loopback.
- If a user sets a non-loopback value, print an explicit error explaining V1 limitation.

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
