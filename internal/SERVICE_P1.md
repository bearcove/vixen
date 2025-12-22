# Phase 1: Local-Only Service Split with Rapace IPC

## Goal

Split vertex from a monolithic in-process architecture into 4 separate binaries communicating via rapace IPC over TCP localhost. This establishes the foundation for future distributed execution without implementing it yet.

## Explicit Constraints

- **Localhost only**: All services bind to `127.0.0.1` only
- **No remote execution**: Remote endpoints like `tcp://build-server:9001` are NOT supported in V1
- **Workspace access**: All services operate on the local filesystem (CAS-materialized inputs deferred to V2)
- **Build scripts**: Keep current `build_script.rs` approach (execd build script support deferred to V2)
- **Path handling**: Expand a leading `~` in path args in-process (don’t rely on shell expansion)

## In Scope

✅ Split into 4 binaries: `vx`, `vx-daemon`, `vx-casd`, `vx-execd`
✅ TCP IPC on `127.0.0.1` (ports 9001-9003)
✅ Move `Command::new("rustc")` from daemon → execd (crates/vx-daemon/src/lib.rs:1902, 2113)
✅ Simple auto-spawn (if connect fails, spawn child process; daemon tracks spawned `Child` handles for deps)
✅ Basic shutdown (daemon kills only the children it spawned via `Child::kill()` and exits)

## Out of Scope (Deferred to V2+)

❌ Remote endpoints / distributed execution
❌ Build script execution in execd
❌ Sophisticated file locking / PID files
❌ CAS-materialized workspace inputs
❌ Service crash recovery / orphan cleanup
❌ Graceful shutdown with in-flight tracking

---

## Architecture

### Service Topology

```
┌────────────┐
│     vx     │  (CLI - one invocation per command)
│  :ephemeral│
└─────┬──────┘
      │ rapace
      ↓
┌────────────┐
│ vx-daemon  │  (Build orchestrator)
│  127.0.0.1 │
│  :9001     │
└──┬──────┬──┘
   │      │
   │      └─────────────┐
   ↓ rapace        rapace│
┌──────────┐      ┌─────┴────┐
│ vx-casd  │      │ vx-execd │
│127.0.0.1 │      │127.0.0.1 │
│  :9002   │←─────┤  :9003   │
└──────────┘rapace└──────────┘
```

### Service Responsibilities

| Service | Role | Depends On |
|---------|------|------------|
| `vx` | CLI interface, daemon client | daemon |
| `vx-daemon` | Build orchestration, graph execution | cas, exec |
| `vx-casd` | Content-addressed storage, toolchain acquisition | (none) |
| `vx-execd` | Hermetic command execution (rustc, zig cc) | cas |

---

## Service Discovery

### Endpoint Format

Use simple `host:port` strings (no URL parsing complexity):

| Service | Env Var | Default |
|---------|---------|---------|
| Daemon | `VX_DAEMON` | `127.0.0.1:9001` |
| CAS | `VX_CAS` | `127.0.0.1:9002` |
| Exec | `VX_EXEC` | `127.0.0.1:9003` |

**Binding behavior**: All services bind to `127.0.0.1` (loopback only, not accessible remotely).

---

## Startup Flow

### Lazy Auto-Spawn Pattern

Each service spawns its dependencies on-demand:

1. **User runs `vx build`**
   - Try connect to `127.0.0.1:9001`
   - If fails → spawn `vx-daemon` as child process, track `Child` handle
   - Retry connection with backoff [10, 50, 100, 500, 1000]ms

2. **`vx-daemon` starts**
   - Try connect to `127.0.0.1:9002` (cas)
   - If fails → spawn `vx-casd`, track `Child`
   - Try connect to `127.0.0.1:9003` (exec)
   - If fails → spawn `vx-execd`, track `Child`

3. **`vx-execd` starts**
   - Connect to `127.0.0.1:9002` (cas)
   - If fails → error (cas should already be running from step 2)

**Port conflicts / wrong process on port**:
- If the port is already bound and rapace calls succeed, treat it as “already running” and do not spawn.
- If the port is bound but rapace cannot speak to it (protocol mismatch), surface an error rather than respawning repeatedly.

### Spawn Tracking

```rust
struct SpawnTracker {
    casd: Option<Child>,   // Some if we spawned it
    execd: Option<Child>,  // Some if we spawned it
}
```

Daemon tracks which services it spawned for shutdown purposes.
`vx` does not persist PIDs in V1; it uses RPC (`vx kill`) to ask the daemon to stop.

---

## Shutdown Flow

### Basic Shutdown (V1)

**`vx kill` command:**
1. Connect to daemon
2. Call `daemon.shutdown()` RPC
3. Daemon implementation:
   - If casd/execd were spawned by this daemon process → `child.kill()`
   - If casd/execd were already running (connected successfully) → do not touch them
   - Exit daemon process

**Limitations (accepted for V1):**
- No graceful shutdown (kills immediately)
- No in-flight request tracking
- `vx kill` is best-effort: if the daemon isn’t reachable or won’t respond, report that and exit non-zero
- Remote services (if connected) not shut down
- CAS persists by default (daemon only kills it if it spawned it; otherwise it can be stopped manually)

---

## Implementation Phases

### Phase 1: Add Binary Infrastructure

Create `main.rs` files and `[[bin]]` sections without changing in-process behavior.

#### 1.1 Create `crates/vx-casd/src/main.rs`

```rust
use facet_args::CommandLineTool;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use vx_casd::CasService;

#[derive(CommandLineTool)]
struct Args {
    /// Storage root directory
    #[arg(long, default_value = "~/.vx/cas")]
    root: String,

    /// Bind address (host:port)
    #[arg(long, default_value = "127.0.0.1:9002")]
    bind: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("vx_casd=info")
        .init();

    let args = Args::from_cli();

    // Initialize CAS
    let cas = Arc::new(CasService::new(args.root.into()));
    cas.init()?;

    // Start TCP server
    let addr: SocketAddr = args.bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("CAS listening on {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        let cas = Arc::clone(&cas);

        tokio::spawn(async move {
            // TODO: rapace server setup
            tracing::info!("Accepted connection from {}", peer);
        });
    }
}
```

#### 1.2 Create `crates/vx-execd/src/main.rs`

```rust
use facet_args::CommandLineTool;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use vx_execd::ExecService;

#[derive(CommandLineTool)]
struct Args {
    /// Bind address (host:port)
    #[arg(long, default_value = "127.0.0.1:9003")]
    bind: String,

    /// CAS endpoint (host:port)
    #[arg(long, default_value = "127.0.0.1:9002")]
    cas: String,

    /// Toolchains directory
    #[arg(long, default_value = "~/.vx/toolchains")]
    toolchains_dir: String,

    /// Registry cache directory
    #[arg(long, default_value = "~/.vx/registry")]
    registry_dir: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("vx_execd=info")
        .init();

    let args = Args::from_cli();

    // Connect to CAS
    // TODO: implement CAS client connection

    // Initialize Exec service
    // let exec = Arc::new(ExecService::new(...));

    // Start TCP server
    let addr: SocketAddr = args.bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Exec listening on {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        // let exec = Arc::clone(&exec);

        tokio::spawn(async move {
            // TODO: rapace server setup
            tracing::info!("Accepted connection from {}", peer);
        });
    }
}
```

#### 1.3 Create `crates/vx-daemon/src/main.rs`

```rust
use facet_args::CommandLineTool;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use vx_daemon::DaemonService;

#[derive(CommandLineTool)]
struct Args {
    /// Bind address (host:port)
    #[arg(long, default_value = "127.0.0.1:9001")]
    bind: String,

    /// CAS endpoint (host:port)
    #[arg(long, default_value = "127.0.0.1:9002")]
    cas: String,

    /// Exec endpoint (host:port)
    #[arg(long, default_value = "127.0.0.1:9003")]
    exec: String,

    /// VX home directory
    #[arg(long, default_value = "~/.vx")]
    vx_home: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("vx_daemon=info")
        .init();

    let args = Args::from_cli();

    // Connect to CAS and Exec (with auto-spawn)
    // TODO: implement client connections

    // Initialize Daemon service
    // let daemon = Arc::new(DaemonService::new(...));

    // Start TCP server
    let addr: SocketAddr = args.bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("Daemon listening on {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        // let daemon = Arc::clone(&daemon);

        tokio::spawn(async move {
            // TODO: rapace server setup
            tracing::info!("Accepted connection from {}", peer);
        });
    }
}
```

#### 1.4 Update Cargo.toml Files

**crates/vx-casd/Cargo.toml** - add:
```toml
[[bin]]
name = "vx-casd"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
eyre = { workspace = true }
tokio = { workspace = true, features = ["full"] }
```

**crates/vx-execd/Cargo.toml** - add:
```toml
[[bin]]
name = "vx-execd"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
eyre = { workspace = true }
tokio = { workspace = true, features = ["full"] }
```

**crates/vx-daemon/Cargo.toml** - add:
```toml
[[bin]]
name = "vx-daemon"
path = "src/main.rs"

[dependencies]
# ... existing deps ...
facet-args = { workspace = true }
tracing-subscriber = { workspace = true }
eyre = { workspace = true }
tokio = { workspace = true, features = ["full"] }
```

---

### Phase 2: Wire Up Rapace Servers

The `#[rapace::service]` macro generates both client and server types.

#### 2.1 Understand Generated Types

From `#[rapace::service]` on `pub trait Cas`:
- `CasClient` - client for calling CAS service
- `CasServer<T: Cas>` - server wrapper for service implementation

Similar for `Exec` and `Daemon` traits.

#### 2.2 Server Pattern (All Services)

```rust
// In spawned task per connection
let transport = rapace::Transport::stream(tcp_stream);
let session = rapace::RpcSession::new(transport);
let server = XxxServer::new(service_impl);
server.serve(session).await?;
```

This pattern allows multiple concurrent clients.

#### 2.3 Complete main.rs for Each Service

Fill in the TODO sections from Phase 1.1-1.3 with rapace server setup.

---

### Phase 3: Refactor DaemonService for IPC

Change daemon from owning service instances to owning rapace clients.

#### 3.1 Make DaemonService Generic

**Current:**
```rust
pub struct DaemonService {
    cas: Arc<CasService>,
    exec: ExecService<Arc<CasService>>,
    // ...
}
```

**Target:**
```rust
pub struct DaemonService<C: Cas, E: Exec> {
    cas: C,
    exec: E,
    // ...
}
```

**Type aliases:**
```rust
// For tests and in-process use
type InProcessDaemonService = DaemonService<
    Arc<CasService>,
    ExecService<Arc<CasService>>
>;

// For IPC mode (production)
type IpcDaemonService = DaemonService<CasClient, ExecClient>;
```

#### 3.2 Remove Direct Command::new("rustc") Calls

**File:** `crates/vx-daemon/src/lib.rs`

**Line ~1902** (in `build_rlib()`):
```rust
// BEFORE
let output = Command::new("rustc")
    .args(&invocation.args)
    .current_dir(&invocation.cwd)
    .output()?;

// AFTER
let result = self.exec.execute_rustc(invocation).await;
if result.exit_code != 0 {
    return Err(format!("rustc failed: {}", result.stderr));
}
// Materialize outputs from CAS to workspace
```

**Line ~2113** (in `build_bin_with_deps()`):
Same pattern as above.

---

### Phase 4: Update vx CLI for IPC

Replace in-process daemon instantiation with rapace client connection.

#### 4.1 Daemon Client Connection with Auto-Spawn

**File:** `crates/vx/src/main.rs`

```rust
async fn get_or_spawn_daemon() -> eyre::Result<DaemonClient> {
    let endpoint = std::env::var("VX_DAEMON")
        .unwrap_or_else(|_| "127.0.0.1:9001".to_string());

    // Try to connect
    match try_connect_daemon(&endpoint).await {
        Ok(client) => Ok(client),
        Err(_) => {
            // Spawn daemon binary
            let mut child = Command::new("vx-daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()?;

            // Retry with backoff
            for delay_ms in [10, 50, 100, 500, 1000] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if let Ok(client) = try_connect_daemon(&endpoint).await {
                    // Don't wait for child - daemon runs in background (adopted when vx exits)
                    drop(child);
                    return Ok(client);
                }
            }

            // Failed to connect after spawn
            let _ = child.kill();
            Err(eyre!("daemon failed to start"))
        }
    }
}

async fn try_connect_daemon(endpoint: &str) -> eyre::Result<DaemonClient> {
    let stream = TcpStream::connect(endpoint).await?;
    let transport = rapace::Transport::stream(stream);
    let session = rapace::RpcSession::new(transport);
    Ok(DaemonClient::new(session))
}
```

#### 4.2 Add `vx kill` Command

```rust
// In vx CLI command dispatch
Commands::Kill => {
    match try_connect_daemon(&get_daemon_endpoint()).await {
        Ok(client) => {
            client.shutdown().await?;
            println!("Daemon stopped");
        }
        Err(_) => {
            println!("Daemon not running");
        }
    }
}
```

---

### Phase 5: Complete Daemon Binary

Wire up daemon to connect to CAS/Exec and serve requests.

#### 5.1 Auto-Spawn Dependencies

**File:** `crates/vx-daemon/src/main.rs`

```rust
struct DaemonState {
    daemon: Arc<DaemonService<CasClient, ExecClient>>,
    spawned_cas: Option<Child>,
    spawned_exec: Option<Child>,
}

async fn get_or_spawn_cas(endpoint: &str) -> eyre::Result<(CasClient, Option<Child>)> {
    match try_connect_cas(endpoint).await {
        Ok(client) => Ok((client, None)),
        Err(_) => {
            let mut child = Command::new("vx-casd").spawn()?;

            // Retry connection
            for delay_ms in [10, 50, 100, 500, 1000] {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                if let Ok(client) = try_connect_cas(endpoint).await {
                    return Ok((client, Some(child)));
                }
            }

            let _ = child.kill();
            Err(eyre!("vx-casd failed to start"))
        }
    }
}

// Similar for get_or_spawn_exec
```

#### 5.2 Implement shutdown() RPC

**File:** `crates/vx-daemon-proto/src/lib.rs`

```rust
#[rapace::service]
pub trait Daemon {
    // ... existing methods ...

    /// Shutdown daemon and any spawned services
    async fn shutdown(&self);
}
```

**Implementation in daemon:**
```rust
// Wrap the "pure service" with spawn-tracking state.
struct DaemonImpl {
    service: Arc<DaemonService<CasClient, ExecClient>>,
    spawned: tokio::sync::Mutex<SpawnTracker>,
}

impl Daemon for DaemonImpl {
    async fn shutdown(&self) {
        // Kill only the services we spawned (V1: immediate kill, no graceful drain).
        let mut spawned = self.spawned.lock().await;
        if let Some(mut child) = spawned.execd.take() {
            let _ = child.kill();
        }
        if let Some(mut child) = spawned.casd.take() {
            let _ = child.kill();
        }

        // Exit daemon process
        std::process::exit(0);
    }
}
```

---

## File Changes Summary

### New Files

| Path | Purpose |
|------|---------|
| `crates/vx-casd/src/main.rs` | CAS service binary entrypoint |
| `crates/vx-execd/src/main.rs` | Exec service binary entrypoint |
| `crates/vx-daemon/src/main.rs` | Daemon service binary entrypoint |

### Modified Files

| Path | Changes |
|------|---------|
| `crates/vx-casd/Cargo.toml` | Add `[[bin]]`, tracing deps |
| `crates/vx-execd/Cargo.toml` | Add `[[bin]]`, tracing deps |
| `crates/vx-daemon/Cargo.toml` | Add `[[bin]]`, tracing deps |
| `crates/vx-daemon/src/lib.rs` | Generify over Cas/Exec clients, remove Command::new("rustc") |
| `crates/vx/src/main.rs` | Replace in-process daemon with client + auto-spawn |
| `crates/vx-daemon-proto/src/lib.rs` | Add `shutdown()` to Daemon trait |

---

## Testing Strategy

### Build Tests

1. `cargo build -p vx-casd` → binary at `target/debug/vx-casd`
2. `cargo build -p vx-execd` → binary at `target/debug/vx-execd`
3. `cargo build -p vx-daemon` → binary at `target/debug/vx-daemon`
4. `cargo build -p vx` → binary at `target/debug/vx`

### Smoke Tests

```bash
# Start services manually
./target/debug/vx-casd &
./target/debug/vx-execd &
./target/debug/vx-daemon &

# Run build (should connect to running services)
./target/debug/vx build

# Test auto-spawn (kill all services first)
pkill vx-casd vx-execd vx-daemon
./target/debug/vx build  # Should auto-spawn all services

# Test shutdown
./target/debug/vx kill
ps aux | rg "vx-(casd|execd|daemon)"  # Verify services stopped (ripgrep)

# Verify bind addresses (macOS)
lsof -nP -iTCP:9001 -sTCP:LISTEN
lsof -nP -iTCP:9002 -sTCP:LISTEN
lsof -nP -iTCP:9003 -sTCP:LISTEN
```

### Unit Tests

Existing tests should continue working using `InProcessDaemonService` type alias.

---

## Success Criteria

- [ ] All 4 binaries build successfully
- [ ] `vx build` auto-spawns daemon/cas/exec if not running
- [ ] Build output identical to in-process mode
- [ ] Second `vx build` reuses running services (no respawn)
- [ ] `vx kill` stops daemon and spawned services
- [ ] Services bind to `127.0.0.1` only (verify with `lsof`)
- [ ] Existing unit tests pass with in-process mode

---

## Known Limitations (V1)

1. **No remote execution**: Hardcoded localhost, workspace on local disk
2. **No graceful shutdown**: Services killed immediately, in-flight work lost
3. **No crash recovery**: Orphaned services stay running if daemon crashes
4. **No PID files**: Port binding is the only lock mechanism
5. **Build scripts in daemon**: Still use `build_script.rs` directly (not via exec)

These are acceptable tradeoffs for V1 and will be addressed in future phases.

---

## Next Steps (Post-V1)

- **V2:** Graceful shutdown with in-flight tracking
- **V3:** PID files and lock management
- **V4:** Remote execution support (CAS-materialized inputs)
- **V5:** Build script execution in execd
