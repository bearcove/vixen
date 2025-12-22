# Implementation Plan: Separate vertex into 4 Distinct Binaries with rapace IPC

## Executive Summary

This plan details how to refactor vertex from its current in-process architecture to 4 separate binaries (`vx`, `vx-daemon`, `vx-casd`, `vx-execd`) communicating via rapace IPC.

## Current State Analysis

### Architecture Today

- `vx` CLI directly instantiates `DaemonService` in-process (no IPC)
- `DaemonService` directly instantiates `CasService` and `ExecService` as Rust objects
- No `main.rs` files exist for daemon, casd, or execd (only library crates)
- No `[[bin]]` sections in Cargo.toml files for these crates

### Key Issues Found

1. **Direct `Command::new("rustc")` in daemon** (bypasses Exec service):
   - `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs:1902` - in `build_rlib()`
   - `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs:2113` - in `build_bin_with_deps()`

2. **Direct `Command::new` in build script execution** (bypasses Exec service):
   - `/Users/amos/bearcove/vertex/crates/vx-rs/src/build_script.rs:81` - compiling build.rs
   - `/Users/amos/bearcove/vertex/crates/vx-rs/src/build_script.rs:97` - running build.rs

3. **In-process service instantiation**:
   - `DaemonService::new()` creates `CasService` and `ExecService` directly
   - No rapace client connections exist

---

## Design Decisions

### Service Discovery Strategy

**Approach: Environment Variables with Sensible Defaults**

| Service | Env Var | Default Value |
|---------|---------|---------------|
| Daemon | `VX_DAEMON` | `shm://$VX_HOME/daemon.sock` |
| CAS | `VX_CAS` | `shm://$VX_HOME/cas.sock` |
| Exec | `VX_EXEC` | `shm://$VX_HOME/exec.sock` |

Where `$VX_HOME` defaults to `~/.vx`.

**Rationale**: 
- SHM sockets are fast for local development
- Env vars allow easy override for remote workers
- Defaults mean zero configuration for common case

### Startup Order and Auto-Spawn

**Strategy: Lazy Auto-Spawn with Health Checks**

1. `vx build` connects to daemon; if connection fails, spawns `vx-daemon`
2. `vx-daemon` connects to casd; if fails, spawns `vx-casd`
3. `vx-daemon` connects to execd; if fails, spawns `vx-execd`
4. `vx-execd` connects to casd (must already be running from step 2)

**Auto-spawn implementation**:
```rust
// In vx CLI
async fn get_daemon_client() -> Result<DaemonClient> {
    match DaemonClient::connect(&get_daemon_endpoint()).await {
        Ok(client) => Ok(client),
        Err(_) => {
            spawn_daemon()?;
            // Retry with exponential backoff
            for delay in [10, 50, 100, 500, 1000] {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if let Ok(client) = DaemonClient::connect(&get_daemon_endpoint()).await {
                    return Ok(client);
                }
            }
            Err(eyre!("failed to connect to daemon after spawn"))
        }
    }
}
```

### Shutdown Cascade

**Strategy: Graceful Shutdown via RPC**

1. `vx kill` calls `daemon.shutdown()`
2. Daemon calls `execd.shutdown()`, waits for ACK
3. Daemon exits
4. CAS stays running (shared resource, might serve other projects)

**For full cleanup**: `vx clean` sends shutdown to all services.

---

## Implementation Phases

### Phase 1: Add Binary Infrastructure (No Behavior Change)

Add `main.rs` files and `[[bin]]` sections without changing current in-process behavior. This allows building binaries that will be wired up later.

#### 1.1 Create `crates/vx-casd/src/main.rs`

```rust
//! vx-casd: Content-addressed storage service binary
//!
//! Listens on a rapace endpoint and serves the Cas trait.

use camino::Utf8PathBuf;
use eyre::Result;
use facet::Facet;
use facet_args as args;
use tracing_subscriber::EnvFilter;
use vx_casd::CasService;

#[derive(Facet, Debug)]
struct Cli {
    /// CAS storage root directory
    #[facet(args::named)]
    root: Option<Utf8PathBuf>,
    
    /// Endpoint to listen on (default: shm://$VX_HOME/cas.sock)
    #[facet(args::named)]
    endpoint: Option<String>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,vx_casd=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    
    let cli: Cli = args::from_std_args()?;
    
    let vx_home = get_vx_home()?;
    let root = cli.root.unwrap_or_else(|| vx_home.clone());
    let endpoint = cli.endpoint.unwrap_or_else(|| {
        format!("shm://{}/cas.sock", vx_home)
    });
    
    let service = CasService::new(root);
    service.init()?;
    
    tracing::info!(endpoint = %endpoint, "starting CAS service");
    
    // TODO: Start rapace server (Phase 2)
    // let server = CasServer::new(service);
    // server.serve(Transport::from_endpoint(&endpoint)?).await?;
    
    // For now, just block forever (placeholder)
    std::future::pending::<()>().await;
    Ok(())
}

fn get_vx_home() -> Result<Utf8PathBuf> {
    if let Ok(vx_home) = std::env::var("VX_HOME") {
        return Ok(Utf8PathBuf::from(vx_home));
    }
    let home = std::env::var("HOME")?;
    Ok(Utf8PathBuf::from(home).join(".vx"))
}
```

#### 1.2 Create `crates/vx-execd/src/main.rs`

```rust
//! vx-execd: Execution service binary
//!
//! Listens on a rapace endpoint and serves the Exec trait.

use camino::Utf8PathBuf;
use eyre::Result;
use facet::Facet;
use facet_args as args;
use tracing_subscriber::EnvFilter;

#[derive(Facet, Debug)]
struct Cli {
    /// Endpoint to listen on
    #[facet(args::named)]
    endpoint: Option<String>,
    
    /// CAS endpoint to connect to
    #[facet(args::named)]
    cas_endpoint: Option<String>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,vx_execd=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    
    let cli: Cli = args::from_std_args()?;
    
    let vx_home = get_vx_home()?;
    let endpoint = cli.endpoint.unwrap_or_else(|| {
        format!("shm://{}/exec.sock", vx_home)
    });
    let _cas_endpoint = cli.cas_endpoint.unwrap_or_else(|| {
        format!("shm://{}/cas.sock", vx_home)
    });
    
    tracing::info!(endpoint = %endpoint, "starting Exec service");
    
    // TODO: Connect to CAS, start rapace server (Phase 2)
    
    std::future::pending::<()>().await;
    Ok(())
}

fn get_vx_home() -> Result<Utf8PathBuf> {
    if let Ok(vx_home) = std::env::var("VX_HOME") {
        return Ok(Utf8PathBuf::from(vx_home));
    }
    let home = std::env::var("HOME")?;
    Ok(Utf8PathBuf::from(home).join(".vx"))
}
```

#### 1.3 Create `crates/vx-daemon/src/main.rs`

```rust
//! vx-daemon: Build orchestration daemon binary
//!
//! Listens on a rapace endpoint and serves the Daemon trait.

use camino::Utf8PathBuf;
use eyre::Result;
use facet::Facet;
use facet_args as args;
use tracing_subscriber::EnvFilter;

#[derive(Facet, Debug)]
struct Cli {
    /// Endpoint to listen on
    #[facet(args::named)]
    endpoint: Option<String>,
    
    /// CAS endpoint to connect to
    #[facet(args::named)]
    cas_endpoint: Option<String>,
    
    /// Exec endpoint to connect to
    #[facet(args::named)]
    exec_endpoint: Option<String>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,vx_daemon=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    
    let cli: Cli = args::from_std_args()?;
    
    let vx_home = get_vx_home()?;
    let endpoint = cli.endpoint.unwrap_or_else(|| {
        format!("shm://{}/daemon.sock", vx_home)
    });
    
    tracing::info!(endpoint = %endpoint, "starting daemon service");
    
    // TODO: Connect to CAS and Exec, start rapace server (Phase 2)
    
    std::future::pending::<()>().await;
    Ok(())
}

fn get_vx_home() -> Result<Utf8PathBuf> {
    if let Ok(vx_home) = std::env::var("VX_HOME") {
        return Ok(Utf8PathBuf::from(vx_home));
    }
    let home = std::env::var("HOME")?;
    Ok(Utf8PathBuf::from(home).join(".vx"))
}
```

#### 1.4 Update Cargo.toml Files

**`crates/vx-casd/Cargo.toml`** - add:
```toml
[[bin]]
name = "vx-casd"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
eyre = { workspace = true }
```

**`crates/vx-execd/Cargo.toml`** - add:
```toml
[[bin]]
name = "vx-execd"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
eyre = { workspace = true }
```

**`crates/vx-daemon/Cargo.toml`** - add:
```toml
[[bin]]
name = "vx-daemon"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
```

---

### Phase 2: Generate rapace Clients and Servers

The `#[rapace::service]` macro generates both client and server types. We need to use these.

#### 2.1 Verify Generated Types

The macro generates:
- `DaemonClient` - for CLI to call daemon
- `DaemonServer` - for daemon binary to serve
- `CasClient` - for daemon/execd to call CAS
- `CasServer` - for casd binary to serve
- `ExecClient` - for daemon to call execd
- `ExecServer` - for execd binary to serve

#### 2.2 Add Client Types to Proto Crates

The proto crates already have `#[rapace::service]` on their traits. The macro should generate both client and server. We may need to re-export or add convenience constructors.

**In `vx-daemon-proto/src/lib.rs`**, verify the macro generates:
```rust
// These should be auto-generated by #[rapace::service]
pub struct DaemonClient { ... }
pub struct DaemonServer<T: Daemon> { ... }
```

#### 2.3 Wire Up Servers in Binaries

**`crates/vx-casd/src/main.rs`** - replace placeholder:
```rust
use vx_cas_proto::CasServer;
use rapace::Transport;

// In main():
let service = CasService::new(root);
service.init()?;

let server = CasServer::new(service);
let transport = Transport::shm_listen(&endpoint).await?;
server.serve(transport).await?;
```

---

### Phase 3: Refactor DaemonService to Accept Clients

This is the core refactoring - changing `DaemonService` from owning service instances to owning rapace clients.

#### 3.1 New DaemonService Signature

**Current** (`crates/vx-daemon/src/lib.rs`):
```rust
pub struct DaemonService {
    cas: Arc<CasService>,
    exec: ExecService<Arc<CasService>>,
    db: Arc<Mutex<Database>>,
    // ...
}

impl DaemonService {
    pub fn new(vx_home: Utf8PathBuf) -> std::io::Result<Self> {
        let cas = CasService::new(cas_root);
        let exec = ExecService::new(Arc::clone(&cas), ...);
        // ...
    }
}
```

**New**:
```rust
pub struct DaemonService<C, E> 
where
    C: Cas + CasToolchain + CasRegistry + Send + Sync,
    E: Exec + Send + Sync,
{
    cas: C,
    exec: E,
    db: Arc<Mutex<Database>>,
    // ...
}

impl<C, E> DaemonService<C, E>
where
    C: Cas + CasToolchain + CasRegistry + Send + Sync + Clone,
    E: Exec + Send + Sync,
{
    pub fn new(vx_home: Utf8PathBuf, cas: C, exec: E) -> std::io::Result<Self> {
        // No longer creates CasService or ExecService
        // ...
    }
}

// Type alias for in-process mode (backward compat for tests)
pub type InProcessDaemonService = DaemonService<Arc<CasService>, ExecService<Arc<CasService>>>;

// Type alias for IPC mode
pub type IpcDaemonService = DaemonService<CasClient, ExecClient>;
```

#### 3.2 Remove Direct Command::new Calls

**In `build_rlib()` and `build_bin_with_deps()`**, replace:

```rust
// BEFORE (line ~1902):
let output = Command::new("rustc")
    .args(&invocation.args)
    .current_dir(&invocation.cwd)
    .output()
    .map_err(|e| format!("failed to execute rustc: {}", e))?;

// AFTER:
let result = self.exec.execute_rustc(invocation.clone()).await;
if result.exit_code != 0 {
    return Err(format!("rustc failed for {}: {}", crate_name, result.stderr));
}
```

This requires that the `RustcInvocation` already has the toolchain manifest set (which it does from `plan_compile_*` queries).

#### 3.3 Update Build Script Execution

The build script execution in `vx-rs/src/build_script.rs` also needs to go through Exec service.

**Option A**: Add `execute_build_script` to Exec trait
**Option B**: Build scripts are compiled/run as special rustc invocations

Recommend **Option A** - add to `vx-exec-proto`:
```rust
#[derive(Debug, Clone, Facet)]
pub struct BuildScriptInvocation {
    pub toolchain_manifest: ManifestHash,
    pub build_script_path: String,  // workspace-relative
    pub manifest_dir: String,
    pub out_dir: String,
    pub target: String,
    pub host: String,
    pub crate_name: String,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Facet)]
pub struct BuildScriptResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    /// Parsed cargo: directives
    pub cfgs: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
}

#[rapace::service]
pub trait Exec {
    // ... existing methods ...
    
    /// Execute a build script (compile + run)
    async fn execute_build_script(&self, invocation: BuildScriptInvocation) -> BuildScriptResult;
}
```

---

### Phase 4: Update vx CLI to Connect via rapace

#### 4.1 Replace In-Process Daemon with Client

**Current** (`crates/vx/src/main.rs`):
```rust
async fn cmd_build(release: bool) -> Result<()> {
    let vx_home = get_vx_home()?;
    let daemon = DaemonService::new(vx_home)?;  // In-process!
    
    let result = daemon.build(request).await;
    // ...
}
```

**New**:
```rust
use vx_daemon_proto::DaemonClient;

async fn cmd_build(release: bool) -> Result<()> {
    let daemon = get_or_spawn_daemon().await?;
    
    let result = daemon.build(request).await;
    // ...
}

async fn get_or_spawn_daemon() -> Result<DaemonClient> {
    let endpoint = get_daemon_endpoint();
    
    // Try to connect
    match DaemonClient::connect(&endpoint).await {
        Ok(client) => return Ok(client),
        Err(e) => {
            tracing::debug!(error = %e, "daemon not running, spawning");
        }
    }
    
    // Spawn daemon
    spawn_process("vx-daemon", &["--endpoint", &endpoint])?;
    
    // Retry with backoff
    for delay in [10, 50, 100, 500, 1000] {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        if let Ok(client) = DaemonClient::connect(&endpoint).await {
            return Ok(client);
        }
    }
    
    bail!("failed to connect to daemon after spawn")
}

fn spawn_process(binary: &str, args: &[&str]) -> Result<()> {
    use std::process::Command;
    
    Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())  // Show startup errors
        .spawn()?;
    
    Ok(())
}
```

#### 4.2 Add `vx kill` Implementation

```rust
async fn cmd_kill() -> Result<()> {
    let endpoint = get_daemon_endpoint();
    
    match DaemonClient::connect(&endpoint).await {
        Ok(client) => {
            client.shutdown().await?;
            println!("Daemon stopped");
        }
        Err(_) => {
            println!("Daemon not running");
        }
    }
    
    Ok(())
}
```

---

### Phase 5: Wire Up Daemon Binary

#### 5.1 Complete `vx-daemon/src/main.rs`

```rust
use vx_cas_proto::CasClient;
use vx_exec_proto::ExecClient;
use vx_daemon::{DaemonService, IpcDaemonService};
use vx_daemon_proto::DaemonServer;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli: Cli = args::from_std_args()?;
    
    let vx_home = get_vx_home()?;
    let endpoint = cli.endpoint.unwrap_or_else(|| format!("shm://{}/daemon.sock", vx_home));
    let cas_endpoint = cli.cas_endpoint.unwrap_or_else(|| format!("shm://{}/cas.sock", vx_home));
    let exec_endpoint = cli.exec_endpoint.unwrap_or_else(|| format!("shm://{}/exec.sock", vx_home));
    
    // Ensure CAS is running (auto-spawn if needed)
    let cas = get_or_spawn_cas(&cas_endpoint).await?;
    
    // Ensure Exec is running (auto-spawn if needed)
    let exec = get_or_spawn_exec(&exec_endpoint, &cas_endpoint).await?;
    
    // Create daemon service with clients
    let service = DaemonService::new(vx_home, cas, exec)?;
    
    // Load picante cache
    let _ = service.load_cache().await;
    
    // Serve
    tracing::info!(endpoint = %endpoint, "daemon ready");
    let server = DaemonServer::new(service);
    let transport = Transport::shm_listen(&endpoint).await?;
    server.serve(transport).await?;
    
    Ok(())
}

async fn get_or_spawn_cas(endpoint: &str) -> Result<CasClient> {
    // Similar pattern to get_or_spawn_daemon in vx CLI
}

async fn get_or_spawn_exec(endpoint: &str, cas_endpoint: &str) -> Result<ExecClient> {
    // Similar pattern, passes --cas-endpoint to execd
}
```

---

### Phase 6: Add Shutdown RPC

#### 6.1 Extend Protocol Traits

**`vx-daemon-proto/src/lib.rs`**:
```rust
#[rapace::service]
pub trait Daemon {
    async fn build(&self, request: BuildRequest) -> BuildResult;
    async fn shutdown(&self);
}
```

**`vx-exec-proto/src/lib.rs`**:
```rust
#[rapace::service]
pub trait Exec {
    // ... existing ...
    async fn shutdown(&self);
}
```

#### 6.2 Implement Shutdown in Services

Each service binary will have a `shutdown()` method that:
1. Sets a shutdown flag
2. Stops accepting new requests
3. Waits for in-flight requests to complete
4. Exits cleanly

---

## File Changes Summary

### New Files to Create

| File | Purpose |
|------|---------|
| `crates/vx-casd/src/main.rs` | CAS service binary entrypoint |
| `crates/vx-execd/src/main.rs` | Exec service binary entrypoint |
| `crates/vx-daemon/src/main.rs` | Daemon service binary entrypoint |

### Files to Modify

| File | Changes |
|------|---------|
| `crates/vx-casd/Cargo.toml` | Add `[[bin]]` section, facet-args dep |
| `crates/vx-execd/Cargo.toml` | Add `[[bin]]` section, facet-args dep |
| `crates/vx-daemon/Cargo.toml` | Add `[[bin]]` section |
| `crates/vx-daemon/src/lib.rs` | Generify over Cas/Exec clients, remove direct Command::new |
| `crates/vx/src/main.rs` | Replace in-process daemon with rapace client |
| `crates/vx-daemon-proto/src/lib.rs` | Add `shutdown()` to trait |
| `crates/vx-exec-proto/src/lib.rs` | Add `shutdown()` and `execute_build_script()` to trait |
| `crates/vx-execd/src/lib.rs` | Implement new trait methods |
| `crates/vx-rs/src/build_script.rs` | Remove direct Command::new (move to execd) |

---

## Testing Strategy

### Unit Tests
- Existing tests continue to work using in-process mode (`InProcessDaemonService`)
- Add tests for client/server connection logic

### Integration Tests
- Add test that spawns all 4 binaries and runs a build
- Add test for auto-spawn behavior
- Add test for graceful shutdown

### Manual Testing Checklist
- [ ] `cargo build -p vx-casd` produces binary
- [ ] `cargo build -p vx-execd` produces binary
- [ ] `cargo build -p vx-daemon` produces binary
- [ ] `vx-casd --help` works
- [ ] `vx-execd --help` works
- [ ] `vx-daemon --help` works
- [ ] `vx build` auto-spawns all services
- [ ] `vx kill` stops daemon and execd
- [ ] Second `vx build` reuses running services
- [ ] Build output is identical to in-process mode

---

## Critical Files for Implementation

1. **`/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`** - Core refactoring: generify over Cas/Exec clients, remove direct Command::new calls at lines 1902 and 2113

2. **`/Users/amos/bearcove/vertex/crates/vx/src/main.rs`** - Replace in-process DaemonService with rapace DaemonClient, add auto-spawn logic

3. **`/Users/amos/bearcove/vertex/crates/vx-exec-proto/src/lib.rs`** - Add `execute_build_script()` method to Exec trait for hermetic build script execution

4. **`/Users/amos/bearcove/vertex/crates/vx-execd/src/lib.rs`** - Already implements Exec trait; will need new `execute_build_script()` implementation

5. **`/Users/amos/bearcove/vertex/crates/vx-daemon-proto/src/lib.rs`** - Add `shutdown()` method to Daemon trait

---

## Risk Assessment

| Risk | Mitigation |
|------|------------|
| rapace SHM transport not ready | Start with TCP transport, migrate to SHM later |
| Performance regression from IPC overhead | SHM is near-zero overhead; benchmark before/after |
| Auto-spawn race conditions | Use file locks or PID files for mutual exclusion |
| Backward compatibility | Keep `InProcessDaemonService` type alias for tests |

---

## Open Questions

1. **Should CAS persist across `vx kill`?** Current plan says yes (shared resource). Confirm this is desired.

2. **How to handle version skew?** If `vx` binary is newer than running daemon, should it restart the daemon?

3. **PID file location?** Suggest `$VX_HOME/daemon.pid`, `$VX_HOME/exec.pid` for process management.

4. **Logging destination for spawned services?** Currently stderr; consider `$VX_HOME/logs/` directory.
# Implementation Plan: Move All Execution from vx-daemon to Exec Service

## Executive Summary

This plan details how to move all direct `Command::new()` calls from `vx-daemon` to the `Exec` service (`vx-execd`). Currently, the daemon directly invokes `rustc` in `build_rlib()` and `build_bin_with_deps()`, bypassing the Exec service entirely. The goal is to make the daemon a pure orchestrator that never executes anything directly.

## Current State Analysis

### Direct Command Invocations in vx-daemon

Located in `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`:

1. **`build_rlib()` (around line 1902)**:
   ```rust
   let output = Command::new("rustc")
       .args(&invocation.args)
       .current_dir(&invocation.cwd)
       .output()
   ```

2. **`build_bin_with_deps()` (around line 2113)**:
   ```rust
   let output = Command::new("rustc")
       .args(&invocation.args)
       .current_dir(&invocation.cwd)
       .output()
   ```

### Existing Exec Service Interface

From `/Users/amos/bearcove/vertex/crates/vx-exec-proto/src/lib.rs`:

```rust
#[rapace::service]
pub trait Exec {
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult;
    async fn execute_cc(&self, invocation: CcInvocation) -> CcExecuteResult;
    async fn materialize_registry_crate(&self, request: RegistryMaterializeRequest) -> RegistryMaterializeResult;
}
```

### Build Script Execution

From `/Users/amos/bearcove/vertex/crates/vx-rs/src/build_script.rs`:
- `run_build_script()` function currently uses `Command::new()` directly
- Compiles build.rs to binary, then runs it
- Parses `cargo:` directives from stdout
- Returns `BuildScriptOutput` with cfgs, envs, link_libs, link_search

## Design Decisions

### 1. BuildScriptInvocation Structure

```rust
/// Structured build script invocation
#[derive(Debug, Clone, Facet)]
pub struct BuildScriptInvocation {
    /// Toolchain manifest hash (for rustc to compile build.rs)
    pub toolchain_manifest: ManifestHash,
    
    /// Crate name (for CARGO_PKG_NAME and error messages)
    pub crate_name: String,
    
    /// Package version (for CARGO_PKG_VERSION* env vars)
    pub pkg_version: PackageVersion,
    
    /// Path to build.rs relative to workspace root
    pub build_script_rel: String,
    
    /// Crate manifest directory relative to workspace root
    pub manifest_dir_rel: String,
    
    /// Output directory for build script artifacts (OUT_DIR)
    /// Relative to workspace root, e.g., ".vx/build-script-out/<crate_id>"
    pub out_dir_rel: String,
    
    /// Target triple (for TARGET env var)
    pub target: String,
    
    /// Host triple (for HOST env var)
    pub host: String,
    
    /// Profile ("debug" or "release")
    pub profile: String,
    
    /// Working directory (workspace root)
    pub cwd: String,
    
    /// Additional environment variables
    pub env: Vec<(String, String)>,
}

/// Package version components for CARGO_PKG_VERSION* env vars
#[derive(Debug, Clone, Facet)]
pub struct PackageVersion {
    pub full: String,      // "1.2.3"
    pub major: String,     // "1"
    pub minor: String,     // "2"
    pub patch: String,     // "3"
    pub pre: String,       // "" or "alpha.1"
}
```

### 2. BuildScriptResult Structure

```rust
/// Result of executing a build script
#[derive(Debug, Clone, Facet)]
pub struct BuildScriptResult {
    /// Exit code (0 = success)
    pub exit_code: i32,
    
    /// Captured stdout (raw, contains cargo: directives)
    pub stdout: String,
    
    /// Captured stderr
    pub stderr: String,
    
    /// Duration in milliseconds
    pub duration_ms: u64,
    
    /// Parsed build script output
    pub parsed: BuildScriptOutput,
    
    /// Files produced in OUT_DIR (for caching)
    pub out_dir_files: Vec<OutDirFile>,
}

/// A file produced by the build script in OUT_DIR
#[derive(Debug, Clone, Facet)]
pub struct OutDirFile {
    /// Relative path within OUT_DIR
    pub rel_path: String,
    /// Blob hash in CAS
    pub blob_hash: BlobHash,
}

/// Parsed cargo: directives (same as existing BuildScriptOutput)
#[derive(Debug, Clone, Default, Facet)]
pub struct BuildScriptOutput {
    /// cargo:rustc-cfg=... flags
    pub cfgs: Vec<String>,
    /// cargo:rustc-env=NAME=VALUE pairs  
    pub envs: Vec<(String, String)>,
    /// cargo:rustc-link-lib=... libraries
    pub link_libs: Vec<String>,
    /// cargo:rustc-link-search=... paths
    pub link_search: Vec<String>,
}
```

### 3. Environment Variables for Build Scripts

Required environment variables (per Cargo documentation):

| Variable | Source | Example |
|----------|--------|---------|
| `CARGO_MANIFEST_DIR` | Absolute path to manifest dir | `/work/ws/my-crate` |
| `OUT_DIR` | Absolute path to output dir | `/work/ws/.vx/build-script-out/abc123` |
| `TARGET` | From BuildScriptInvocation | `aarch64-apple-darwin` |
| `HOST` | From BuildScriptInvocation | `aarch64-apple-darwin` |
| `RUSTC` | Materialized toolchain path | `/home/.vx/toolchains/<hash>/bin/rustc` |
| `CARGO_PKG_NAME` | From invocation | `my-crate` |
| `CARGO_PKG_VERSION` | From invocation | `1.2.3` |
| `CARGO_PKG_VERSION_MAJOR` | From invocation | `1` |
| `CARGO_PKG_VERSION_MINOR` | From invocation | `2` |
| `CARGO_PKG_VERSION_PATCH` | From invocation | `3` |
| `CARGO_PKG_VERSION_PRE` | From invocation | `` |
| `PROFILE` | From invocation | `release` |
| `DEBUG` | Derived from profile | `false` |
| `OPT_LEVEL` | Derived from profile | `3` |

### 4. Build Script Caching Strategy

**Cache Key Components:**
- Toolchain manifest hash
- Build script source hash (blake3 of build.rs content)
- Target triple
- Profile
- Package version
- Dependency crate hashes (if build script depends on other crates)

**Cached Outputs:**
- `BuildScriptOutput` (cfg flags, env vars, link directives)
- OUT_DIR contents (as blobs in CAS)

**Cache Invalidation:**
- `cargo:rerun-if-changed=PATH` directives (future enhancement)
- `cargo:rerun-if-env-changed=VAR` directives (future enhancement)
- For V1: always rerun (no caching), or cache based on build.rs hash only

## Implementation Steps

### Phase 1: Add BuildScript Types to vx-exec-proto

**File:** `/Users/amos/bearcove/vertex/crates/vx-exec-proto/src/lib.rs`

Add the new types:
- `BuildScriptInvocation`
- `PackageVersion`  
- `BuildScriptResult`
- `BuildScriptOutput` (move from vx-rs or duplicate with Facet derive)
- `OutDirFile`

Add new method to Exec trait:
```rust
#[rapace::service]
pub trait Exec {
    async fn execute_rustc(&self, invocation: RustcInvocation) -> ExecuteResult;
    async fn execute_cc(&self, invocation: CcInvocation) -> CcExecuteResult;
    async fn materialize_registry_crate(&self, request: RegistryMaterializeRequest) -> RegistryMaterializeResult;
    
    // NEW
    async fn execute_build_script(&self, invocation: BuildScriptInvocation) -> BuildScriptResult;
}
```

### Phase 2: Implement execute_build_script in vx-execd

**File:** `/Users/amos/bearcove/vertex/crates/vx-execd/src/lib.rs`

Implementation outline:
```rust
async fn execute_build_script(&self, invocation: BuildScriptInvocation) -> BuildScriptResult {
    let start = Instant::now();
    
    // 1. Materialize toolchain
    let toolchain_dir = self.ensure_materialized(invocation.toolchain_manifest).await?;
    let rustc_path = toolchain_dir.join("bin/rustc");
    
    // 2. Compute absolute paths
    let workspace_root = Utf8Path::new(&invocation.cwd);
    let manifest_dir = workspace_root.join(&invocation.manifest_dir_rel);
    let out_dir = workspace_root.join(&invocation.out_dir_rel);
    let build_script_path = workspace_root.join(&invocation.build_script_rel);
    
    // 3. Create OUT_DIR
    std::fs::create_dir_all(&out_dir)?;
    
    // 4. Compile build.rs to binary
    let build_script_bin = out_dir.join("build_script");
    let compile_output = Command::new(&rustc_path)
        .arg("--sysroot").arg(&toolchain_dir)
        .arg(&build_script_path)
        .arg("--crate-type=bin")
        .arg("--edition=2021")
        .arg("-o").arg(&build_script_bin)
        .env_clear()
        .output()?;
    
    if !compile_output.status.success() {
        return BuildScriptResult::compile_error(...);
    }
    
    // 5. Run build script with required env vars
    let run_output = Command::new(&build_script_bin)
        .env_clear()
        .env("CARGO_MANIFEST_DIR", &manifest_dir)
        .env("OUT_DIR", &out_dir)
        .env("TARGET", &invocation.target)
        .env("HOST", &invocation.host)
        .env("RUSTC", &rustc_path)
        .env("CARGO_PKG_NAME", &invocation.crate_name)
        .env("CARGO_PKG_VERSION", &invocation.pkg_version.full)
        .env("CARGO_PKG_VERSION_MAJOR", &invocation.pkg_version.major)
        .env("CARGO_PKG_VERSION_MINOR", &invocation.pkg_version.minor)
        .env("CARGO_PKG_VERSION_PATCH", &invocation.pkg_version.patch)
        .env("CARGO_PKG_VERSION_PRE", &invocation.pkg_version.pre)
        .env("PROFILE", &invocation.profile)
        .env("DEBUG", if invocation.profile == "debug" { "true" } else { "false" })
        .env("OPT_LEVEL", if invocation.profile == "release" { "3" } else { "0" })
        .envs(&invocation.env)
        .current_dir(&manifest_dir)
        .output()?;
    
    // 6. Parse stdout for cargo: directives
    let stdout = String::from_utf8_lossy(&run_output.stdout).to_string();
    let parsed = parse_build_script_output(&stdout);
    
    // 7. Collect OUT_DIR files and upload to CAS
    let out_dir_files = self.collect_out_dir_files(&out_dir).await?;
    
    BuildScriptResult {
        exit_code: run_output.status.code().unwrap_or(-1),
        stdout,
        stderr: String::from_utf8_lossy(&run_output.stderr).to_string(),
        duration_ms: start.elapsed().as_millis() as u64,
        parsed,
        out_dir_files,
    }
}
```

### Phase 3: Replace Direct rustc Calls in vx-daemon

**File:** `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`

#### 3.1 Add exec service field to DaemonService

The `DaemonService` already has an `exec: ExecService` field, so we can use `self.exec.execute_rustc()`.

#### 3.2 Modify build_rlib() to use Exec service

Replace:
```rust
let output = Command::new("rustc")
    .args(&invocation.args)
    .current_dir(&invocation.cwd)
    .output()
    .map_err(|e| format!("failed to execute rustc: {}", e))?;
```

With:
```rust
let result = self.exec.execute_rustc(invocation.clone()).await;

if result.exit_code != 0 {
    return Err(format!("rustc failed for {}: {}", crate_name, result.stderr));
}
```

#### 3.3 Modify build_bin_with_deps() similarly

Same pattern as build_rlib().

#### 3.4 Handle output materialization

The `execute_rustc` in execd already:
1. Materializes the toolchain
2. Runs rustc with --sysroot
3. Uploads outputs to CAS
4. Returns `ProducedOutput` with blob hashes

The daemon needs to materialize outputs from CAS to workspace (similar to cache hit path).

### Phase 4: Add Build Script Integration to Daemon

**File:** `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`

Add a method to run build scripts via Exec:
```rust
async fn run_build_script_via_exec(
    &self,
    crate_node: &CrateNode,
    workspace_root: &Utf8PathBuf,
    target_triple: &str,
    profile: &str,
    toolchain_manifest: ManifestHash,
) -> Result<BuildScriptOutput, String> {
    let build_script_rel = crate_node.build_script_rel.as_ref()
        .ok_or("no build script")?;
    
    // Parse version from manifest (TODO: add to CrateNode)
    let pkg_version = PackageVersion::default(); // Placeholder
    
    let invocation = BuildScriptInvocation {
        toolchain_manifest,
        crate_name: crate_node.crate_name.clone(),
        pkg_version,
        build_script_rel: build_script_rel.to_string(),
        manifest_dir_rel: crate_node.workspace_rel.to_string(),
        out_dir_rel: format!(".vx/build-script-out/{}", crate_node.id.short_hex()),
        target: target_triple.to_string(),
        host: target_triple.to_string(), // Native build
        profile: profile.to_string(),
        cwd: workspace_root.to_string(),
        env: vec![],
    };
    
    let result = self.exec.execute_build_script(invocation).await;
    
    if result.exit_code != 0 {
        return Err(format!(
            "build script failed for {}: {}",
            crate_node.crate_name, result.stderr
        ));
    }
    
    Ok(result.parsed)
}
```

### Phase 5: Wire Up Build Script Execution in do_build()

Currently, `build_script_output` is always `None` in the crate graph. We need to:

1. Before building each crate, check if it has a build script
2. Run the build script via Exec service
3. Store the output in the crate node
4. Pass cfg flags to the rustc invocation (already done)

## Detailed File Changes

### `/Users/amos/bearcove/vertex/crates/vx-exec-proto/src/lib.rs`

**Add:**
- `BuildScriptInvocation` struct with Facet derive
- `PackageVersion` struct with Facet derive
- `BuildScriptResult` struct with Facet derive
- `BuildScriptOutput` struct with Facet derive (move from vx-rs)
- `OutDirFile` struct with Facet derive
- `execute_build_script` method to `Exec` trait

### `/Users/amos/bearcove/vertex/crates/vx-execd/src/lib.rs`

**Add:**
- `execute_build_script` implementation
- `collect_out_dir_files` helper method
- `parse_build_script_output` (move from vx-rs or reuse)

### `/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`

**Modify:**
- `build_rlib()`: Replace `Command::new("rustc")` with `self.exec.execute_rustc()`
- `build_bin_with_deps()`: Replace `Command::new("rustc")` with `self.exec.execute_rustc()`
- `do_build()`: Add build script execution before crate compilation
- Add `run_build_script_via_exec()` helper method

**Remove:**
- Direct `use std::process::Command;` (or reduce usage)

### `/Users/amos/bearcove/vertex/crates/vx-rs/src/build_script.rs`

**Consider:**
- Keep for parsing logic (`parse_build_script_output`)
- Remove `run_build_script` function (execution moves to execd)
- Or keep as fallback for non-daemon usage

### `/Users/amos/bearcove/vertex/crates/vx-rs/src/crate_graph.rs`

**Consider:**
- Add `pkg_version` field to `CrateNode` for build script env vars
- Parse version from Cargo.toml during graph construction

## Sequence Diagram

```
┌──────────┐     ┌────────┐     ┌────────┐     ┌─────┐
│  Daemon  │     │ ExecD  │     │  CAS   │     │ FS  │
└────┬─────┘     └───┬────┘     └───┬────┘     └──┬──┘
     │               │              │             │
     │ execute_rustc(invocation)    │             │
     │──────────────>│              │             │
     │               │              │             │
     │               │ get_toolchain_manifest()   │
     │               │─────────────>│             │
     │               │<─────────────│             │
     │               │              │             │
     │               │ materialize toolchain      │
     │               │────────────────────────────>
     │               │              │             │
     │               │ run rustc with --sysroot   │
     │               │────────────────────────────>
     │               │<────────────────────────────
     │               │              │             │
     │               │ put_blob(output)           │
     │               │─────────────>│             │
     │               │<─────────────│             │
     │               │              │             │
     │<──────────────│ ExecuteResult              │
     │               │              │             │
     │ get_blob(output_hash)        │             │
     │─────────────────────────────>│             │
     │<─────────────────────────────│             │
     │               │              │             │
     │ write to workspace           │             │
     │────────────────────────────────────────────>
     │               │              │             │
```

## Testing Strategy

1. **Unit tests for BuildScriptInvocation serialization**
2. **Integration test: simple build script** (just prints cfgs)
3. **Integration test: build script with OUT_DIR** (generates code)
4. **Integration test: build script failure handling**
5. **Verify no `Command::new("rustc")` remains in daemon**

## Future Considerations

### Build Script Caching (V2)

- Cache key: hash(toolchain, build.rs, target, profile, version)
- Store: BuildScriptResult + OUT_DIR blobs
- Invalidation: rerun-if-changed directives

### Build Script Dependencies (V2)

- Some build scripts depend on other crates
- Need to compile those dependencies first
- Add `build_deps` to CrateNode

### Sandboxing (V3)

- Run build scripts in isolated sandbox
- Restrict filesystem access
- Enforce hermetic builds

## Critical Files for Implementation

1. **`/Users/amos/bearcove/vertex/crates/vx-exec-proto/src/lib.rs`** - Add BuildScriptInvocation, BuildScriptResult types and execute_build_script trait method

2. **`/Users/amos/bearcove/vertex/crates/vx-execd/src/lib.rs`** - Implement execute_build_script method that compiles and runs build.rs with proper env vars

3. **`/Users/amos/bearcove/vertex/crates/vx-daemon/src/lib.rs`** - Replace Command::new("rustc") with exec.execute_rustc() calls, add build script orchestration

4. **`/Users/amos/bearcove/vertex/crates/vx-rs/src/build_script.rs`** - Reference for build script parsing logic (parse_build_script_output function to move/share with execd)

5. **`/Users/amos/bearcove/vertex/crates/vx-rs/src/crate_graph.rs`** - May need to add pkg_version field to CrateNode for build script env vars
