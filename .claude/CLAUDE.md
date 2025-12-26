# Vixen Development Guide

## IMPORTANT: Always Install After Changes

**After making ANY code changes**, you MUST run:
```bash
cargo xtask install
```
This rebuilds and installs the binaries to `~/.cargo/bin` so your changes take effect.

## Testing vx build manually

To test the vx build system end-to-end:

1. **Install the binaries**:
   ```bash
   cargo xtask install
   ```
   This builds all vixen binaries in release mode and installs them to `~/.cargo/bin`:
   - `vx` - Main CLI
   - `vx-cass` - Content-Addressed Storage service
   - `vx-rhea` - Execution worker service
   - `vx-aether` - Build orchestration daemon

2. **Use test VX_HOME** (IMPORTANT):
   ```bash
   # Use the .envrc to set VX_HOME to /tmp/.vx-for-tests
   direnv allow
   ```
   This prevents polluting the user's `~/.vx` with test data and downloaded crates.

   **NEVER run `rm -rf ~/.vx` - it nukes the CAS and forces redownloading all registry crates!**

3. **Test in a project**:
   ```bash
   cd ~/bearcove/helloworld
   vx build
   ```

4. **Check logs**:
   - Cass logs: `$VX_HOME/cass.log`
   - Rhea logs: `$VX_HOME/rhea.log`
   - Aether logs: `$VX_HOME/aether.log`

5. **Clean state for testing**:
   ```bash
   # Only clean the project's .vx directory
   rm -rf .vx

   # Or clean the test VX_HOME (if using .envrc)
   rm -rf /tmp/.vx-for-tests
   ```

## Architecture Overview

The vx build system consists of three services:

- **vx-aether**: Build orchestration daemon that:
  - Manages the action dependency graph
  - Schedules parallel execution using Kahn's algorithm
  - Tracks incremental state with picante
  - Spawns vx-cass and vx-rhea on demand

- **vx-cass**: Content-Addressed Storage service that:
  - Stores build artifacts, toolchains, and source trees
  - Provides hermetic caching by content hash
  - Downloads toolchains from static.rust-lang.org
  - Downloads registry crates from crates.io

- **vx-rhea**: Execution worker that:
  - Materializes toolchains and source trees from CAS
  - Runs rustc with proper sysroot configuration
  - Ingests build outputs back to CAS
  - Returns output manifest hashes

## Async Filesystem Operations (MANDATORY)

**ALWAYS use `tokio::fs::*` instead of `std::fs::*`** in async code. This codebase uses fs-kitty VFS mounts, and blocking syscalls on tokio worker threads cause deadlocks.

| ❌ Never use | ✅ Always use |
|-------------|---------------|
| `std::fs::metadata()` | `tokio::fs::metadata()` |
| `std::fs::read()` | `tokio::fs::read()` |
| `std::fs::write()` | `tokio::fs::write()` |
| `std::fs::create_dir_all()` | `tokio::fs::create_dir_all()` |
| `std::fs::remove_file()` | `tokio::fs::remove_file()` |
| `std::fs::remove_dir_all()` | `tokio::fs::remove_dir_all()` |
| `std::fs::read_dir()` | `tokio::fs::read_dir()` |
| `std::process::Command` | `tokio::process::Command` |

This applies to **any** filesystem path that might go through VFS (especially `~/.rhea/`).
