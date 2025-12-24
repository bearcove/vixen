# Vixen Development Guide

## Testing vx build manually

To test the vx build system end-to-end:

1. **Install the binaries**:
   ```bash
   cargo xtask install
   ```
   This builds all vixen binaries in release mode and installs them to `~/.cargo/bin`:
   - `vx` - Main CLI
   - `vx-oort` - Content-Addressed Storage service
   - `vx-rhea` - Execution worker service
   - `vx-aether` - Build orchestration daemon

2. **Test in a project**:
   ```bash
   cd ~/bearcove/helloworld
   vx build
   ```

3. **Check logs**:
   - Oort logs: `~/.vx/oort.log`
   - Rhea logs: `~/.vx/rhea.log`
   - Aether logs are shown in the terminal

4. **Clean state for testing**:
   ```bash
   rm -rf ~/.vx
   ```
   This removes the picante cache, CAS blobs, toolchains, etc. for a fresh start.

## Architecture Overview

The vx build system consists of three services:

- **vx-aether**: Build orchestration daemon that:
  - Manages the action dependency graph
  - Schedules parallel execution using Kahn's algorithm
  - Tracks incremental state with picante
  - Spawns vx-oort and vx-rhea on demand

- **vx-oort**: Content-Addressed Storage service that:
  - Stores build artifacts, toolchains, and source trees
  - Provides hermetic caching by content hash
  - Downloads toolchains from static.rust-lang.org
  - Downloads registry crates from crates.io

- **vx-rhea**: Execution worker that:
  - Materializes toolchains and source trees from CAS
  - Runs rustc with proper sysroot configuration
  - Ingests build outputs back to CAS
  - Returns output manifest hashes
