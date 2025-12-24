# Unix Socket Migration & Integration Test Fixes

## Current State

### Done
- **Switched IPC from TCP ports to Unix sockets** for local communication between vx services
  - Added `Endpoint` enum (TCP or Unix) to `vx-io/src/net.rs`
  - Added `Stream` enum with `AsyncRead`/`AsyncWrite` impl for both stream types
  - Added `Listener` abstraction that works with both TCP and Unix sockets
  - Added `connect`/`try_connect` helpers
  - Updated all services (vx-cass, vx-rhea, vx-aether) and the vx client to use the new API
  - Default endpoints are now Unix sockets in `$VX_HOME/`:
    - `$VX_HOME/aether.sock`
    - `$VX_HOME/cass.sock`
    - `$VX_HOME/rhea.sock`

- **Fixed unit tests** for `vx-manifest` and `vx-cache` that broke due to earlier API changes

- **Simplified test harness** - no more port allocation needed since each test's VX_HOME gets its own Unix sockets

### In Progress
- **Integration tests are failing** - they compile and run but the builds don't succeed

## Next Steps

1. **Debug why integration tests fail** - the build process seems to not complete successfully:
   - Run a single test with verbose output: `cargo test -p vx --test build_reports build_writes_report_file -- --nocapture`
   - Check the test's VX_HOME logs (aether.log, cass.log, rhea.log)
   - The tests create temp directories - you may need to print them to inspect

2. **Likely issues to investigate**:
   - Services may not be finding each other (check socket paths)
   - The spawned services might be dying before they can handle requests
   - The backoff/retry logic might not be waiting long enough for Unix socket creation

3. **After tests pass**: Push changes and update CLAUDE.md if needed

## Key Files

- `crates/vx-io/src/net.rs` - New networking abstraction with Endpoint/Stream/Listener
- `crates/vx/tests/harness/mod.rs` - Test harness (simplified, now relies on Unix sockets)
- `crates/vx/tests/build_reports.rs` - The failing integration tests
- `crates/vx/src/main.rs` - vx CLI, see `get_or_spawn_aether()`
- `crates/vx-aether/src/main.rs` - Orchestrator, spawns vx-cass and vx-rhea

## Gotchas

- **NEVER run `rm -rf ~/.vx`** - it nukes the CAS with all downloaded registry crates
- Use `direnv allow` in the vixen directory to set `VX_HOME=/tmp/.vx-for-tests` for manual testing
- The services use `ur_taking_me_with_you` to die when their parent dies
- Unix sockets are created by binding, so they need the parent directory to exist
- `Listener::bind` for Unix sockets removes any existing socket file before binding

## Commands

```bash
# Install binaries (needed after code changes)
cargo xtask install

# Run just the failing integration tests
cargo test -p vx --test build_reports

# Run a single test with output
cargo test -p vx --test build_reports build_writes_report_file -- --nocapture

# Run unit tests only (these all pass)
cargo test --lib

# Manual test in a project (with test VX_HOME)
cd ~/bearcove/helloworld
VX_HOME=/tmp/.vx-for-tests vx build

# Check service logs
cat /tmp/.vx-for-tests/aether.log
cat /tmp/.vx-for-tests/cass.log
cat /tmp/.vx-for-tests/rhea.log
```

## Recent Commits

- `702773b` - Switch from TCP ports to Unix sockets for local IPC
- `d8a3205` - Fix tests for updated API signatures
- `4d58ab1` - Add .envrc for test VX_HOME and update testing documentation
